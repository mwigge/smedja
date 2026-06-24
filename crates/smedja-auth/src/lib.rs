//! Constant-time comparison helpers for secrets and authentication tokens.
//!
//! This crate wraps [`subtle::ConstantTimeEq`] to provide timing-attack
//! resistant equality checks. Use these helpers anywhere a secret value
//! (API token, session token, HMAC, etc.) is compared against
//! attacker-influenced input.

use subtle::ConstantTimeEq;

/// Compares two byte slices in constant time over equal-length inputs.
///
/// Returns `true` only if `a` and `b` have the same length and the same
/// contents. When the lengths differ, this returns `false` immediately.
///
/// # Constant-time guarantee
///
/// For inputs of *equal length*, the comparison runs in time independent of
/// where (or whether) the byte sequences first differ, so it does not leak
/// match position through timing.
///
/// # Length-leak caveat
///
/// The length check itself is **not** constant-time: an attacker can in
/// principle distinguish "wrong length" from "right length, wrong contents"
/// via timing. This is the standard, accepted behaviour for this kind of
/// helper — token/secret lengths are generally not themselves secret.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Compares two strings for equality in constant time over equal-length inputs.
///
/// Convenience wrapper that delegates to [`constant_time_eq`] on the UTF-8
/// byte representations. Inherits the same constant-time guarantee and the
/// same length-leak caveat documented there.
#[must_use]
pub fn tokens_match(a: &str, b: &str) -> bool {
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, tokens_match};

    #[test]
    fn equal_inputs_match() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
    }

    #[test]
    fn unequal_same_length_inputs_do_not_match() {
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
    }

    #[test]
    fn differing_length_inputs_do_not_match() {
        assert!(!constant_time_eq(b"short", b"considerably-longer"));
    }

    #[test]
    fn empty_inputs_match() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn tokens_match_delegates_to_byte_comparison() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc123", "abc124"));
        assert!(!tokens_match("abc", "abcd"));
    }
}
