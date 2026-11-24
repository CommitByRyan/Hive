use std::env;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use hive_config::{Config, EnvInspection, PublicConfig};
use hive_store::MigrationDryRunReport;
use hive_types::HiveError;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolStatus {
    Available(String),
    Missing,
    ProbeFailed(String),
}

impl Display for ToolStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available(version) => write!(formatter, "available ({})", version),
            Self::Missing => formatter.write_str("missing from PATH"),
            Self::ProbeFailed(reason) => write!(formatter, "probe failed ({})", reason),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolReport {
    name: &'static str,
    status: ToolStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DirectoryStatus {
    Writable,
    Unwritable(String),
}

impl Display for DirectoryStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Writable => formatter.write_str("writable"),
            Self::Unwritable(reason) => write!(formatter, "not writable ({reason})"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryReport {
    name: &'static str,
    status: DirectoryStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DoctorReport {
    public_config: PublicConfig,
    directories: [DirectoryReport; 2],
    tools: [ToolReport; 2],
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Version,
    Doctor,
    ConfigMigrate { file: Option<PathBuf> },
    DbMigrate { db: Option<PathBuf> },
    Help,
}

#[derive(Debug)]
enum CliError {
    Hive(HiveError),
    Store(hive_store::StoreError),
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hive(error) => Display::fmt(error, formatter),
            Self::Store(error) => Display::fmt(error, formatter),
        }
    }
}

impl CliError {
    const fn code(&self) -> &'static str {
        match self {
            Self::Hive(error) => error.code(),
            Self::Store(error) => error.code(),
        }
    }
}

impl From<HiveError> for CliError {
    fn from(error: HiveError) -> Self {
        Self::Hive(error)
    }
}

impl From<hive_store::StoreError> for CliError {
    fn from(error: hive_store::StoreError) -> Self {
        Self::Store(error)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error[{}]: {}", error.code(), error);
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, CliError> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match parse_action(&arguments)? {
        Action::Version => {
            println!("{}", render_version());
            Ok(0)
        }
        Action::Help => {
            print!("{}", render_help());
            Ok(0)
        }
        Action::Doctor => {
            let config = Config::load()?;
            let report = build_doctor_report(&config.public());
            print!("{}", render_doctor_report(&report));
            Ok(doctor_exit_code(&report))
        }
        Action::ConfigMigrate { file } => run_config_migrate(file),
        Action::DbMigrate { db } => run_db_migrate(db),
    }
}

fn parse_action(arguments: &[String]) -> Result<Action, HiveError> {
    match arguments {
        [argument] if argument == "--version" => Ok(Action::Version),
        [argument] if argument == "doctor" => Ok(Action::Doctor),
        [first, second, rest @ ..]
            if first.as_str() == "config" && second.as_str() == "migrate" =>
        {
            Ok(Action::ConfigMigrate {
                file: parse_migrate_args("config migrate", rest, "--file")?,
            })
        }
        [first, second, rest @ ..] if first.as_str() == "db" && second.as_str() == "migrate" => {
            Ok(Action::DbMigrate {
                db: parse_migrate_args("db migrate", rest, "--db")?,
            })
        }
        [argument] if matches!(argument.as_str(), "--help" | "-h" | "help") => Ok(Action::Help),
        [] => Err(HiveError::UnsupportedCommand {
            command: "(none)".to_owned(),
        }),
        other => Err(HiveError::UnsupportedCommand {
            command: other.join(" "),
        }),
    }
}

fn parse_migrate_args(
    command: &str,
    arguments: &[String],
    value_flag: &str,
) -> Result<Option<PathBuf>, HiveError> {
    let mut dry_run = false;
    let mut value = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--dry-run" => {
                if dry_run {
                    return Err(command_error(command, "duplicate --dry-run"));
                }
                dry_run = true;
                index += 1;
            }
            flag if flag == value_flag => {
                if value.is_some() {
                    return Err(command_error(command, &format!("duplicate {value_flag}")));
                }
                let Some(next) = arguments.get(index + 1) else {
                    return Err(command_error(
                        command,
                        &format!("{value_flag} requires a path"),
                    ));
                };
                value = Some(PathBuf::from(next));
                index += 2;
            }
            other => {
                return Err(command_error(
                    command,
                    &format!("unsupported argument `{other}`"),
                ))
            }
        }
    }
    if !dry_run {
        return Err(command_error(
            command,
            "only --dry-run is supported in this release",
        ));
    }
    Ok(value)
}

