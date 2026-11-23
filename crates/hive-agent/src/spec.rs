use std::path::PathBuf;

/// Static description of one Agent Session.
///
/// `tool_allowlist` is an explicit allowlist: an empty list means the model
/// sees no tools at all, not "all tools".
#[derive(Clone, Debug)]
pub struct AgentSpec {
    pub role: String,
    pub model: String,
    pub system_prompt: String,
    pub tool_allowlist: Vec<String>,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub workspace_root: Option<PathBuf>,
}

impl AgentSpec {
    pub const DEFAULT_MAX_TURNS: u32 = 8;
    pub const DEFAULT_MAX_TOKENS: u32 = 1024;

    pub fn new(role: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            model: model.into(),
            system_prompt: String::new(),
            tool_allowlist: Vec::new(),
            max_turns: Self::DEFAULT_MAX_TURNS,
            max_tokens: Self::DEFAULT_MAX_TOKENS,
            workspace_root: None,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.tool_allowlist = tools.into_iter().collect();
        self
    }

    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }
}
