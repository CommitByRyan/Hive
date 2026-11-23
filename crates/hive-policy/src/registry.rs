use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::PolicyError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskLevel {
    ReadOnly,
    Write,
    Execute,
    Network,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalMode {
    Never,
    Policy,
    Always,
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub version: u32,
    pub risk: RiskLevel,
    pub approval: ApprovalMode,
    pub timeout: Duration,
    pub max_output: usize,
    pub required_arguments: Vec<String>,
    pub allowed_arguments: Vec<String>,
    pub max_argument_bytes: usize,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        version: u32,
        risk: RiskLevel,
        approval: ApprovalMode,
        timeout: Duration,
        max_output: usize,
    ) -> Self {
        Self {
            name: name.into(),
            version,
            risk,
            approval,
            timeout,
            max_output,
            required_arguments: Vec::new(),
            allowed_arguments: Vec::new(),
            max_argument_bytes: 64 * 1024,
        }
    }

    pub fn with_arguments(
        mut self,
        required: impl IntoIterator<Item = String>,
        allowed: impl IntoIterator<Item = String>,
    ) -> Self {
        self.required_arguments = required.into_iter().collect();
        self.allowed_arguments = allowed.into_iter().collect();
        self
    }

    pub fn with_max_argument_bytes(mut self, max_argument_bytes: usize) -> Self {
        self.max_argument_bytes = max_argument_bytes;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn validate_arguments(&self, arguments: &Value) -> Result<(), PolicyError> {
        let Some(object) = arguments.as_object() else {
            return Err(PolicyError::ToolArgumentsDenied(
                "arguments must be a JSON object".to_owned(),
            ));
        };
        for key in object.keys() {
            if !self.allowed_arguments.iter().any(|allowed| allowed == key) {
                return Err(PolicyError::ToolArgumentsDenied(format!(
                    "unexpected argument `{key}` for {}",
                    self.name
                )));
            }
        }
        for required in &self.required_arguments {
            if !object.contains_key(required) {
                return Err(PolicyError::ToolArgumentsDenied(format!(
                    "missing required argument `{required}` for {}",
                    self.name
                )));
            }
        }
        let encoded = serde_json::to_vec(arguments).map_err(|error| {
            PolicyError::ToolArgumentsDenied(format!("cannot encode arguments: {error}"))
        })?;
        if encoded.len() > self.max_argument_bytes {
            return Err(PolicyError::ToolArgumentsDenied(format!(
                "arguments exceed {} bytes",
                self.max_argument_bytes
            )));
        }
        for value in object.values() {
            if let Value::String(text) = value {
                if text.len() > self.max_argument_bytes {
                    return Err(PolicyError::ToolArgumentsDenied(
                        "string argument exceeds the length limit".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, spec: ToolSpec) -> Result<(), PolicyError> {
        if spec.name.trim().is_empty() || spec.name.chars().any(char::is_whitespace) {
            return Err(PolicyError::ToolArgumentsDenied(
                "tool name must be non-empty and contain no whitespace".to_owned(),
            ));
        }
        if spec.version == 0 {
            return Err(PolicyError::ToolArgumentsDenied(
                "tool version must be non-zero".to_owned(),
            ));
        }
        if spec.timeout.is_zero() || spec.max_output == 0 || spec.max_argument_bytes == 0 {
            return Err(PolicyError::ToolArgumentsDenied(
                "timeout and output/argument limits must be non-zero".to_owned(),
            ));
        }
        for required in &spec.required_arguments {
            if !spec
                .allowed_arguments
                .iter()
                .any(|allowed| allowed == required)
            {
                return Err(PolicyError::ToolArgumentsDenied(format!(
                    "required argument `{required}` is not in the allowed set"
                )));
            }
        }
        if self.tools.contains_key(&spec.name) {
            return Err(PolicyError::ToolDuplicate(spec.name));
        }
        self.tools.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            1,
            RiskLevel::ReadOnly,
            ApprovalMode::Never,
            Duration::from_secs(5),
            1024,
        )
        .with_arguments(["path".to_owned()], ["path".to_owned()])
    }

    #[test]
    fn registry_rejects_duplicate_names() {
        let mut registry = ToolRegistry::new();
        registry.register(spec("fs.read")).expect("first");
        assert_eq!(
            registry
                .register(spec("fs.read"))
                .expect_err("duplicate")
                .code(),
            "tool.duplicate"
        );
    }

    #[test]
    fn registry_rejects_extra_and_missing_arguments() {
        let spec = spec("fs.read");
        assert_eq!(
            spec.validate_arguments(&json!({ "path": "a", "extra": 1 }))
                .expect_err("extra")
                .code(),
            "tool.arguments_denied"
        );
        assert_eq!(
            spec.validate_arguments(&json!({}))
                .expect_err("missing")
                .code(),
            "tool.arguments_denied"
        );
    }

    #[test]
    fn registry_rejects_overlong_arguments() {
        let spec = spec("fs.read").with_max_argument_bytes(8);
        assert_eq!(
            spec.validate_arguments(&json!({ "path": "0123456789" }))
                .expect_err("too long")
                .code(),
            "tool.arguments_denied"
        );
    }
}
