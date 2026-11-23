//! Wire types for the OpenAI chat-completions surface exposed by Magpie.
//!
//! Every response type is parsed leniently: unknown fields are ignored, missing
//! fields fall back to `Option`/`Default`, and unrecognised enum strings are
//! preserved instead of failing the whole response.

use std::fmt::{self, Display, Formatter};

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Deserialize an optional string that may also arrive as `null`, a number or
/// a boolean. Magpie returns `"code": null` in error bodies.
pub(crate) fn de_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text),
        Some(other) => Some(other.to_string()),
    })
}

fn default_function_kind() -> String {
    "function".to_owned()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
    Developer,
    Unknown(String),
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Developer => "developer",
            Self::Unknown(other) => other,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "system" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            "developer" => Self::Developer,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl Display for Role {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Role {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
}

/// Why the model stopped generating.
///
/// Unknown values are kept verbatim so newer gateways never break parsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    Unknown(String),
}

impl FinishReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::FunctionCall => "function_call",
            Self::Unknown(other) => other,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            "function_call" => Self::FunctionCall,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Length)
    }
}

impl Display for FinishReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for FinishReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Message {
    #[serde(default)]
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Self::default()
        }
    }

    pub fn text(&self) -> Option<&str> {
        self.content.as_deref()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FunctionDefinition {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

impl FunctionDefinition {
    pub fn new(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters: Some(parameters),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tool {
    #[serde(rename = "type", default = "default_function_kind")]
    pub kind: String,
    #[serde(default)]
    pub function: FunctionDefinition,
}

impl Default for Tool {
    fn default() -> Self {
        Self {
            kind: default_function_kind(),
            function: FunctionDefinition::default(),
        }
    }
}

impl Tool {
    pub fn function(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            kind: default_function_kind(),
            function: FunctionDefinition::new(name, parameters),
        }
    }
}

/// How the model may use tools. Unknown string modes are preserved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolChoice {
    None,
    Auto,
    Required,
    Function { name: String },
    Other(String),
}

impl ToolChoice {
    pub fn function(name: impl Into<String>) -> Self {
        Self::Function { name: name.into() }
    }
}

impl Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::None => serializer.serialize_str("none"),
            Self::Auto => serializer.serialize_str("auto"),
            Self::Required => serializer.serialize_str("required"),
            Self::Other(other) => serializer.serialize_str(other),
            Self::Function { name } => {
                use serde::ser::SerializeMap;

                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;
                let function = FunctionChoiceRef { name };
                map.serialize_entry("function", &function)?;
                map.end()
            }
        }
    }
}

#[derive(Serialize)]
struct FunctionChoiceRef<'a> {
    name: &'a str,
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => Ok(match text.as_str() {
                "none" => Self::None,
                "auto" => Self::Auto,
                "required" => Self::Required,
                other => Self::Other(other.to_owned()),
            }),
            Value::Object(map) => {
                let kind = map
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                if kind != "function" {
                    return Ok(Self::Other(kind.to_owned()));
                }
                let name = map
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(Self::Function { name })
            }
            _ => Err(de::Error::custom(
                "tool_choice must be a string or an object",
            )),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            ..Self::default()
        }
    }

    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

impl FunctionCall {
    pub fn parse_arguments(&self) -> Result<Value, serde_json::Error> {
        parse_arguments(&self.arguments)
    }
}

pub(crate) fn parse_arguments(arguments: &str) -> Result<Value, serde_json::Error> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(trimmed)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default = "default_function_kind")]
    pub kind: String,
    #[serde(default)]
    pub function: FunctionCall,
}

impl Default for ToolCall {
    fn default() -> Self {
        Self {
            id: None,
            kind: default_function_kind(),
            function: FunctionCall::default(),
        }
    }
}

impl ToolCall {
    pub fn name(&self) -> &str {
        &self.function.name
    }

    pub fn parse_arguments(&self) -> Result<Value, serde_json::Error> {
        self.function.parse_arguments()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Choice {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default)]
    pub message: Message,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

impl ChatResponse {
    pub fn first_choice(&self) -> Option<&Choice> {
        self.choices.first()
    }

