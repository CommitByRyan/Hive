use std::collections::BTreeMap;
use std::env;
use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};

use hive_types::HiveError;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_MAX_PARALLEL: usize = 2;
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

pub const KNOWN_KEYS: [&str; 11] = [
    "HIVE_BASE_URL",
    "HIVE_API_KEY",
    "HIVE_MODEL",
    "HIVE_HOST",
    "HIVE_PORT",
    "HIVE_MAX_PARALLEL",
    "HIVE_ALLOW_SHELL",
    "HIVE_DATA_DIR",
    "HIVE_SANDBOX_DIR",
    "HIVE_RETENTION_DAYS",
    "HIVE_ARCHIVE_DIR",
];

const DEFAULTED_KEYS: [&str; 9] = [
    "HIVE_BASE_URL",
    "HIVE_HOST",
    "HIVE_PORT",
    "HIVE_MAX_PARALLEL",
    "HIVE_ALLOW_SHELL",
    "HIVE_DATA_DIR",
    "HIVE_SANDBOX_DIR",
    "HIVE_RETENTION_DAYS",
    "HIVE_ARCHIVE_DIR",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownEnvKey {
    pub name: String,
    pub line: usize,
}

/// Metadata about an `.env` file without exposing any configured value.
///
/// The API key is deliberately reduced to a boolean: neither `Debug` nor
/// `Display` output can leak secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvInspection {
    pub path: PathBuf,
    pub exists: bool,
    pub recognized_keys: Vec<String>,
    pub missing_defaulted: Vec<&'static str>,
    pub unknown_keys: Vec<UnknownEnvKey>,
    pub api_key_configured: bool,
    pub validation_error: Option<String>,
}

#[derive(Debug, Default)]
struct ParsedEnv {
    values: BTreeMap<String, String>,
    unknown: Vec<UnknownEnvKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Demo,
    Live,
}

impl Display for Mode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Demo => "demo",
            Self::Live => "live",
        })
    }
}

#[derive(Clone)]
pub struct Config {
    base_url: String,
    api_key: String,
    model: String,
    host: String,
    port: u16,
    allow_shell: bool,
    max_parallel: usize,
    data_dir: PathBuf,
    sandbox_dir: PathBuf,
    retention_days: i64,
    archive_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self, HiveError> {
        let project_root = env::current_dir().map_err(|error| HiveError::InvalidConfig {
            field: "project_root",
            reason: error.to_string(),
        })?;
        Self::load_from_env_in(&project_root)
    }

