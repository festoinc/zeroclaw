//! SQLite persistence for heartbeat task execution history.
//!
//! Mirrors the `cron/store.rs` pattern: fresh connection per call, schema
//! auto-created, output truncated, history pruned to a configurable limit.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_MARKER: &str = "\n...[truncated]";

/// A single heartbeat task execution record.
#[derive(Debug, Clone)]
pub struct HeartbeatRun {
    pub id: i64,
    pub task_text: String,
    pub task_priority: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String, // "ok" or "error"
    pub output: Option<String>,
    pub duration_ms: i64,
}

/// Record a heartbeat task execution and prune old entries.
pub fn record_run(
    workspace_dir: &Path,
    task_text: &str,
    task_priority: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    max_history: u32,
) -> Result<()> {
    let bounded_output = output.map(truncate_output);
    with_connection(workspace_dir, |conn| {
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO heartbeat_runs
                (task_text, task_priority, started_at, finished_at, status, output, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_text,
                task_priority,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output.as_deref(),
                duration_ms,
            ],
        )
        .context("Failed to insert heartbeat run")?;

        let keep = i64::from(max_history.max(1));
        tx.execute(
            "DELETE FROM heartbeat_runs
             WHERE id NOT IN (
                 SELECT id FROM heartbeat_runs
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?1
             )",
            params![keep],
        )
        .context("Failed to prune heartbeat run history")?;

        tx.commit()
            .context("Failed to commit heartbeat run transaction")?;
        Ok(())
    })
}

