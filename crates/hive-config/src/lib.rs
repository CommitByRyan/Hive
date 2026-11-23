use std::env;
use std::fmt::{self, Debug, Display, Formatter};
use std::path::{Component, Path, PathBuf};

use hive_types::HiveError;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_MAX_PARALLEL: usize = 2;

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
        load_with(project_root, |name| env::var(name).ok())
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

    let root = absolute_path(project_root, "project_root")?;
    let data_dir = validated_path(&root, &data_dir_value, "HIVE_DATA_DIR")?;
    let sandbox_dir = validated_path(&root, &sandbox_dir_value, "HIVE_SANDBOX_DIR")?;
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
    })
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

    use super::*;

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
}