    pub fn load_from_env_in(project_root: &Path) -> Result<Self, HiveError> {
        load_from_env_sources(project_root, &project_root.join(".env"), |name| {
            env::var(name).ok()
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn allow_shell(&self) -> bool {
        self.allow_shell
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn sandbox_dir(&self) -> &Path {
        &self.sandbox_dir
    }

    pub fn retention_days(&self) -> i64 {
        self.retention_days
    }

    pub fn archive_dir(&self) -> &Path {
        &self.archive_dir
    }

    pub fn mode(&self) -> Mode {
        if self.api_key.is_empty() || self.model.is_empty() {
            Mode::Demo
        } else {
            Mode::Live
        }
    }

    pub fn public(&self) -> PublicConfig {
        PublicConfig {
            base_url: self.base_url.clone(),
            api_key_configured: !self.api_key.is_empty(),
            model: self.model.clone(),
            mode: self.mode(),
            host: self.host.clone(),
            port: self.port,
            allow_shell: self.allow_shell,
            max_parallel: self.max_parallel,
            data_dir: self.data_dir.clone(),
            sandbox_dir: self.sandbox_dir.clone(),
        }
    }
}

impl Debug for Config {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("mode", &self.mode())
            .field("host", &self.host)
            .field("port", &self.port)
            .field("allow_shell", &self.allow_shell)
            .field("max_parallel", &self.max_parallel)
            .field("data_dir", &self.data_dir)
            .field("sandbox_dir", &self.sandbox_dir)
            .field("retention_days", &self.retention_days)
            .field("archive_dir", &self.archive_dir)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicConfig {
    pub base_url: String,
    pub api_key_configured: bool,
    pub model: String,
    pub mode: Mode,
    pub host: String,
    pub port: u16,
    pub allow_shell: bool,
    pub max_parallel: usize,
    pub data_dir: PathBuf,
    pub sandbox_dir: PathBuf,
}

impl From<&Config> for PublicConfig {
    fn from(value: &Config) -> Self {
        value.public()
    }
}

impl Display for PublicConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "Mode: {}", self.mode)?;
        writeln!(formatter, "Model: {}", display_optional(&self.model))?;
        writeln!(
            formatter,
            "API Key: {}",
            if self.api_key_configured {
                "configured"
            } else {
                "not configured"
            }
        )?;
        writeln!(formatter, "Base URL: {}", self.base_url)?;
        writeln!(formatter, "Data directory: {}", self.data_dir.display())?;
        writeln!(
            formatter,
            "Sandbox directory: {}",
            self.sandbox_dir.display()
        )?;
        writeln!(formatter, "Listen: {}:{}", self.host, self.port)?;
        writeln!(
            formatter,
            "Shell: {}",
            if self.allow_shell {
                "enabled (approval required)"
            } else {
                "disabled"
            }
        )?;
        write!(formatter, "Max parallel: {}", self.max_parallel)
    }
}

fn load_with<F>(project_root: &Path, read: F) -> Result<Config, HiveError>
where
    F: Fn(&str) -> Option<String>,
{
    let base_url = string_value(&read, "HIVE_BASE_URL", DEFAULT_BASE_URL)?;
    let api_key = read("HIVE_API_KEY").unwrap_or_default().trim().to_owned();
    let model = read("HIVE_MODEL").unwrap_or_default().trim().to_owned();
    let host = string_value(&read, "HIVE_HOST", DEFAULT_HOST)?;
    let port_value = string_value(&read, "HIVE_PORT", "8765")?;
    let max_parallel_value = string_value(&read, "HIVE_MAX_PARALLEL", "2")?;
    let allow_shell = read("HIVE_ALLOW_SHELL").is_some_and(|value| value.trim() == "1");
    let data_dir_value = string_value(&read, "HIVE_DATA_DIR", "data")?;
    let sandbox_dir_value = string_value(&read, "HIVE_SANDBOX_DIR", "sandbox")?;
    let retention_days_value = string_value(&read, "HIVE_RETENTION_DAYS", "30")?;

    let root = absolute_path(project_root, "project_root")?;
    let data_dir = validated_path(&root, &data_dir_value, "HIVE_DATA_DIR")?;
    let sandbox_dir = validated_path(&root, &sandbox_dir_value, "HIVE_SANDBOX_DIR")?;
    let archive_dir = match read("HIVE_ARCHIVE_DIR") {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(invalid_config(
                    "HIVE_ARCHIVE_DIR",
                    "value must not be empty",
                ));
            }
            validated_path(&root, value, "HIVE_ARCHIVE_DIR")?
        }
        None => data_dir.join("archive"),
    };
    if paths_equivalent(&data_dir, &sandbox_dir) {
        return Err(invalid_config(
            "HIVE_DATA_DIR",
            "data and sandbox directories must be different",
        ));
    }

    Ok(Config {
        base_url,
        api_key,
        model,
        host,
        port: parse_port(&port_value)?,
        allow_shell,
        max_parallel: parse_max_parallel(&max_parallel_value)?,
        data_dir,
        sandbox_dir,
        retention_days: parse_retention_days(&retention_days_value)?,
        archive_dir,
    })
}

fn load_from_env_sources<F>(
    project_root: &Path,
    env_path: &Path,
    process: F,
) -> Result<Config, HiveError>
where
    F: Fn(&str) -> Option<String>,
{
    let parsed = parse_env_file(env_path)?;
    if let Some(unknown) = parsed.unknown.first() {
        return Err(unknown_key_error(unknown));
    }
    load_with(project_root, |name| {
        process(name).or_else(|| parsed.values.get(name).cloned())
    })
}

/// Inspect an `.env` file without applying it and without printing values.
pub fn inspect_env_file(project_root: &Path, env_path: &Path) -> Result<EnvInspection, HiveError> {
    inspect_env_file_with(project_root, env_path, |name| env::var(name).ok())
}