    pub fn message(&self) -> Option<&Message> {
        self.first_choice().map(|choice| &choice.message)
    }

    pub fn text(&self) -> Option<&str> {
        self.message().and_then(Message::text)
    }

    pub fn finish_reason(&self) -> Option<&FinishReason> {
        self.first_choice()
            .and_then(|choice| choice.finish_reason.as_ref())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub accepted_prediction_tokens: Option<u32>,
    #[serde(default)]
    pub rejected_prediction_tokens: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub audio_tokens: Option<u32>,
}

/// Magpie's per-request quota snapshot. All fields are optional because only
/// the final streaming chunk carries it and the shape is gateway-defined.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct QuotaInfo {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub percent: Option<f32>,
    #[serde(default)]
    pub reset_at: Option<i64>,
    #[serde(default)]
    pub shared_models: Option<Vec<String>>,
    #[serde(default)]
    pub credit: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
    #[serde(default)]
    pub quota: Option<QuotaInfo>,
}

impl Usage {
    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens)
    }

    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default)]
    pub delta: Delta,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_tolerates_missing_details() {
        let usage: Usage = serde_json::from_str(r#"{"prompt_tokens":3,"total_tokens":5}"#)
            .expect("usage should parse without optional fields");
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 0);
        assert!(usage.completion_tokens_details.is_none());
        assert!(usage.cached_tokens().is_none());
    }

    #[test]
    fn usage_reads_reasoning_and_cached_tokens() {
        let usage: Usage = serde_json::from_str(
            r#"{
                "prompt_tokens": 10,
                "completion_tokens": 7,
                "total_tokens": 17,
                "completion_tokens_details": {"reasoning_tokens": 4},
                "prompt_tokens_details": {"cached_tokens": 6, "cache_write_tokens": 2},
                "quota": {"model": "auto/auto", "percent": 42.5, "reset_at": 1764000000}
            }"#,
        )
        .expect("extended usage should parse");
        assert_eq!(usage.reasoning_tokens(), Some(4));
        assert_eq!(usage.cached_tokens(), Some(6));
        let quota = usage.quota.expect("quota present");
        assert_eq!(quota.model.as_deref(), Some("auto/auto"));
        assert_eq!(quota.percent, Some(42.5));
    }

    #[test]
    fn finish_reason_keeps_unknown_values() {
        let reason: FinishReason =
            serde_json::from_str(r#""guardrail""#).expect("unknown finish reason parses");
        assert_eq!(reason, FinishReason::Unknown("guardrail".to_owned()));
        assert_eq!(reason.as_str(), "guardrail");
        assert!(!reason.is_truncated());
    }

    #[test]
    fn finish_reason_null_is_none() {
        let choice: ChunkChoice =
            serde_json::from_str(r#"{"index":0,"finish_reason":null,"delta":{}}"#)
                .expect("null finish reason parses");
        assert!(choice.finish_reason.is_none());
    }

    #[test]
    fn response_ignores_unknown_fields_and_roles() {
        let response: ChatResponse = serde_json::from_str(
            r#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "auto/auto",
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "orchestrator", "content": "hi", "extra": true},
                    "vendor_extension": {"x": 1}
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                "unexpected": [1, 2, 3]
            }"#,
        )
        .expect("lenient response parse");
        assert_eq!(response.text(), Some("hi"));
        assert_eq!(
            response.message().map(|message| &message.role),
            Some(&Role::Unknown("orchestrator".to_owned()))
        );
        assert_eq!(response.finish_reason(), Some(&FinishReason::Stop));
    }

    #[test]
    fn tool_choice_round_trips_modes_and_functions() {
        assert_eq!(
            serde_json::from_str::<ToolChoice>(r#""required""#).expect("mode"),
            ToolChoice::Required
        );
        let choice: ToolChoice =
            serde_json::from_str(r#"{"type":"function","function":{"name":"lookup"}}"#)
                .expect("function choice");
        assert_eq!(
            choice,
            ToolChoice::Function {
                name: "lookup".to_owned()
            }
        );
        let encoded = serde_json::to_string(&choice).expect("serialize");
        assert_eq!(
            encoded,
            r#"{"type":"function","function":{"name":"lookup"}}"#
        );
    }
}
