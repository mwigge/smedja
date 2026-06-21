//! Loop engine persistence — `loops` table tracking multi-role pipeline runs.

use serde::{Deserialize, Serialize};

/// Persisted state for a single loop run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRecord {
    /// Unique identifier for this loop run (UUID recommended).
    pub id: String,
    /// Name of the `OpenSpec` change driving this loop.
    pub change_name: String,
    /// Current lifecycle status: `planning`, `slicing`, `verifying`, `reviewing`,
    /// `complete`, or `failed`.
    pub status: String,
    /// Index of the slice currently being processed (0-based).
    pub current_slice: i64,
    /// Attempt number for the current slice (1-based).
    pub attempt: i64,
    /// Unix epoch timestamp (seconds) when this record was created.
    pub created_at: f64,
    /// Unix epoch timestamp (seconds) when this record was last updated.
    pub updated_at: f64,
}

pub(crate) fn insert(
    conn: &rusqlite::Connection,
    rec: &LoopRecord,
) -> Result<(), crate::error::IngotError> {
    conn.execute(
        "INSERT INTO loops \
         (id, change_name, status, current_slice, attempt, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            rec.id,
            rec.change_name,
            rec.status,
            rec.current_slice,
            rec.attempt,
            rec.created_at,
            rec.updated_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn update_status(
    conn: &rusqlite::Connection,
    id: &str,
    status: &str,
    updated_at: f64,
) -> Result<(), crate::error::IngotError> {
    conn.execute(
        "UPDATE loops SET status = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![status, updated_at, id],
    )?;
    Ok(())
}

pub(crate) fn get(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<Option<LoopRecord>, crate::error::IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, change_name, status, current_slice, attempt, created_at, updated_at \
         FROM loops WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![id], |row| {
        Ok(LoopRecord {
            id: row.get(0)?,
            change_name: row.get(1)?,
            status: row.get(2)?,
            current_slice: row.get(3)?,
            attempt: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub(crate) fn list_by_change(
    conn: &rusqlite::Connection,
    change_name: &str,
) -> Result<Vec<LoopRecord>, crate::error::IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, change_name, status, current_slice, attempt, created_at, updated_at \
         FROM loops WHERE change_name = ?1 ORDER BY created_at DESC",
    )?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map(rusqlite::params![change_name], |row| {
            Ok(LoopRecord {
                id: row.get(0)?,
                change_name: row.get(1)?,
                status: row.get(2)?,
                current_slice: row.get(3)?,
                attempt: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect();
    Ok(rows?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample(id: &str) -> LoopRecord {
        LoopRecord {
            id: id.into(),
            change_name: "smedja".into(),
            status: "planning".into(),
            current_slice: 0,
            attempt: 1,
            created_at: 1_000.0,
            updated_at: 1_000.0,
        }
    }

    #[test]
    fn insert_and_get_loop_record() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.create_loop(&sample("loop-1")).unwrap();
        let got = ig.get_loop("loop-1").unwrap().unwrap();
        assert_eq!(got.status, "planning");
        assert_eq!(got.change_name, "smedja");
    }

    #[test]
    fn update_loop_status() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.create_loop(&sample("l2")).unwrap();
        ig.update_loop_status("l2", "complete", 2_000.0).unwrap();
        let got = ig.get_loop("l2").unwrap().unwrap();
        assert_eq!(got.status, "complete");
        assert!((got.updated_at - 2_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn get_returns_none_for_missing_id() {
        let ig = Ingot::open_in_memory().unwrap();
        assert!(ig.get_loop("nonexistent").unwrap().is_none());
    }

    #[test]
    fn list_by_change_returns_in_descending_order() {
        let mut ig = Ingot::open_in_memory().unwrap();
        let mut early = sample("early");
        early.created_at = 100.0;
        early.updated_at = 100.0;
        let mut late = sample("late");
        late.created_at = 200.0;
        late.updated_at = 200.0;
        ig.create_loop(&early).unwrap();
        ig.create_loop(&late).unwrap();
        let list = ig.list_loops("smedja").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "late");
        assert_eq!(list[1].id, "early");
    }

    #[test]
    fn list_by_change_filters_by_change_name() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.create_loop(&sample("a")).unwrap();
        let mut other = sample("b");
        other.change_name = "other-change".into();
        ig.create_loop(&other).unwrap();
        let list = ig.list_loops("smedja").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a");
    }
}