fn inspect_env_file_with<F>(
    project_root: &Path,
    env_path: &Path,
    process: F,
) -> Result<EnvInspection, HiveError>
where
    F: Fn(&str) -> Option<String>,
{
    let parsed = parse_env_file(env_path)?;
    let exists = env_path.exists();
    let recognized_keys = KNOWN_KEYS
        .iter()
        .filter(|key| parsed.values.contains_key(**key))
        .map(|key| (*key).to_owned())
        .collect();
    let missing_defaulted = DEFAULTED_KEYS
        .iter()
        .filter(|key| !parsed.values.contains_key(**key) && process(key).is_none())
        .copied()
        .collect();
    let api_key = process("HIVE_API_KEY")
        .or_else(|| parsed.values.get("HIVE_API_KEY").cloned())
        .unwrap_or_default();
    let api_key_configured = !api_key.trim().is_empty();
    let validation_error = if parsed.unknown.is_empty() {
        match load_with(project_root, |name| {
            process(name).or_else(|| parsed.values.get(name).cloned())
        }) {
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        }
    } else {
        None
    };

    Ok(EnvInspection {
        path: env_path.to_path_buf(),
        exists,
        recognized_keys,
        missing_defaulted,
        unknown_keys: parsed.unknown,
        api_key_configured,
        validation_error,
    })
}

fn parse_env_file(path: &Path) -> Result<ParsedEnv, HiveError> {
    if !path.exists() {
        return Ok(ParsedEnv::default());
    }
    let bytes = fs::read(path)
        .map_err(|error| env_file_error(format!("failed to read {}: {error}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| env_file_error(format!("{} is not valid UTF-8", path.display())))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut parsed = ParsedEnv::default();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export") {
            if rest.chars().next().is_some_and(char::is_whitespace) {
                line = rest.trim();
            }
        }
        let Some(equals) = line.find('=') else {
            let key = line.trim();
            if key.starts_with("HIVE_") {
                parsed.unknown.push(UnknownEnvKey {
                    name: key.to_owned(),
                    line: line_number,
                });
            }
            continue;
        };
        let key = line[..equals].trim();
        if !key.starts_with("HIVE_") {
            continue;
        }
        if !KNOWN_KEYS.contains(&key) {
            parsed.unknown.push(UnknownEnvKey {
                name: key.to_owned(),
                line: line_number,
            });
            continue;
        }
        let value = parse_env_value(key, line[equals + 1..].trim(), line_number)?;
        parsed.values.insert(key.to_owned(), value);
    }

    Ok(parsed)
}

fn parse_env_value(key: &str, raw: &str, line: usize) -> Result<String, HiveError> {
    if let Some(inner) = raw.strip_prefix('"') {
        let Some(inner) = inner.strip_suffix('"') else {
            return Err(env_line_error(
                line,
                key,
                "unterminated double-quoted value",
            ));
        };
        return unescape_double_quoted(inner).map_err(|reason| env_line_error(line, key, &reason));
    }
    if let Some(inner) = raw.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            return Err(env_line_error(
                line,
                key,
                "unterminated single-quoted value",
            ));
        };
        return Ok(inner.to_owned());
    }
    Ok(raw.to_owned())
}

fn unescape_double_quoted(value: &str) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(escaped) = characters.next() else {
            return Err("trailing backslash in double-quoted value".to_owned());
        };
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '"' => output.push('"'),
            '\\' => output.push('\\'),
            other => {
                output.push('\\');
                output.push(other);
            }
        }
    }
    Ok(output)
}

fn unknown_key_error(unknown: &UnknownEnvKey) -> HiveError {
    HiveError::InvalidConfig {
        field: "HIVE_ENV_UNKNOWN",
        reason: format!("unknown key {} at line {}", unknown.name, unknown.line),
    }
}

fn env_line_error(line: usize, key: &str, reason: &str) -> HiveError {
    env_file_error(format!("line {line}: {key}: {reason}"))
}

fn env_file_error(reason: String) -> HiveError {
    HiveError::InvalidConfig {
        field: "HIVE_ENV_FILE",
        reason,
    }
}

