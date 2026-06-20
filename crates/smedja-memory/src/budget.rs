use crate::types::Message;

/// Estimates the token count for a text string.
///
/// Uses the naive approximation of 1 token ≈ 4 characters, which is accurate
/// enough for context-window budgeting purposes.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Estimates the total token count for a slice of messages.
///
/// Sums [`estimate_tokens`] over each message's content. Role labels and
/// framing tokens are not counted; callers should add a small overhead if
/// exact fidelity is required.
#[must_use]
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| estimate_tokens(&m.content)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_four_chars() {
        assert_eq!(estimate_tokens("abcd"), 1);
    }

    #[test]
    fn estimate_tokens_five_chars() {
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn estimate_messages_tokens_sum() {
        let messages = vec![
            Message::user("abcd"),       // 1 token
            Message::assistant("abcde"), // 2 tokens
        ];
        assert_eq!(estimate_messages_tokens(&messages), 3);
    }
}
