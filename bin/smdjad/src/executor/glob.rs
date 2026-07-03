//! Minimal glob matcher shared by the `list_files` and `find_files` handlers.

/// Minimal glob matcher supporting `*` (any sequence except `/`) and `?` (one char).
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let mut p = pattern.as_bytes();
    let mut s = name.as_bytes();
    loop {
        match (p.first(), s.first()) {
            (None, None) => return true,
            (Some(&b'*'), _) => {
                p = &p[1..];
                if p.is_empty() {
                    return true;
                }
                // Try matching `*` against 0..n chars.
                for i in 0..=s.len() {
                    if glob_match(
                        std::str::from_utf8(p).unwrap_or(""),
                        std::str::from_utf8(&s[i..]).unwrap_or(""),
                    ) {
                        return true;
                    }
                }
                return false;
            }
            (Some(&b'?'), Some(_)) => {
                p = &p[1..];
                s = &s[1..];
            }
            (Some(a), Some(b)) if a == b => {
                p = &p[1..];
                s = &s[1..];
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_match_star_matches_any_suffix() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.toml"));
    }

    #[test]
    fn glob_match_question_mark_matches_one_char() {
        assert!(glob_match("fo?", "foo"));
        assert!(!glob_match("fo?", "fo"));
    }

    #[test]
    fn glob_match_star_matches_empty() {
        assert!(glob_match("*.rs", ".rs"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "cargo.toml"));
    }
}
