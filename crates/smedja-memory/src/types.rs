use serde::{Deserialize, Serialize};

/// The role of a participant in a conversation turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// An OpenAI-style system prompt or context injection.
    System,
    /// A human (or tool-injected) user message.
    User,
    /// A model-generated assistant message.
    Assistant,
    /// A tool-call result fed back into the conversation.
    Tool,
}

impl Role {
    /// Returns the lowercase string representation used in provider APIs.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A single message in a conversation, with a role and text content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Who produced this message.
    pub role: Role,
    /// The text content of the message.
    pub content: String,
}

impl Message {
    /// Creates a new system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    /// Creates a new user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Creates a new assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Memory stratum — determines the retention policy for a stored turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stratum {
    /// Last N turns — always included in context verbatim.
    Hot,
    /// Turns beyond Hot but within the warm limit — included when budget allows.
    Warm,
    /// Oldest in-memory turns — stored in smedja-vault, retrieved on demand.
    Cold,
    /// Completed sessions — archived in smedja-ingot only.
    Archive,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_as_str_system() {
        assert_eq!(Role::System.as_str(), "system");
    }

    #[test]
    fn role_as_str_all_variants() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::Tool.as_str(), "tool");
    }

    #[test]
    fn message_constructors() {
        let sys = Message::system("hello");
        assert_eq!(sys.role, Role::System);
        assert_eq!(sys.content, "hello");

        let usr = Message::user("world");
        assert_eq!(usr.role, Role::User);
        assert_eq!(usr.content, "world");

        let ast = Message::assistant("ok");
        assert_eq!(ast.role, Role::Assistant);
        assert_eq!(ast.content, "ok");
    }
}
