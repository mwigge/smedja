use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use smedja_types::{Microdollars, Timestamp};

use super::tier::RollupTier;
use crate::error::IngotError;

/// One aggregated `(tier, bucket_start, runner)` row.
///
/// Cost is kept as exact integer [`Microdollars`]; conversion to USD happens
/// only at the display boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsBucket {
    /// The tier this bucket belongs to.
    pub tier: RollupTier,
    /// Bucket start (microseconds since the Unix epoch), per [`RollupTier::bucket_start`].
    pub bucket_start: Timestamp,
    /// Runner name (e.g. `"claude"`, `"local"`).
    pub runner: String,
    /// Number of turns (cost-ledger rows) in this bucket.
    pub turns: i64,
    /// Sum of input tokens.
    pub input_tok: i64,
    /// Sum of output tokens.
    pub output_tok: i64,
    /// Exact total cost (microdollars).
    pub cost_usd: Microdollars,
    /// Number of `audit_events` rows with `status = 'error'` in this bucket.
    pub error_count: i64,
}

/// A mutable per-`(bucket_start, runner)` accumulator used while merging the two
/// grouped result sets.
#[derive(Default)]
struct Accumulator {
    turns: i64,
    input_tok: i64,
    output_tok: i64,
    cost_micros: i64,
    error_count: i64,
}

