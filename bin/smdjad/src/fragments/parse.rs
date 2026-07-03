//! Fragment parsing: splits a submitted message into literal-text and
//! recognised-fragment spans.

use crate::fragments::Fragment;

/// Returns `true` when `kind` names a recognised fragment.
fn is_known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "file" | "git" | "branch" | "shell" | "clippy" | "lsp" | "paste"
    )
}

/// Parses `content` into a sequence of literal-text and recognised-fragment
/// spans.
///
/// A fragment is recognised only when `@` begins a token (preceded by
/// start-of-string or whitespace). `@file` consumes the next whitespace-delimited
/// token as its path; `@shell` consumes the remainder of the line as its command;
/// `@git` / `@branch` take no argument. An `@<kind>` with an unknown kind — or
/// `@file` with no following path token — is left verbatim.
#[must_use]
pub(crate) fn parse(content: &str) -> Vec<Fragment> {
    let bytes = content.as_bytes();
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut literal_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'@' || !at_token_boundary(bytes, i) {
            i += 1;
            continue;
        }

        let kind_start = i + 1;
        let kind_end = scan_word(bytes, kind_start);
        let kind = &content[kind_start..kind_end];
        if !is_known_kind(kind) {
            i = kind_end.max(i + 1);
            continue;
        }

        let Some((fragment, consumed_end)) = take_fragment(content, bytes, kind, kind_end) else {
            // Recognised kind but malformed (e.g. `@file` with no path): leave
            // verbatim by skipping past the kind word.
            i = kind_end.max(i + 1);
            continue;
        };

        // Flush any pending literal text before this fragment.
        if literal_start < i {
            fragments.push(Fragment::Literal(content[literal_start..i].to_owned()));
        }
        fragments.push(fragment);
        i = consumed_end;
        literal_start = consumed_end;
    }

    if literal_start < content.len() {
        fragments.push(Fragment::Literal(content[literal_start..].to_owned()));
    }
    fragments
}

/// Returns `true` when the `@` at `at` begins a token (start-of-string or
/// preceded by an ASCII-whitespace byte).
fn at_token_boundary(bytes: &[u8], at: usize) -> bool {
    at == 0 || bytes[at - 1].is_ascii_whitespace()
}

/// Returns the index one past the run of word bytes (ASCII alphanumeric,
/// underscore, or hyphen) starting at `start`.
fn scan_word(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
    {
        j += 1;
    }
    j
}

/// Builds the [`Fragment`] for a recognised `kind` whose word ends at `kind_end`,
/// returning the fragment and the byte offset one past everything it consumes.
///
/// Returns `None` when the kind requires an argument that is absent (e.g. `@file`
/// with no following path token), so the caller can leave the token verbatim.
fn take_fragment(
    content: &str,
    bytes: &[u8],
    kind: &str,
    kind_end: usize,
) -> Option<(Fragment, usize)> {
    match kind {
        "git" => Some((Fragment::Git, kind_end)),
        "branch" => Some((Fragment::Branch, kind_end)),
        "clippy" => Some((Fragment::Clippy, kind_end)),
        "lsp" => Some((Fragment::Lsp, kind_end)),
        "file" => {
            // Skip inline spaces/tabs (not newlines) before the path token.
            let path_start = skip_inline_space(bytes, kind_end);
            let path_end = scan_path(bytes, path_start);
            if path_end == path_start {
                return None;
            }
            Some((
                Fragment::File(content[path_start..path_end].to_owned()),
                path_end,
            ))
        }
        "shell" => {
            let cmd_start = skip_inline_space(bytes, kind_end);
            let cmd_end = scan_to_eol(bytes, cmd_start);
            let cmd = content[cmd_start..cmd_end].trim_end();
            if cmd.is_empty() {
                return None;
            }
            Some((Fragment::Shell(cmd.to_owned()), cmd_end))
        }
        "paste" => {
            // The format is @paste:{sha8}. After "paste", the next char must be ':'.
            if bytes.get(kind_end) != Some(&b':') {
                return None;
            }
            let sha_start = kind_end + 1;
            let sha_end = scan_word(bytes, sha_start);
            if sha_end == sha_start {
                return None;
            }
            let sha8 = content[sha_start..sha_end].to_owned();
            Some((Fragment::Paste(sha8), sha_end))
        }
        _ => None,
    }
}

/// Returns the index past any run of inline spaces/tabs starting at `start`
/// (newlines terminate the run).
fn skip_inline_space(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    j
}

/// Returns the index one past the path token starting at `start` (a path is a run
/// of non-whitespace bytes).
fn scan_path(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    j
}

/// Returns the index of the next newline at or after `start`, or the end of the
/// buffer when none remains.
fn scan_to_eol(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && bytes[j] != b'\n' {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_known_fragments_only_at_token_boundary() {
        let frags = parse("see @file src/lib.rs and @git then @branch and @shell echo hi");
        assert_eq!(
            frags,
            vec![
                Fragment::Literal("see ".to_owned()),
                Fragment::File("src/lib.rs".to_owned()),
                Fragment::Literal(" and ".to_owned()),
                Fragment::Git,
                Fragment::Literal(" then ".to_owned()),
                Fragment::Branch,
                Fragment::Literal(" and ".to_owned()),
                Fragment::Shell("echo hi".to_owned()),
            ]
        );

        // `@` not at a token boundary is never a fragment.
        let frags = parse("email me at foo@bar.com or user@file.com");
        assert_eq!(
            frags,
            vec![Fragment::Literal(
                "email me at foo@bar.com or user@file.com".to_owned()
            )]
        );
    }

    #[test]
    fn unknown_fragment_left_verbatim() {
        let frags = parse("hello @world and @fileness stays");
        assert_eq!(
            frags,
            vec![Fragment::Literal(
                "hello @world and @fileness stays".to_owned()
            )]
        );
    }

    #[test]
    fn shell_consumes_to_end_of_line_only() {
        let frags = parse("@shell ls -la | grep foo\nnext line");
        assert_eq!(
            frags,
            vec![
                Fragment::Shell("ls -la | grep foo".to_owned()),
                Fragment::Literal("\nnext line".to_owned()),
            ]
        );
    }

    #[test]
    fn file_without_path_left_verbatim() {
        let frags = parse("@file\n");
        assert_eq!(frags, vec![Fragment::Literal("@file\n".to_owned())]);
    }

    #[test]
    fn paste_fragment_parsed() {
        let frags = parse("look at @paste:abc12345 please");
        assert_eq!(
            frags,
            vec![
                Fragment::Literal("look at ".to_owned()),
                Fragment::Paste("abc12345".to_owned()),
                Fragment::Literal(" please".to_owned()),
            ]
        );
    }

    #[test]
    fn paste_without_colon_left_verbatim() {
        // "paste" without ":sha8" is not a valid fragment — left as literal.
        let frags = parse("@paste nope");
        assert_eq!(frags, vec![Fragment::Literal("@paste nope".to_owned())]);
    }

    #[test]
    fn paste_with_empty_sha_left_verbatim() {
        // "@paste:" with nothing after the colon is not a valid fragment.
        let frags = parse("@paste: nope");
        assert_eq!(frags, vec![Fragment::Literal("@paste: nope".to_owned())]);
    }
}
