use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Claude,
    Gemini,
    Codex,
}

impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Claude => "claude",
            RuntimeKind::Gemini => "gemini",
            RuntimeKind::Codex => "codex",
        }
    }
}