fn string_value<F>(read: &F, name: &'static str, default: &str) -> Result<String, HiveError>
where
    F: Fn(&str) -> Option<String>,
{
    let value = match read(name) {
        Some(value) => value.trim().to_owned(),
        None => default.to_owned(),
    };
    if value.is_empty() {
        return Err(invalid_config(name, "value must not be empty"));
    }
    Ok(value)
}

fn parse_port(value: &str) -> Result<u16, HiveError> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid_config("HIVE_PORT", "value must be an integer from 1 to 65535"))
}

fn parse_max_parallel(value: &str) -> Result<usize, HiveError> {
    value
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| invalid_config("HIVE_MAX_PARALLEL", "value must be a positive integer"))
}

fn parse_retention_days(value: &str) -> Result<i64, HiveError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|days| *days >= 0)
        .ok_or_else(|| {
            invalid_config(
                "HIVE_RETENTION_DAYS",
                "value must be a non-negative integer",
            )
        })
}

fn validated_path(root: &Path, value: &str, field: &'static str) -> Result<PathBuf, HiveError> {
    let joined = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        root.join(value)
    };
    let absolute = absolute_path(&joined, field)?;
    if is_filesystem_root(&absolute) {
        return Err(invalid_config(
            field,
            "filesystem root is not a valid directory",
        ));
    }
    Ok(lexical_normalize(&absolute))
}

fn absolute_path(path: &Path, field: &'static str) -> Result<PathBuf, HiveError> {
    std::path::absolute(path).map_err(|error| HiveError::InvalidConfig {
        field,
        reason: error.to_string(),
    })
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    } else {
        left == right
    }
}

fn display_optional(value: &str) -> &str {
    if value.is_empty() {
        "(not configured)"
    } else {
        value
    }
}