/// Computes rollup buckets for `tier` over `[since, until)` from source rows.
///
/// Tokens, cost, and turns are summed from `cost_ledger`; error counts are
/// counted from `audit_events` with `status = 'error'`. Both source timestamps
/// (`cost_ledger.created_at`, `audit_events.ts`) are bucketed with the same
/// [`RollupTier::bucket_start`], so identical instants land in identical
/// buckets. The merge is keyed on `(bucket_start, runner)`; results are ordered
/// by `bucket_start` then `runner`.
///
/// The `until` bound is exclusive. The `runner` dimension for audit errors is
/// taken from `agent_name` when present, falling back to `actor`, mirroring how
/// the conversation rollups attribute agents.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if either source query fails.
pub(crate) fn compute(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<MetricsBucket>, IngotError> {
    let mut acc: BTreeMap<(i64, String), Accumulator> = BTreeMap::new();

    // ── cost_ledger: tokens / cost / turns per (bucket, runner) ──────────────
    let mut cost_stmt = conn.prepare(
        "SELECT runner, input_tok, output_tok, cost_usd, created_at \
         FROM cost_ledger \
         WHERE created_at >= ?1 AND created_at < ?2",
    )?;
    let cost_rows = cost_stmt.query_map(
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| {
            let runner: String = row.get(0)?;
            let input_tok: i64 = row.get(1)?;
            let output_tok: i64 = row.get(2)?;
            let cost_micros = crate::read_micros(row, 3)?;
            let created_at = crate::read_micros(row, 4)?;
            Ok((runner, input_tok, output_tok, cost_micros, created_at))
        },
    )?;
    for row in cost_rows {
        let (runner, input_tok, output_tok, cost_micros, created_at) = row?;
        let bucket = tier.bucket_start(created_at);
        let entry = acc.entry((bucket, runner)).or_default();
        entry.turns += 1;
        entry.input_tok += input_tok;
        entry.output_tok += output_tok;
        entry.cost_micros = entry.cost_micros.saturating_add(cost_micros);
    }

    // ── audit_events: error counts per (bucket, runner) ──────────────────────
    let mut err_stmt = conn.prepare(
        "SELECT COALESCE(agent_name, actor) AS runner, ts \
         FROM audit_events \
         WHERE status = 'error' AND ts >= ?1 AND ts < ?2",
    )?;
    let err_rows = err_stmt.query_map(
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| {
            let runner: String = row.get(0)?;
            let ts = crate::read_micros(row, 1)?;
            Ok((runner, ts))
        },
    )?;
    for row in err_rows {
        let (runner, ts) = row?;
        let bucket = tier.bucket_start(ts);
        let entry = acc.entry((bucket, runner)).or_default();
        entry.error_count += 1;
    }

    // BTreeMap iteration is ordered by (bucket_start, runner) — the required order.
    let buckets = acc
        .into_iter()
        .map(|((bucket_start, runner), a)| MetricsBucket {
            tier,
            bucket_start: Timestamp::from_micros(bucket_start),
            runner,
            turns: a.turns,
            input_tok: a.input_tok,
            output_tok: a.output_tok,
            cost_usd: Microdollars::from_micros(a.cost_micros),
            error_count: a.error_count,
        })
        .collect();
    Ok(buckets)
}

/// Upserts the computed buckets for `tier` over `[since, until)` into
/// `metrics_rollups`, keyed on `(tier, bucket_start, runner)`.
///
/// Re-running with the same inputs reproduces identical rows (idempotent): the
/// `ON CONFLICT … DO UPDATE` overwrites every aggregate column with the freshly
/// computed value rather than accumulating.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the source queries or the upsert fail.
pub(crate) fn materialise(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<MetricsBucket>, IngotError> {
    let buckets = compute(conn, tier, since, until)?;
    for b in &buckets {
        conn.execute(
            "INSERT INTO metrics_rollups \
             (tier, bucket_start, runner, turns, input_tok, output_tok, cost_usd, error_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(tier, bucket_start, runner) DO UPDATE SET \
               turns       = excluded.turns, \
               input_tok   = excluded.input_tok, \
               output_tok  = excluded.output_tok, \
               cost_usd    = excluded.cost_usd, \
               error_count = excluded.error_count",
            rusqlite::params![
                b.tier.as_str(),
                b.bucket_start.as_micros(),
                b.runner,
                b.turns,
                b.input_tok,
                b.output_tok,
                b.cost_usd.as_micros(),
                b.error_count,
            ],
        )?;
    }
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::super::tier::MICROS_PER_SEC;
    use super::*;
    use crate::{AuditEvent, CostEntry, Ingot};
    use chrono::{TimeZone as _, Utc};
    use uuid::Uuid;

    /// Returns the micros for a UTC `Y-M-D h:m:s` instant.
    fn micros(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid instant")
            .timestamp()
            * MICROS_PER_SEC
    }

    fn cost_entry(runner: &str, cost_usd: f64, in_tok: i64, out_tok: i64, at: i64) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            runner: runner.to_owned(),
            model: "m".to_owned(),
            input_tok: in_tok,
            output_tok: out_tok,
            cost_usd: Microdollars::from_usd_f64(cost_usd),
            created_at: Timestamp::from_micros(at),
            change_name: None,
        }
    }

    fn error_event(runner: &str, at: i64) -> AuditEvent {
        AuditEvent {
            id: Uuid::new_v4(),
            ts: Timestamp::from_micros(at),
            session_id: "s1".to_owned(),
            action_type: "llm".to_owned(),
            actor: runner.to_owned(),
            status: Some("error".to_owned()),
            ..AuditEvent::default()
        }
    }

    #[test]
    fn migration_22_creates_metrics_rollups_table() {
        let ingot = Ingot::open_in_memory().unwrap();
        let count: i64 = ingot
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'metrics_rollups'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "metrics_rollups table must exist after migration");
    }

    #[test]
    fn daily_rollup_groups_by_day_and_runner() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day1 = micros(2026, 1, 1, 9, 0, 0);
        let day2 = micros(2026, 1, 2, 9, 0, 0);
        // claude: two entries on day1, one on day2; local: one on day1.
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day1))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("claude", 0.02, 200, 60, day1 + 1_000_000))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("claude", 0.03, 300, 70, day2))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("local", 0.04, 400, 80, day1))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 3, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();

        // (day1, claude), (day1, local), (day2, claude) — ordered by bucket then runner.
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].runner, "claude");
        assert_eq!(
            buckets[0].bucket_start.as_micros(),
            micros(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(buckets[0].turns, 2);
        assert_eq!(buckets[0].input_tok, 300);
        assert_eq!(buckets[0].output_tok, 110);
        assert_eq!(buckets[0].cost_usd, Microdollars::from_micros(30_000));
        assert_eq!(buckets[1].runner, "local");
        assert_eq!(
            buckets[1].bucket_start.as_micros(),
            micros(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(buckets[2].runner, "claude");
        assert_eq!(
            buckets[2].bucket_start.as_micros(),
            micros(2026, 1, 2, 0, 0, 0)
        );
        assert_eq!(buckets[2].turns, 1);
    }

    #[test]
    fn error_events_populate_error_count() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_audit_event(&error_event("claude", day))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", day + 1_000_000))
            .unwrap();
        // A non-error event must not count.
        let mut ok = error_event("claude", day);
        ok.status = Some("ok".to_owned());
        ingot.insert_audit_event(&ok).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].runner, "claude");
        assert_eq!(buckets[0].error_count, 2);
        assert_eq!(buckets[0].turns, 0, "no cost rows means zero turns");
    }

    #[test]
    fn cost_and_error_at_same_instant_share_one_bucket() {
        let ingot = Ingot::open_in_memory().unwrap();
        let at = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.05, 100, 50, at))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", at))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 1, "one merged (bucket, runner) row");
        assert_eq!(buckets[0].turns, 1);
        assert_eq!(buckets[0].cost_usd, Microdollars::from_micros(50_000));
        assert_eq!(buckets[0].error_count, 1);
    }

    #[test]
    fn agent_name_is_preferred_over_actor_for_error_runner() {
        let ingot = Ingot::open_in_memory().unwrap();
        let at = micros(2026, 1, 1, 9, 0, 0);
        let mut ev = error_event("actor-fallback", at);
        ev.agent_name = Some("coder-rust".to_owned());
        ingot.insert_audit_event(&ev).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].runner, "coder-rust");
    }

    #[test]
    fn materialise_then_read_equals_on_read() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", day))
            .unwrap();

        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let on_read = ingot
            .metrics_rollup(RollupTier::Daily, Timestamp::from_micros(0), until)
            .unwrap();
        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();

        // Read the persisted rows directly and compare to on-read.
        let mut stmt = ingot
            .conn
            .prepare(
                "SELECT bucket_start, runner, turns, input_tok, output_tok, cost_usd, error_count \
                 FROM metrics_rollups WHERE tier = 'daily' \
                 ORDER BY bucket_start, runner",
            )
            .unwrap();
        let stored: Vec<MetricsBucket> = stmt
            .query_map([], |row| {
                Ok(MetricsBucket {
                    tier: RollupTier::Daily,
                    bucket_start: Timestamp::from_micros(crate::read_micros(row, 0)?),
                    runner: row.get(1)?,
                    turns: row.get(2)?,
                    input_tok: row.get(3)?,
                    output_tok: row.get(4)?,
                    cost_usd: Microdollars::from_micros(crate::read_micros(row, 5)?),
                    error_count: row.get(6)?,
                })
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stored, on_read);
    }

    #[test]
    fn materialise_is_idempotent() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day))
            .unwrap();
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));

        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();
        let count_after_first: i64 = ingot
            .conn
            .query_row("SELECT COUNT(*) FROM metrics_rollups", [], |row| row.get(0))
            .unwrap();
        let cost_after_first: i64 = ingot
            .conn
            .query_row(
                "SELECT cost_usd FROM metrics_rollups WHERE runner = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();
        let count_after_second: i64 = ingot
            .conn
            .query_row("SELECT COUNT(*) FROM metrics_rollups", [], |row| row.get(0))
            .unwrap();
        let cost_after_second: i64 = ingot
            .conn
            .query_row(
                "SELECT cost_usd FROM metrics_rollups WHERE runner = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count_after_first, count_after_second, "row count unchanged");
        assert_eq!(cost_after_first, cost_after_second, "values unchanged");
    }
}
