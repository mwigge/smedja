/// The canonical conversation message types are owned by `smedja-adapter` (the
/// provider boundary). Working memory re-uses them directly so there is a single
/// source of truth and no lossy conversion before the provider call.
pub use smedja_adapter::types::{Message, Role};

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
