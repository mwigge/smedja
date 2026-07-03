use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;
use uuid::Uuid;

mod queries;
mod updates;

pub(crate) use queries::{create, get, list, search};
pub(crate) use updates::{
    delete, update_cowork_mode, update_mode, update_model_override, update_runner_override,
    update_status, update_task_id, update_title, update_workspace_root,
};

/// A top-level orchestration session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Timestamp when the session was created (micros since the Unix epoch).
    pub created_at: Timestamp,
    /// Timestamp of the last status change (micros since the Unix epoch).
    pub updated_at: Timestamp,
    /// Lifecycle status: `"active"`, `"complete"`, or `"failed"`.
    pub status: String,
    /// Optional associated task identifier.
    pub task_id: Option<String>,
    /// Optional operating mode: `"tdd"`, `"ponytail"`, `"spec"`, or `"sre"`.
    pub mode: Option<String>,
    /// Human-readable session title supplied by the caller at creation time.
    #[serde(default)]
    pub title: String,
    /// Whether human-in-the-loop cowork gate is active for this session.
    pub cowork_mode: bool,
    /// Optional filesystem path to the workspace root for this session.
    pub workspace_root: Option<String>,
    /// Optional model name override; when set, `run_turn` uses this instead of
    /// the `SMEDJA_MODEL` environment variable.
    pub model_override: Option<String>,
    /// Optional runner override; when set, `run_turn` bypasses the assayer and
    /// routes to this runner (e.g. `"claude-cli"`, `"codex-cli"`, `"local"`).
    pub runner_override: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_000.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_000.0),
            status: "active".to_string(),
            task_id: None,
            mode: Some("tdd".to_string()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[test]
    fn create_then_get_returns_session() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, s.id);
        assert_eq!(fetched.status, "active");
        assert_eq!(fetched.mode.as_deref(), Some("tdd"));
    }

    #[test]
    fn get_unknown_session_returns_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let result = ingot.get_session("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn update_session_status_changes_status() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_status(&s.id.to_string(), "complete")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.status, "complete");
    }

    #[test]
    fn update_status_changes_updated_at() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_status(&s.id.to_string(), "failed")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        // updated_at must be >= created_at (set by update_session_status)
        assert!(fetched.updated_at >= fetched.created_at);
    }

    #[test]
    fn nullable_task_id_and_mode_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_002.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_002.0),
            status: "active".to_string(),
            task_id: Some("task-xyz".to_string()),
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.task_id.as_deref(), Some("task-xyz"));
        assert!(fetched.mode.is_none());
    }

    #[test]
    fn workspace_root_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_workspace_root(&s.id.to_string(), "/home/user/projects/myrepo")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(
            fetched.workspace_root.as_deref(),
            Some("/home/user/projects/myrepo")
        );
    }

    #[test]
    fn update_mode_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_mode(&s.id.to_string(), "ponytail")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.mode.as_deref(), Some("ponytail"));
    }

    #[test]
    fn model_override_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_model_override(&s.id.to_string(), "gemma4-27b")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.model_override.as_deref(), Some("gemma4-27b"));
    }

    #[test]
    fn model_override_defaults_to_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert!(fetched.model_override.is_none());
    }

    #[test]
    fn task_id_link_round_trip() {
        use crate::Task;
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let task_id = Uuid::new_v4();
        let task = Task {
            id: task_id,
            title: "Test task".to_string(),
            description: String::new(),
            status: "planned".to_string(),
            created_at: Timestamp::from_secs_f64(1_700_000_010.0),
            session_id: Some(s.id.to_string()),
            response: None,
        };
        ingot.create_task(&task).unwrap();

        ingot
            .update_session_task_id(&s.id.to_string(), &task_id.to_string())
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(
            fetched.task_id.as_deref(),
            Some(task_id.to_string().as_str())
        );
    }

    #[test]
    fn runner_override_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_runner_override(&s.id.to_string(), "codex-cli")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.runner_override.as_deref(), Some("codex-cli"));
    }

    #[test]
    fn runner_override_defaults_to_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert!(fetched.runner_override.is_none());
    }

    #[test]
    fn update_title_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        update_title(&ingot.conn, &s.id.to_string(), "my new title").unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.title, "my new title");
    }

    #[test]
    fn update_title_unknown_id_is_noop() {
        let ingot = Ingot::open_in_memory().unwrap();
        update_title(&ingot.conn, "no-such-id", "ignored").unwrap();
    }

    #[test]
    fn search_sessions_matches_title_substring() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.title = "rust memory pressure investigation".to_string();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("memory").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, s.id);
    }

    #[test]
    fn search_sessions_matches_workspace_root() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.workspace_root = Some("/home/user/projects/smedja".to_string());
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("smedja").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, s.id);
    }

    #[test]
    fn search_sessions_returns_empty_for_no_match() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("zzznomatch").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_sessions_is_case_insensitive() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.title = "Rust Project".to_string();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("rust").unwrap();
        assert_eq!(results.len(), 1);
    }
}
