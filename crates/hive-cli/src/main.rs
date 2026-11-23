use std::env;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::process::{Command, ExitCode};

use hive_config::{Config, PublicConfig};
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
struct DoctorReport {
    public_config: PublicConfig,
    tools: [ToolReport; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Version,
    Doctor,
    Help,
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

fn run() -> Result<u8, HiveError> {
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
    }
}

fn parse_action(arguments: &[String]) -> Result<Action, HiveError> {
    match arguments {
        [argument] if argument == "--version" => Ok(Action::Version),
        [argument] if argument == "doctor" => Ok(Action::Doctor),
        [argument] if matches!(argument.as_str(), "--help" | "-h" | "help") => Ok(Action::Help),
        [] => Err(HiveError::UnsupportedCommand {
            command: "(none)".to_owned(),
        }),
        other => Err(HiveError::UnsupportedCommand {
            command: other.join(" "),
        }),
    }
}

fn render_version() -> String {
    format!("Hive {}", VERSION)
}

fn render_help() -> String {
    "Usage:\n  hive --version\n  hive doctor\n".to_owned()
}

fn build_doctor_report(public_config: &PublicConfig) -> DoctorReport {
    DoctorReport {
        public_config: public_config.clone(),
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
    for tool in &report.tools {
        output.push_str(&format!("{}: {}\n", tool.name, tool.status));
    }
    output
}

fn doctor_exit_code(report: &DoctorReport) -> u8 {
    if report
        .tools
        .iter()
        .all(|tool| matches!(tool.status, ToolStatus::Available(_)))
    {
        0
    } else {
        1
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
        assert!(parse_action(&strings(&["serve"])).is_err());
    }

    #[test]
    fn version_output_is_stable() {
        assert_eq!(render_version(), format!("Hive {}", VERSION));
    }

    #[test]
    fn doctor_output_reports_requested_fields_without_secret() {
        let report = DoctorReport {
            public_config: public_config(),
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
        assert!(output.contains("Listen: 127.0.0.1:8765"));
        assert!(output.contains("Shell: disabled"));
        assert!(output.contains("rustc: available (rustc 1.80.0)"));
        assert!(output.contains("cargo: missing from PATH"));
        assert!(!output.contains("top-secret"));
        assert_eq!(doctor_exit_code(&report), 1);
    }
}