fn command_error(command: &str, reason: &str) -> HiveError {
    HiveError::UnsupportedCommand {
        command: format!("{command}: {reason}"),
    }
}

fn render_version() -> String {
    format!("Hive {}", VERSION)
}

fn render_help() -> String {
    "Usage:\n  hive --version\n  hive doctor\n  hive config migrate --dry-run [--file <path>]\n  hive db migrate --dry-run [--db <path>]\n".to_owned()
}

fn build_doctor_report(public_config: &PublicConfig) -> DoctorReport {
    DoctorReport {
        public_config: public_config.clone(),
        directories: [
            DirectoryReport {
                name: "data",
                status: probe_directory(&public_config.data_dir),
            },
            DirectoryReport {
                name: "sandbox",
                status: probe_directory(&public_config.sandbox_dir),
            },
        ],
        tools: [
            ToolReport {
                name: "rustc",
                status: probe_tool("rustc"),
            },
            ToolReport {
                name: "cargo",
                status: probe_tool("cargo"),
            },
        ],
    }
}

fn probe_directory(path: &Path) -> DirectoryStatus {
    if let Err(error) = fs::create_dir_all(path) {
        return DirectoryStatus::Unwritable(error.to_string());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let probe = path.join(format!(".hive-doctor-{}-{nanos}", std::process::id()));
    match fs::write(&probe, b"hive doctor write probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            DirectoryStatus::Writable
        }
        Err(error) => DirectoryStatus::Unwritable(error.to_string()),
    }
}

fn probe_tool(name: &'static str) -> ToolStatus {
    match Command::new(name).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let version = stdout
                .lines()
                .chain(stderr.lines())
                .find(|line| !line.trim().is_empty())
                .unwrap_or("version unknown")
                .trim()
                .to_owned();
            ToolStatus::Available(version)
        }
        Ok(output) => ToolStatus::ProbeFailed(format!(
            "exit code {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_owned())
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => ToolStatus::Missing,
        Err(error) => ToolStatus::ProbeFailed(error.to_string()),
    }
}

fn render_doctor_report(report: &DoctorReport) -> String {
    let mut output = format!("Hive {}\n{}\n", VERSION, report.public_config);
    for directory in &report.directories {
        output.push_str(&format!(
            "{} directory check: {}\n",
            title_case(directory.name),
            directory.status
        ));
    }
    for tool in &report.tools {
        output.push_str(&format!("{}: {} [optional]\n", tool.name, tool.status));
    }
    output
}

