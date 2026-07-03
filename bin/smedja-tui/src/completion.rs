use crate::state::AppState;

/// Lists directory entries for the file picker: `../` first, then sorted dirs, then files.
pub(crate) fn list_dir_entries(dir: &std::path::Path) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    if dir.parent().is_some() {
        entries.push(("../".to_owned(), true));
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // skip hidden
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            dirs.push((format!("{name}/"), true));
        } else {
            files.push((name, false));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    entries.extend(dirs);
    entries.extend(files);
    entries
}

/// Opens the file picker overlay rooted at `dir`.
pub(crate) fn open_file_picker(state: &mut AppState, dir: std::path::PathBuf) {
    state.file_picker_entries = list_dir_entries(&dir);
    state.file_picker_dir = dir;
    state.file_picker_cursor = 0;
    state.file_picker_open = true;
}

pub(crate) fn accept_slash_completion(state: &mut AppState, append_space: bool) -> bool {
    let Some(completion) = state.slash_completions.get(state.slash_cursor).cloned() else {
        state.slash_popup_visible = false;
        return false;
    };
    completion.clone_into(&mut state.input);
    if append_space {
        state.input.push(' ');
    }
    state.input_cursor = state.input.len();
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    true
}

pub(crate) fn clear_slash_popup(state: &mut AppState) {
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    state.input.clear();
    state.input_cursor = 0;
    state.runner_picker_mode = false;
    state.session_picker_mode = false;
    state.command_palette_mode = false;
    state.session_picker_ids.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::commands::filtered_completions;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn slash_accept_space_inserts_completion_with_trailing_space() {
        let mut state = make_state("test-session");
        state.input = "/ti".to_owned();
        state.slash_completions = filtered_completions("/ti");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        assert!(accept_slash_completion(&mut state, true));

        assert_eq!(state.input, "/tier ");
        assert!(!state.slash_popup_visible);
        assert!(state.slash_completions.is_empty());
    }

    #[test]
    fn slash_accept_enter_inserts_completion_without_space() {
        let mut state = make_state("test-session");
        state.input = "/h".to_owned();
        state.slash_completions = filtered_completions("/h");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        assert!(accept_slash_completion(&mut state, false));

        assert_eq!(state.input, "/health");
        assert!(!state.slash_popup_visible);
    }

    #[test]
    fn clear_slash_popup_resets_runner_picker_mode() {
        let mut state = make_state("sess-popup");
        state.runner_picker_mode = true;
        state.slash_popup_visible = true;
        state.slash_completions = vec!["claude".to_owned()];

        clear_slash_popup(&mut state);

        assert!(
            !state.runner_picker_mode,
            "runner_picker_mode must be false after clear"
        );
        assert!(!state.slash_popup_visible);
        assert!(state.slash_completions.is_empty());
    }
}
