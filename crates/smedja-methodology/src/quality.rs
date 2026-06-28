use std::path::PathBuf;

use crate::{clean, file_size, skill_inject, tdd, types::QualityScore};

/// Evaluates all four deterministic gates over a diff and returns a composite
/// [`QualityScore`].
///
/// This is the Tier 1 quality check — pure, zero model cost, always on.
///
/// # Arguments
///
/// * `diff` — unified diff text for the current turn's changes.
/// * `changed_files` — `(path, line_count)` pairs for every file touched by
///   the diff.  Used by the file-size gate.
/// * `session_skills` — skills already invoked this session.  Used by the
///   skill-inject gate.
/// * `file_size_threshold` — maximum acceptable line count per file; default
///   600 if callers pass `None`.
#[must_use]
pub fn evaluate(
    diff: &str,
    changed_files: &[(PathBuf, usize)],
    session_skills: &[String],
    file_size_threshold: Option<usize>,
) -> QualityScore {
    let threshold = file_size_threshold.unwrap_or(600);

    let tdd_pass = !tdd::evaluate(diff).is_advisory();
    let clean_pass = clean::check(diff).is_ok();
    let file_size_pass = file_size::check(changed_files, threshold).is_empty();
    let skill_inject_pass = skill_inject::check(diff, session_skills).is_empty();

    #[allow(clippy::cast_possible_truncation)]
    let score = [tdd_pass, clean_pass, file_size_pass, skill_inject_pass]
        .iter()
        .filter(|&&p| p)
        .count() as u8
        * 25;

    QualityScore {
        score,
        tdd_pass,
        clean_pass,
        file_size_pass,
        skill_inject_pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_files() -> Vec<(PathBuf, usize)> {
        vec![]
    }

    fn no_skills() -> Vec<String> {
        vec![]
    }

    #[test]
    fn perfect_diff_scores_100() {
        // 3 fns + test → tdd pass; no unwrap → clean pass; /tdd-workflow invoked.
        let diff = "\
+fn a() {}\n\
+fn b() {}\n\
+fn c() {}\n\
+#[test]\n\
+fn test_a() { assert!(true); }\n";
        let tdd_skill = vec!["/tdd-workflow".to_string()];
        let score = evaluate(diff, &no_files(), &tdd_skill, None);
        assert_eq!(score.score, 100);
        assert!(score.is_green());
    }

    #[test]
    fn tdd_fail_scores_75() {
        // 3 fns, no tests
        let diff = "+fn a() {}\n+fn b() {}\n+fn c() {}\n+fn d() {}\n";
        let score = evaluate(diff, &no_files(), &no_skills(), None);
        assert!(!score.tdd_pass);
        assert!(score.clean_pass);
        assert!(score.file_size_pass);
        assert!(score.skill_inject_pass);
        assert_eq!(score.score, 75);
    }

    #[test]
    fn clean_fail_scores_75() {
        let diff = "+let x = val.unwrap();\n";
        let score = evaluate(diff, &no_files(), &no_skills(), None);
        assert!(score.tdd_pass);
        assert!(!score.clean_pass);
        assert_eq!(score.score, 75);
    }

    #[test]
    fn file_size_fail_scores_75() {
        let diff = "";
        let big_files = vec![(PathBuf::from("main.rs"), 7880_usize)];
        let score = evaluate(diff, &big_files, &no_skills(), Some(600));
        assert!(!score.file_size_pass);
        assert_eq!(score.score, 75);
    }

    #[test]
    fn skill_inject_fail_scores_75() {
        let diff = "+headers.insert(\"Authorization\", tok);";
        let score = evaluate(diff, &no_files(), &no_skills(), None);
        assert!(!score.skill_inject_pass);
        assert_eq!(score.score, 75);
    }

    #[test]
    fn all_four_fail_scores_0() {
        // tdd fail: 4 fns no test
        // clean fail: unwrap
        // file size fail: big file
        // skill inject fail: Authorization header
        let diff = "\
+fn a() {}\n\
+fn b() {}\n\
+fn c() {}\n\
+fn d() {}\n\
+let x = v.unwrap();\n\
+headers.insert(\"Authorization\", tok);\n";
        let big_files = vec![(PathBuf::from("main.rs"), 7880_usize)];
        let score = evaluate(diff, &big_files, &no_skills(), Some(600));
        assert_eq!(score.score, 0);
        assert!(!score.is_green());
    }

    #[test]
    fn score_60_is_green() {
        // Only two gates pass — score 50 (below green); three pass → score 75 (green).
        // Verify threshold.
        let score_50 = QualityScore {
            score: 50,
            tdd_pass: true,
            clean_pass: true,
            file_size_pass: false,
            skill_inject_pass: false,
        };
        assert!(!score_50.is_green());

        let score_75 = QualityScore {
            score: 75,
            tdd_pass: true,
            clean_pass: true,
            file_size_pass: true,
            skill_inject_pass: false,
        };
        assert!(score_75.is_green());
    }

    #[test]
    fn default_threshold_is_600() {
        let files = vec![(PathBuf::from("a.rs"), 601_usize)];
        let score = evaluate("", &files, &no_skills(), None);
        assert!(!score.file_size_pass);
    }
}