fn invalid_config(field: &'static str, reason: &str) -> HiveError {
    HiveError::InvalidConfig {
        field,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-config-{name}-{}-{nanos}-{counter}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn load(values: &[(&str, &str)]) -> Result<Config, HiveError> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        load_with(Path::new("project"), |name| values.get(name).cloned())
    }

    #[test]
    fn defaults_match_build_spec() {
        let config = load(&[]).unwrap();
        assert_eq!(config.base_url(), DEFAULT_BASE_URL);
        assert_eq!(config.model(), "");
        assert_eq!(config.mode(), Mode::Demo);
        assert_eq!(config.host(), DEFAULT_HOST);
        assert_eq!(config.port(), DEFAULT_PORT);
        assert!(!config.allow_shell());
        assert_eq!(config.max_parallel(), DEFAULT_MAX_PARALLEL);
        assert!(config
            .data_dir()
            .ends_with(Path::new("project").join("data")));
        assert!(config
            .sandbox_dir()
            .ends_with(Path::new("project").join("sandbox")));
        assert_eq!(config.retention_days(), DEFAULT_RETENTION_DAYS);
        assert_eq!(config.archive_dir(), config.data_dir().join("archive"));
    }

    #[test]
    fn live_mode_requires_api_key_and_model() {
        let key_only = load(&[("HIVE_API_KEY", "secret")]).unwrap();
        let model_only = load(&[("HIVE_MODEL", "model-a")]).unwrap();
        let live = load(&[("HIVE_API_KEY", "secret"), ("HIVE_MODEL", "model-a")]).unwrap();

        assert_eq!(key_only.mode(), Mode::Demo);
        assert_eq!(model_only.mode(), Mode::Demo);
        assert_eq!(live.mode(), Mode::Live);
    }

    #[test]
    fn invalid_port_and_parallel_values_are_rejected() {
        for value in ["", "0", "65536", "abc"] {
            let error = load(&[("HIVE_PORT", value)]).unwrap_err();
            assert!(matches!(
                error,
                HiveError::InvalidConfig {
                    field: "HIVE_PORT",
                    ..
                }
            ));
        }
        for value in ["", "0", "-1", "abc"] {
            let error = load(&[("HIVE_MAX_PARALLEL", value)]).unwrap_err();
            assert!(matches!(
                error,
                HiveError::InvalidConfig {
                    field: "HIVE_MAX_PARALLEL",
                    ..
                }
            ));
        }
    }

    #[test]
    fn invalid_paths_are_rejected() {
        let empty = load(&[("HIVE_DATA_DIR", " ")]).unwrap_err();
        assert!(matches!(
            empty,
            HiveError::InvalidConfig {
                field: "HIVE_DATA_DIR",
                ..
            }
        ));

        let same =
            load(&[("HIVE_DATA_DIR", "shared"), ("HIVE_SANDBOX_DIR", "shared")]).unwrap_err();
        assert!(matches!(
            same,
            HiveError::InvalidConfig {
                field: "HIVE_DATA_DIR",
                ..
            }
        ));
    }

    #[test]
    fn retention_days_must_be_non_negative() {
        assert_eq!(
            load(&[("HIVE_RETENTION_DAYS", "0")])
                .unwrap()
                .retention_days(),
            0
        );
        assert_eq!(
            load(&[("HIVE_RETENTION_DAYS", "7")])
                .unwrap()
                .retention_days(),
            7
        );
        for value in ["", "-1", "abc", "1.5"] {
            let error = load(&[("HIVE_RETENTION_DAYS", value)]).unwrap_err();
            assert!(matches!(
                error,
                HiveError::InvalidConfig {
                    field: "HIVE_RETENTION_DAYS",
                    ..
                }
            ));
        }
    }

    #[test]
    fn archive_dir_is_resolved_against_project_root() {
        let config = load(&[("HIVE_ARCHIVE_DIR", "custom/archive")]).unwrap();
        assert!(config
            .archive_dir()
            .ends_with(Path::new("project").join("custom").join("archive")));

        let empty = load(&[("HIVE_ARCHIVE_DIR", " ")]).unwrap_err();
        assert!(matches!(
            empty,
            HiveError::InvalidConfig {
                field: "HIVE_ARCHIVE_DIR",
                ..
            }
        ));
    }

    #[test]
    fn archive_dir_defaults_under_custom_data_dir() {
        let custom = load(&[("HIVE_DATA_DIR", "custom-data")]).unwrap();
        assert_eq!(custom.archive_dir(), custom.data_dir().join("archive"));
        assert!(custom
            .archive_dir()
            .ends_with(Path::new("project").join("custom-data").join("archive")));

        let explicit = load(&[
            ("HIVE_DATA_DIR", "custom-data"),
            ("HIVE_ARCHIVE_DIR", "explicit-archive"),
        ])
        .unwrap();
        assert_ne!(explicit.archive_dir(), explicit.data_dir().join("archive"));
        assert!(explicit
            .archive_dir()
            .ends_with(Path::new("project").join("explicit-archive")));
    }

    #[test]
    fn shell_is_enabled_only_by_exact_one() {
        assert!(load(&[("HIVE_ALLOW_SHELL", "1")]).unwrap().allow_shell());
        assert!(!load(&[("HIVE_ALLOW_SHELL", "true")]).unwrap().allow_shell());
        assert!(!load(&[("HIVE_ALLOW_SHELL", "0")]).unwrap().allow_shell());
    }

    #[test]
    fn public_config_never_contains_api_key() {
        let secret = "top-secret-key";
        let config = load(&[("HIVE_API_KEY", secret), ("HIVE_MODEL", "model-a")]).unwrap();
        let public = config.public();

        assert!(public.api_key_configured);
        assert!(!format!("{:?}", config).contains(secret));
        assert!(!format!("{:?}", public).contains(secret));
        assert!(!public.to_string().contains(secret));
    }

    #[test]
    fn dotenv_supports_bom_comments_export_and_quotes() {
        let dir = TempDir::new("dotenv-quotes");
        let path = dir.path().join(".env");
        fs::write(
            &path,
            "\u{feff}# heading\n\nexport HIVE_MODEL=\"model-a\"\nHIVE_HOST='127.0.0.2'\nHIVE_API_KEY=\"sec\\\"ret\"\nHIVE_DATA_DIR=data\nHIVE_SANDBOX_DIR=sandbox\n",
        )
        .expect("write env");

        let config = load_from_env_sources(dir.path(), &path, |_| None).expect("load from dotenv");
        assert_eq!(config.model(), "model-a");
        assert_eq!(config.host(), "127.0.0.2");
        assert_eq!(config.api_key(), "sec\"ret");
        assert_eq!(config.mode(), Mode::Live);
    }

    #[test]
    fn process_environment_overrides_dotenv() {
        let dir = TempDir::new("dotenv-precedence");
        let path = dir.path().join(".env");
        fs::write(&path, "HIVE_PORT=1111\nHIVE_HOST=file-host\n").expect("write env");
        let process: HashMap<String, String> = [("HIVE_PORT".to_owned(), "2222".to_owned())]
            .into_iter()
            .collect();

        let config = load_from_env_sources(dir.path(), &path, |name| process.get(name).cloned())
            .expect("load");
        assert_eq!(config.port(), 2222);
        assert_eq!(config.host(), "file-host");
    }

    #[test]
    fn unknown_dotenv_keys_include_name_and_line() {
        let dir = TempDir::new("dotenv-unknown");
        let path = dir.path().join(".env");
        fs::write(&path, "# heading\nHIVE_PORT=8765\nHIVE_NOPE=1\n").expect("write env");

        let error = load_from_env_sources(dir.path(), &path, |_| None).expect_err("unknown key");
        let message = error.to_string();
        assert!(message.contains("HIVE_NOPE"));
        assert!(message.contains("line 3"));
    }

    #[test]
    fn missing_dotenv_file_keeps_defaults() {
        let dir = TempDir::new("dotenv-missing");
        let config = load_from_env_sources(dir.path(), &dir.path().join("missing.env"), |_| None)
            .expect("missing file is allowed");

        assert_eq!(config.port(), DEFAULT_PORT);
        assert_eq!(config.host(), DEFAULT_HOST);
        assert_eq!(config.model(), "");
    }

    #[test]
    fn inspection_reports_keys_without_exposing_values() {
        let dir = TempDir::new("dotenv-inspect");
        let path = dir.path().join(".env");
        fs::write(
            &path,
            "HIVE_API_KEY=inspection-secret\nHIVE_PORT=abc\nHIVE_UNKNOWN=1\n",
        )
        .expect("write env");

        let inspection = inspect_env_file_with(dir.path(), &path, |_| None).expect("inspect");
        assert!(inspection.exists);
        assert_eq!(
            inspection.recognized_keys,
            vec!["HIVE_API_KEY".to_owned(), "HIVE_PORT".to_owned()]
        );
        assert_eq!(inspection.unknown_keys.len(), 1);
        assert_eq!(inspection.unknown_keys[0].name, "HIVE_UNKNOWN");
        assert_eq!(inspection.unknown_keys[0].line, 3);
        assert!(inspection.api_key_configured);
        assert!(inspection.validation_error.is_none());
        assert!(inspection.missing_defaulted.contains(&"HIVE_HOST"));
        assert!(!inspection.missing_defaulted.contains(&"HIVE_API_KEY"));
        assert!(!format!("{inspection:?}").contains("inspection-secret"));
    }

    #[test]
    fn inspection_reports_invalid_known_values() {
        let dir = TempDir::new("dotenv-invalid");
        let path = dir.path().join(".env");
        fs::write(&path, "HIVE_PORT=abc\n").expect("write env");

        let inspection = inspect_env_file_with(dir.path(), &path, |_| None).expect("inspect");
        let validation_error = inspection.validation_error.expect("invalid port");
        assert!(validation_error.contains("HIVE_PORT"));
    }

    #[test]
    fn dotenv_api_key_is_redacted_from_debug_and_display() {
        let dir = TempDir::new("dotenv-secret");
        let path = dir.path().join(".env");
        let secret = "dotenv-top-secret";
        fs::write(
            &path,
            format!("HIVE_API_KEY={secret}\nHIVE_MODEL=model-a\n"),
        )
        .expect("write env");

        let config = load_from_env_sources(dir.path(), &path, |_| None).expect("load from dotenv");
        assert!(!format!("{config:?}").contains(secret));
        assert!(!config.public().to_string().contains(secret));
    }
}
