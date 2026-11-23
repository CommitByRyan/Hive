//! Model capability catalog built from Magpie's extended `/v1/models` payload.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::error::LlmError;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub data: Vec<ModelEntry>,
    #[serde(default)]
    pub first_id: Option<String>,
    #[serde(default)]
    pub last_id: Option<String>,
    #[serde(default)]
    pub has_more: Option<bool>,
}

/// One `/v1/models` entry. Fields beyond the OpenAI minimum are optional and
/// unknown additions are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub tool_call: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<String>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub modalities: Option<Value>,
    #[serde(default)]
    pub pricing: Option<Value>,
    #[serde(default)]
    pub native_endpoints: Option<Value>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub created: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelCapabilities {
    pub id: String,
    pub display_name: Option<String>,
    pub owned_by: Option<String>,
    /// Absent means "not advertised"; treat as unsupported.
    pub tool_call: bool,
    pub reasoning: bool,
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning_levels: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: Option<Value>,
    pub kind: Option<String>,
}

impl ModelCapabilities {
    pub fn supports_reasoning_level(&self, level: &str) -> bool {
        self.reasoning_levels
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(level))
    }
}

impl From<ModelEntry> for ModelCapabilities {
    fn from(entry: ModelEntry) -> Self {
        let reasoning_levels = if entry.reasoning_levels.is_empty() {
            entry.supported_reasoning_levels
        } else {
            entry.reasoning_levels
        };
        Self {
            id: entry.id,
            display_name: entry.display_name,
            owned_by: entry.owned_by,
            tool_call: entry.tool_call.unwrap_or(false),
            reasoning: entry.reasoning.unwrap_or(false),
            context_length: entry.context_length.or(entry.context_window),
            max_output_tokens: entry.max_output_tokens,
            reasoning_levels,
            supported_parameters: entry.supported_parameters,
            pricing: entry.pricing,
            kind: entry.kind,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModelCatalog {
    models: BTreeMap<String, ModelCapabilities>,
}

impl ModelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_json_str(text: &str) -> Result<Self, LlmError> {
        if let Ok(response) = serde_json::from_str::<ModelsResponse>(text) {
            return Ok(Self::from_response(response));
        }
        let entries: Vec<ModelEntry> = serde_json::from_str(text)?;
        Ok(Self::from_entries(entries))
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, LlmError> {
        let text = std::str::from_utf8(bytes).map_err(|error| {
            LlmError::InvalidResponse(format!("models payload is not UTF-8: {error}"))
        })?;
        Self::from_json_str(text)
    }

    pub fn from_response(response: ModelsResponse) -> Self {
        Self::from_entries(response.data)
    }

    pub fn from_entries(entries: Vec<ModelEntry>) -> Self {
        let mut catalog = Self::new();
        for entry in entries {
            catalog.insert(entry);
        }
        catalog
    }

    /// Insert or replace a model. Entries without an `id` are ignored.
    pub fn insert(&mut self, entry: ModelEntry) -> bool {
        if entry.id.is_empty() {
            return false;
        }
        let id = entry.id.clone();
        self.models.insert(id, ModelCapabilities::from(entry));
        true
    }

    pub fn get(&self, id: &str) -> Option<&ModelCapabilities> {
        self.models.get(id)
    }

    pub fn require(&self, id: &str) -> Result<&ModelCapabilities, LlmError> {
        self.models
            .get(id)
            .ok_or_else(|| LlmError::UnknownModel(id.to_owned()))
    }

    pub fn contains(&self, id: &str) -> bool {
        self.models.contains_key(id)
    }

    pub fn supports_tools(&self, id: &str) -> bool {
        self.models.get(id).is_some_and(|model| model.tool_call)
    }

    pub fn supports_reasoning_level(&self, id: &str, level: &str) -> bool {
        self.models
            .get(id)
            .is_some_and(|model| model.supports_reasoning_level(level))
    }

    pub fn context_length(&self, id: &str) -> Option<u64> {
        self.models.get(id).and_then(|model| model.context_length)
    }

    pub fn max_output_tokens(&self, id: &str) -> Option<u64> {
        self.models
            .get(id)
            .and_then(|model| model.max_output_tokens)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = r#"{
        "object": "list",
        "data": [
            {
                "id": "workbuddy/deepseek-v4.1-flash",
                "display_name": "DeepSeek V4.1 Flash",
                "context_length": 1000000,
                "max_output_tokens": 128000,
                "tool_call": true,
                "reasoning": true,
                "reasoning_levels": ["none", "low", "high", "max"],
                "pricing": {"input": 0.1, "output": 0.4, "currency": "USD"},
                "native_endpoints": ["chat"],
                "unknown_extension": {"nested": true}
            },
            {
                "id": "auto/auto",
                "context_window": 256000,
                "max_output_tokens": 32000,
                "tool_call": true,
                "supported_reasoning_levels": ["low", "high", "max"]
            },
            {"display_name": "no id, ignored"}
        ],
        "first_id": "workbuddy/deepseek-v4.1-flash",
        "last_id": "auto/auto",
        "has_more": false,
        "future_field": 42
    }"#;

    #[test]
    fn parses_extended_capabilities_and_ignores_unknown_fields() {
        let catalog = ModelCatalog::from_json_str(MODELS).expect("catalog parses");
        assert_eq!(catalog.len(), 2);
        let model = catalog
            .require("workbuddy/deepseek-v4.1-flash")
            .expect("model");
        assert!(model.tool_call);
        assert!(model.reasoning);
        assert_eq!(model.context_length, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert!(model.supports_reasoning_level("HIGH"));
        assert!(!model.supports_reasoning_level("xhigh"));
        assert!(model.pricing.is_some());
    }

    #[test]
    fn falls_back_to_context_window_and_supported_levels() {
        let catalog = ModelCatalog::from_json_str(MODELS).expect("catalog parses");
        assert_eq!(catalog.context_length("auto/auto"), Some(256_000));
        assert_eq!(catalog.max_output_tokens("auto/auto"), Some(32_000));
        assert!(catalog.supports_reasoning_level("auto/auto", "max"));
        assert!(catalog.supports_tools("auto/auto"));
    }

    #[test]
    fn unknown_model_is_a_typed_error() {
        let catalog = ModelCatalog::from_json_str(MODELS).expect("catalog parses");
        assert!(!catalog.contains("does-not-exist"));
        assert!(!catalog.supports_tools("does-not-exist"));
        match catalog.require("does-not-exist") {
            Err(LlmError::UnknownModel(id)) => assert_eq!(id, "does-not-exist"),
            other => panic!("expected unknown model, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_bare_model_array() {
        let catalog =
            ModelCatalog::from_json_str(r#"[{"id":"a/b","tool_call":true,"context_length":100}]"#)
                .expect("bare array");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.context_length("a/b"), Some(100));
    }
}