fn doctor_exit_code(report: &DoctorReport) -> u8 {
    if report
        .directories
        .iter()
        .all(|directory| matches!(directory.status, DirectoryStatus::Writable))
    {
        0
    } else {
        1
    }
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn run_config_migrate(file: Option<PathBuf>) -> Result<u8, CliError> {
    let project_root = env::current_dir().map_err(|error| HiveError::InvalidConfig {
        field: "project_root",
        reason: error.to_string(),
    })?;
    let env_path = match file {
        Some(path) => absolute_path(&path)?,
        None => project_root.join(".env"),
    };
    let inspection = hive_config::inspect_env_file(&project_root, &env_path)?;
    print!("{}", render_env_inspection(&inspection));
    if !inspection.exists {
        return Ok(0);
    }
    if inspection.unknown_keys.is_empty() && inspection.validation_error.is_none() {
        Ok(0)
    } else {
        Ok(1)
    }
}

fn run_db_migrate(db: Option<PathBuf>) -> Result<u8, CliError> {
    let db_path = match db {
        Some(path) => absolute_path(&path)?,
        None => {
            let config = Config::load()?;
            config.data_dir().join("hive.db")
        }
    };
    let report = hive_store::dry_run_migration(&db_path)?;
    print!("{}", render_migration_dry_run(&report));
    if report.failure.is_some() {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, HiveError> {
    std::path::absolute(path).map_err(|error| HiveError::InvalidConfig {
        field: "path",
        reason: error.to_string(),
    })
}

fn render_env_inspection(inspection: &EnvInspection) -> String {
    let mut output = String::from("Hive config migration dry-run\n");
    output.push_str(&format!("File: {}\n", inspection.path.display()));
    output.push_str(&format!(
        "Status: {}\n",
        if inspection.exists {
            "found"
        } else {
            "missing"
        }
    ));
    output.push_str(&format!(
        "Recognized keys: {}\n",
        inspection.recognized_keys.len()
    ));
    output.push_str(&format!(
        "Missing defaults: {}\n",
        join_or_none(&inspection.missing_defaulted)
    ));
    let unknown = inspection
        .unknown_keys
        .iter()
        .map(|key| format!("{} (line {})", key.name, key.line))
        .collect::<Vec<_>>();
    output.push_str(&format!("Unknown keys: {}\n", join_or_none(&unknown)));
    output.push_str(&format!(
        "API Key: {}\n",
        if inspection.api_key_configured {
            "set"
        } else {
            "unset"
        }
    ));
    if !inspection.unknown_keys.is_empty() {
        output.push_str("Validation: skipped (unknown keys)\n");
        output.push_str("Result: blocked by unknown keys; no files changed\n");
    } else if let Some(error) = &inspection.validation_error {
        output.push_str(&format!("Validation: error: {error}\n"));
        output.push_str("Result: invalid configuration; no files changed\n");
    } else if !inspection.exists {
        output.push_str("Validation: skipped (file missing)\n");
        output.push_str("Result: no .env file; nothing to migrate\n");
    } else {
        output.push_str("Validation: ok\n");
        output.push_str("Result: configuration is valid; no files changed\n");
    }
    output
}

fn render_migration_dry_run(report: &MigrationDryRunReport) -> String {
    let mut output = String::from("Hive database migration dry-run\n");
    output.push_str(&format!("Source: {}\n", report.source.display()));
    output.push_str(&format!(
        "Status: {}\n",
        if report.exists { "present" } else { "missing" }
    ));
    output.push_str(&format!("Current version: {}\n", report.current_version));
    output.push_str(&format!("Target version: {}\n", report.target_version));
    let pending = report
        .pending_versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>();
    output.push_str(&format!("Pending migrations: {}\n", join_or_none(&pending)));
    output.push_str(&format!("Final version: {}\n", report.final_version));
    if let Some(copy) = &report.copy_path {
        let state = if report.copy_retained {
            "retained"
        } else if report.failure.is_some() {
            "not created"
        } else {
            "removed"
        };
        output.push_str(&format!("Copy: {} ({state})\n", copy.display()));
    } else {
        output.push_str("Copy: none\n");
    }
    if let Some(warning) = &report.warning {
        output.push_str(&format!("Warning: {warning}\n"));
    }
    match &report.failure {
        Some(failure) => output.push_str(&format!("Result: failed: {failure}\n")),
        None if !report.exists => output.push_str(&format!(
            "Result: fresh start would create V{}; no files changed\n",
            report.target_version
        )),
        None if report.current_version == 0 && report.warning.is_some() => {
            output.push_str(&format!(
                "Result: empty database would be initialized to V{}; no files changed\n",
                report.target_version
            ))
        }
        None if report.pending_versions.is_empty() => output.push_str(&format!(
            "Result: already at V{}; no migration needed\n",
            report.target_version
        )),
        None => output.push_str("Result: dry-run only; original database unchanged\n"),
    }
    output
}

fn join_or_none<T: ToString>(values: &[T]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use hive_config::Mode;
    use std::path::PathBuf;

    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn public_config() -> PublicConfig {
        PublicConfig {
            base_url: "http://127.0.0.1:3425/v1".to_owned(),
            api_key_configured: true,
            model: "workbuddy/hy4flash-free".to_owned(),
            mode: Mode::Live,
            host: "127.0.0.1".to_owned(),
            port: 8765,
            allow_shell: false,
            max_parallel: 2,
            data_dir: PathBuf::from("project/data"),
            sandbox_dir: PathBuf::from("project/sandbox"),
        }
    }

    #[test]
    fn parses_only_supported_commands() {
        assert_eq!(
            parse_action(&strings(&["--version"])).unwrap(),
            Action::Version
        );
        assert_eq!(parse_action(&strings(&["doctor"])).unwrap(), Action::Doctor);
        assert_eq!(parse_action(&strings(&["--help"])).unwrap(), Action::Help);
        assert_eq!(
            parse_action(&strings(&["config", "migrate", "--dry-run"])).unwrap(),
            Action::ConfigMigrate { file: None }
        );
        assert_eq!(
            parse_action(&strings(&[
                "config",
                "migrate",
                "--dry-run",
                "--file",
                "custom.env"
            ]))
            .unwrap(),
            Action::ConfigMigrate {
                file: Some(PathBuf::from("custom.env"))
            }
        );
        assert_eq!(
            parse_action(&strings(&[
                "db",
                "migrate",
                "--dry-run",
                "--db",
                "custom.db"
            ]))
            .unwrap(),
            Action::DbMigrate {
                db: Some(PathBuf::from("custom.db"))
            }
        );
        assert!(parse_action(&strings(&["serve"])).is_err());
        assert!(parse_action(&strings(&["config", "migrate"])).is_err());
        assert!(parse_action(&strings(&["db", "migrate", "--dry-run", "--bad"])).is_err());
    }

    #[test]
    fn version_output_is_stable() {
        assert_eq!(render_version(), format!("Hive {}", VERSION));
    }

    #[test]
    fn doctor_output_reports_requested_fields_without_secret() {
        let report = DoctorReport {
            public_config: public_config(),
            directories: [
                DirectoryReport {
                    name: "data",
                    status: DirectoryStatus::Writable,
                },
                DirectoryReport {
                    name: "sandbox",
                    status: DirectoryStatus::Writable,
                },
            ],
            tools: [
                ToolReport {
                    name: "rustc",
                    status: ToolStatus::Available("rustc 1.80.0".to_owned()),
                },
                ToolReport {
                    name: "cargo",
                    status: ToolStatus::Missing,
                },
            ],
        };
        let output = render_doctor_report(&report);

        assert!(output.contains("Hive "));
        assert!(output.contains("Mode: live"));
        assert!(output.contains("Model: workbuddy/hy4flash-free"));
        assert!(output.contains("Data directory: project/data"));
        assert!(output.contains("Sandbox directory: project/sandbox"));
        assert!(output.contains("Data directory check: writable"));
        assert!(output.contains("Sandbox directory check: writable"));
        assert!(output.contains("Listen: 127.0.0.1:8765"));
        assert!(output.contains("Shell: disabled"));
        assert!(output.contains("rustc: available (rustc 1.80.0) [optional]"));
        assert!(output.contains("cargo: missing from PATH [optional]"));
        assert!(!output.contains("top-secret"));
        assert_eq!(doctor_exit_code(&report), 0);
    }

    #[test]
    fn doctor_fails_only_when_a_directory_is_not_writable() {
        let report = DoctorReport {
            public_config: public_config(),
            directories: [
                DirectoryReport {
                    name: "data",
                    status: DirectoryStatus::Writable,
                },
                DirectoryReport {
                    name: "sandbox",
                    status: DirectoryStatus::Unwritable("access denied".to_owned()),
                },
            ],
            tools: [
                ToolReport {
                    name: "rustc",
                    status: ToolStatus::Available("rustc 1.80.0".to_owned()),
                },
                ToolReport {
                    name: "cargo",
                    status: ToolStatus::Available("cargo 1.80.0".to_owned()),
                },
            ],
        };

        assert_eq!(doctor_exit_code(&report), 1);
    }

    #[test]
    fn env_inspection_output_uses_stable_fields() {
        let inspection = EnvInspection {
            path: PathBuf::from("project/.env"),
            exists: true,
            recognized_keys: vec!["HIVE_API_KEY".to_owned()],
            missing_defaulted: vec!["HIVE_HOST", "HIVE_PORT"],
            unknown_keys: vec![],
            api_key_configured: true,
            validation_error: None,
        };
        let output = render_env_inspection(&inspection);

        assert!(output.contains("Status: found"));
        assert!(output.contains("Recognized keys: 1"));
        assert!(output.contains("Missing defaults: HIVE_HOST, HIVE_PORT"));
        assert!(output.contains("Unknown keys: none"));
        assert!(output.contains("API Key: set"));
        assert!(output.contains("Validation: ok"));
        assert!(output.contains("Result: configuration is valid; no files changed"));
    }

    #[test]
    fn migration_report_is_human_readable() {
        let report = MigrationDryRunReport {
            source: PathBuf::from("data/hive.db"),
            exists: true,
            current_version: 5,
            target_version: 6,
            pending_versions: vec![6],
            final_version: 6,
            copy_path: Some(PathBuf::from("data/hive.db.dry-run")),
            copy_retained: false,
            failure: None,
            warning: None,
        };
        let output = render_migration_dry_run(&report);

        assert!(output.contains("Source: data/hive.db"));
        assert!(output.contains("Current version: 5"));
        assert!(output.contains("Pending migrations: 6"));
        assert!(output.contains("Final version: 6"));
        assert!(output.contains("Copy: data/hive.db.dry-run (removed)"));
        assert!(output.contains("original database unchanged"));
    }
}