/// List the most recent heartbeat runs.
pub fn list_runs(workspace_dir: &Path, limit: usize) -> Result<Vec<HeartbeatRun>> {
    with_connection(workspace_dir, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(
            "SELECT id, task_text, task_priority, started_at, finished_at, status, output, duration_ms
             FROM heartbeat_runs
             ORDER BY started_at DESC, id DESC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![lim], |row| {
            Ok(HeartbeatRun {
                id: row.get(0)?,
                task_text: row.get(1)?,
                task_priority: row.get(2)?,
                started_at: parse_rfc3339(&row.get::<_, String>(3)?).map_err(sql_err)?,
                finished_at: parse_rfc3339(&row.get::<_, String>(4)?).map_err(sql_err)?,
                status: row.get(5)?,
                output: row.get(6)?,
                duration_ms: row.get(7)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}

/// Get aggregate stats: (total_runs, total_ok, total_error).
pub fn run_stats(workspace_dir: &Path) -> Result<(u64, u64, u64)> {
    with_connection(workspace_dir, |conn| {
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM heartbeat_runs", [], |r| r.get(0))?;
        let ok: i64 = conn.query_row(
            "SELECT COUNT(*) FROM heartbeat_runs WHERE status = 'ok'",
            [],
            |r| r.get(0),
        )?;
        let err: i64 = conn.query_row(
            "SELECT COUNT(*) FROM heartbeat_runs WHERE status = 'error'",
            [],
            |r| r.get(0),
        )?;
        #[allow(clippy::cast_sign_loss)]
        Ok((total as u64, ok as u64, err as u64))
    })
}

fn db_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("heartbeat").join("history.db")
}

fn with_connection<T>(workspace_dir: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let path = db_path(workspace_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create heartbeat directory: {}", parent.display())
        })?;
    }

    let conn = Connection::open(&path)
        .with_context(|| format!("Failed to open heartbeat history DB: {}", path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;

         CREATE TABLE IF NOT EXISTS heartbeat_runs (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            task_text      TEXT NOT NULL,
            task_priority  TEXT NOT NULL,
            started_at     TEXT NOT NULL,
            finished_at    TEXT NOT NULL,
            status         TEXT NOT NULL,
            output         TEXT,
            duration_ms    INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_hb_runs_started ON heartbeat_runs(started_at);
         CREATE INDEX IF NOT EXISTS idx_hb_runs_task ON heartbeat_runs(task_text);",
    )
    .context("Failed to initialize heartbeat history schema")?;

    f(&conn)
}

fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output.to_string();
    }

    if MAX_OUTPUT_BYTES <= TRUNCATED_MARKER.len() {
        return TRUNCATED_MARKER.to_string();
    }

    let mut cutoff = MAX_OUTPUT_BYTES - TRUNCATED_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_MARKER);
    truncated
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid RFC3339 timestamp in heartbeat DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn sql_err(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    #[test]
    fn record_and_list_runs() {
        let tmp = TempDir::new().unwrap();
        let base = Utc::now();

        for i in 0..3 {
            let start = base + ChronoDuration::seconds(i);
            let end = start + ChronoDuration::milliseconds(100);
            record_run(
                tmp.path(),
                &format!("Task {i}"),
                "medium",
                start,
                end,
                "ok",
                Some("done"),
                100,
                50,
            )
            .unwrap();
        }

        let runs = list_runs(tmp.path(), 10).unwrap();
        assert_eq!(runs.len(), 3);
        // Most recent first
        assert!(runs[0].task_text.contains('2'));
    }

    #[test]
    fn prunes_old_runs() {
        let tmp = TempDir::new().unwrap();
        let base = Utc::now();

        for i in 0..5 {
            let start = base + ChronoDuration::seconds(i);
            let end = start + ChronoDuration::milliseconds(50);
            record_run(
                tmp.path(),
                "Task",
                "high",
                start,
                end,
                "ok",
                None,
                50,
                2, // keep only 2
            )
            .unwrap();
        }

        let runs = list_runs(tmp.path(), 10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn run_stats_counts_correctly() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();

        record_run(tmp.path(), "A", "high", now, now, "ok", None, 10, 50).unwrap();
        record_run(
            tmp.path(),
            "B",
            "low",
            now,
            now,
            "error",
            Some("fail"),
            20,
            50,
        )
        .unwrap();
        record_run(tmp.path(), "C", "medium", now, now, "ok", None, 15, 50).unwrap();

        let (total, ok, err) = run_stats(tmp.path()).unwrap();
        assert_eq!(total, 3);
        assert_eq!(ok, 2);
        assert_eq!(err, 1);
    }

    #[test]
    fn truncates_large_output() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let big = "x".repeat(MAX_OUTPUT_BYTES + 512);

        record_run(
            tmp.path(),
            "T",
            "medium",
            now,
            now,
            "ok",
            Some(&big),
            10,
            50,
        )
        .unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        let stored = runs[0].output.as_deref().unwrap_or_default();
        assert!(stored.ends_with(TRUNCATED_MARKER));
        assert!(stored.len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn list_runs_empty_db() {
        let tmp = TempDir::new().unwrap();
        let runs = list_runs(tmp.path(), 10).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn run_stats_empty_db() {
        let tmp = TempDir::new().unwrap();
        let (total, ok, err) = run_stats(tmp.path()).unwrap();
        assert_eq!(total, 0);
        assert_eq!(ok, 0);
        assert_eq!(err, 0);
    }

    #[test]
    fn record_run_with_none_output() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        record_run(tmp.path(), "T", "medium", now, now, "ok", None, 10, 50).unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].output.is_none());
    }

    #[test]
    fn record_run_max_history_one() {
        let tmp = TempDir::new().unwrap();
        let base = Utc::now();

        for i in 0..5 {
            let start = base + ChronoDuration::seconds(i);
            let end = start + ChronoDuration::milliseconds(10);
            record_run(
                tmp.path(),
                &format!("Task {i}"),
                "medium",
                start,
                end,
                "ok",
                None,
                10,
                1, // keep only 1
            )
            .unwrap();
        }

        let runs = list_runs(tmp.path(), 10).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn truncate_output_exactly_at_limit() {
        let exact = "x".repeat(MAX_OUTPUT_BYTES);
        let result = truncate_output(&exact);
        assert_eq!(result.len(), MAX_OUTPUT_BYTES);
        assert!(!result.contains(TRUNCATED_MARKER));
    }

    #[test]
    fn truncate_output_one_byte_over() {
        let over = "x".repeat(MAX_OUTPUT_BYTES + 1);
        let result = truncate_output(&over);
        assert!(result.ends_with(TRUNCATED_MARKER));
        assert!(result.len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn truncate_output_multibyte_boundary() {
        // Build a string of multi-byte chars that crosses the cutoff boundary
        // Each '€' is 3 bytes in UTF-8
        let euro_count = MAX_OUTPUT_BYTES / 3 + 10;
        let input: String = "€".repeat(euro_count);
        let result = truncate_output(&input);
        // Must be valid UTF-8 and end with the marker
        assert!(result.ends_with(TRUNCATED_MARKER));
        // Verify it's valid UTF-8 (would panic if not)
        let _ = result.as_str();
    }

    // ── truncate_output additional edge cases ────────────────────

    #[test]
    fn truncate_output_empty_string() {
        let result = truncate_output("");
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_output_single_char() {
        let result = truncate_output("x");
        assert_eq!(result, "x");
    }

    #[test]
    fn truncate_output_shorter_than_marker() {
        let result = truncate_output("short");
        assert_eq!(result, "short");
        assert!(!result.contains(TRUNCATED_MARKER));
    }

    #[test]
    fn truncate_output_4byte_utf8_boundary() {
        // '𝄞' is 4 bytes in UTF-8 — test char boundary handling with 4-byte chars
        let count = MAX_OUTPUT_BYTES / 4 + 10;
        let input: String = "𝄞".repeat(count);
        let result = truncate_output(&input);
        assert!(result.ends_with(TRUNCATED_MARKER));
        // Must remain valid UTF-8
        let _ = result.as_str();
    }

    // ── record_run additional edge cases ─────────────────────────

    #[test]
    fn record_run_empty_string_output() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        record_run(tmp.path(), "T", "medium", now, now, "ok", Some(""), 10, 50).unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs.len(), 1);
        // Empty string is stored as Some(""), not None
        assert_eq!(runs[0].output.as_deref(), Some(""));
    }

    #[test]
    fn record_run_special_chars_in_task_text() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let special = "Task with 'quotes', \"doubles\", and SQL: DROP TABLE; --";
        record_run(tmp.path(), special, "high", now, now, "ok", None, 10, 50).unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs[0].task_text, special);
    }

    #[test]
    fn record_run_zero_duration() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        record_run(tmp.path(), "T", "medium", now, now, "ok", None, 0, 50).unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs[0].duration_ms, 0);
    }

    #[test]
    fn record_run_negative_duration() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        // Negative duration shouldn't panic — SQLite stores it fine
        record_run(tmp.path(), "T", "medium", now, now, "ok", None, -1, 50).unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs[0].duration_ms, -1);
    }

    #[test]
    fn record_run_unicode_output() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let unicode_output = "日本語の結果 🎉 €100";
        record_run(
            tmp.path(),
            "T",
            "medium",
            now,
            now,
            "ok",
            Some(unicode_output),
            10,
            50,
        )
        .unwrap();

        let runs = list_runs(tmp.path(), 1).unwrap();
        assert_eq!(runs[0].output.as_deref(), Some(unicode_output));
    }

    // ── list_runs additional edge cases ──────────────────────────

    #[test]
    fn list_runs_limit_clamped_to_one() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        for i in 0..3 {
            let start = now + ChronoDuration::seconds(i);
            record_run(
                tmp.path(),
                &format!("T{i}"),
                "medium",
                start,
                start,
                "ok",
                None,
                10,
                50,
            )
            .unwrap();
        }
        // limit=0 is clamped to 1
        let runs = list_runs(tmp.path(), 0).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn list_runs_ordering_same_timestamp() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        // Insert multiple runs with the same timestamp
        for i in 0..3 {
            record_run(
                tmp.path(),
                &format!("Task {i}"),
                "medium",
                now,
                now,
                "ok",
                None,
                10,
                50,
            )
            .unwrap();
        }
        let runs = list_runs(tmp.path(), 10).unwrap();
        assert_eq!(runs.len(), 3);
        // With same timestamp, ordered by id DESC → Task 2 first
        assert!(runs[0].task_text.contains('2'));
        assert!(runs[2].task_text.contains('0'));
    }

    // ── run_stats additional edge cases ──────────────────────────

    #[test]
    fn run_stats_only_errors() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        record_run(tmp.path(), "A", "high", now, now, "error", None, 10, 50).unwrap();
        record_run(tmp.path(), "B", "high", now, now, "error", None, 10, 50).unwrap();

        let (total, ok, err) = run_stats(tmp.path()).unwrap();
        assert_eq!(total, 2);
        assert_eq!(ok, 0);
        assert_eq!(err, 2);
    }

    #[test]
    fn run_stats_only_ok() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        record_run(tmp.path(), "A", "high", now, now, "ok", None, 10, 50).unwrap();
        record_run(tmp.path(), "B", "high", now, now, "ok", None, 10, 50).unwrap();

        let (total, ok, err) = run_stats(tmp.path()).unwrap();
        assert_eq!(total, 2);
        assert_eq!(ok, 2);
        assert_eq!(err, 0);
    }

    #[test]
    fn record_run_max_history_keeps_most_recent() {
        let tmp = TempDir::new().unwrap();
        let base = Utc::now();

        for i in 0..5 {
            let start = base + ChronoDuration::seconds(i);
            let end = start + ChronoDuration::milliseconds(10);
            record_run(
                tmp.path(),
                &format!("Task {i}"),
                "medium",
                start,
                end,
                "ok",
                None,
                10,
                3,
            )
            .unwrap();
        }

        let runs = list_runs(tmp.path(), 10).unwrap();
        assert_eq!(runs.len(), 3);
        // Most recent 3 should be Task 4, 3, 2
        assert!(runs[0].task_text.contains('4'));
        assert!(runs[1].task_text.contains('3'));
        assert!(runs[2].task_text.contains('2'));
    }
}
