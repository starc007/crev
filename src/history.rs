use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::output::Finding;

// ── public types ──────────────────────────────────────────────────────────────

pub struct CompletedReview {
    pub repo_path: PathBuf,
    pub commit_hash: Option<String>,
    pub files_changed: usize,
    pub findings: Vec<Finding>,
    pub model_used: String,
    pub elapsed_ms: u64,
}

#[derive(Debug)]
pub struct ReviewSummary {
    pub id: i64,
    pub timestamp: i64,
    pub files_changed: i64,
    pub finding_count: usize,
    pub model_used: String,
}

#[derive(Debug)]
pub struct RecurringPattern {
    pub pattern: String,
    pub file_path: Option<String>,
    pub count: i64,
    pub last_seen: i64,
}

// ── DB path ───────────────────────────────────────────────────────────────────

fn db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .context("Could not determine data directory")?;

    Ok(data_dir.join("crev/history.db"))
}

fn open_db() -> Result<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)
        .with_context(|| format!("Failed to open database at {}", path.display()))?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reviews (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_path      TEXT    NOT NULL,
            commit_hash    TEXT,
            timestamp      INTEGER NOT NULL,
            files_changed  INTEGER,
            findings       TEXT    NOT NULL,
            model_used     TEXT,
            elapsed_ms     INTEGER
        );

        CREATE TABLE IF NOT EXISTS patterns (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_path    TEXT    NOT NULL,
            pattern      TEXT    NOT NULL,
            file_path    TEXT,
            count        INTEGER DEFAULT 1,
            first_seen   INTEGER NOT NULL,
            last_seen    INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

// ── public API ────────────────────────────────────────────────────────────────

pub fn save_review(review: &CompletedReview) -> Result<()> {
    let mut conn = open_db()?;
    let now = unix_now();
    let findings_json = serde_json::to_string(&review.findings)?;
    let repo = review.repo_path.to_string_lossy();

    let tx = conn.transaction()?;

    tx.execute(
        "INSERT INTO reviews (repo_path, commit_hash, timestamp, files_changed, findings, model_used, elapsed_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            repo.as_ref(),
            review.commit_hash,
            now,
            review.files_changed as i64,
            findings_json,
            review.model_used,
            review.elapsed_ms as i64,
        ],
    )?;

    // Update pattern table
    for finding in &review.findings {
        if finding.severity == crate::output::Severity::Lgtm {
            continue;
        }
        let pattern = normalize_pattern(&finding.message);
        let file = finding.file.to_str().map(|s| s.to_string());

        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, count FROM patterns WHERE repo_path = ?1 AND pattern = ?2",
                params![repo.as_ref(), &pattern],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        if let Some((id, count)) = existing {
            tx.execute(
                "UPDATE patterns SET count = ?1, last_seen = ?2 WHERE id = ?3",
                params![count + 1, now, id],
            )?;
        } else {
            tx.execute(
                "INSERT INTO patterns (repo_path, pattern, file_path, count, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                params![repo.as_ref(), &pattern, file, now],
            )?;
        }
    }

    tx.commit()?;

    Ok(())
}

pub fn detect_patterns(repo: &Path) -> Result<Vec<RecurringPattern>> {
    let conn = open_db()?;
    let repo_str = repo.to_string_lossy();

    // Patterns that appear 3+ times in the last 30 reviews for this repo
    let thirty_days_ago = unix_now() - 30 * 24 * 60 * 60;

    let mut stmt = conn.prepare(
        "SELECT pattern, file_path, count, last_seen FROM patterns
         WHERE repo_path = ?1 AND count >= 3 AND last_seen >= ?2
         ORDER BY count DESC",
    )?;

    let patterns = stmt
        .query_map(params![repo_str.as_ref(), thirty_days_ago], |row| {
            Ok(RecurringPattern {
                pattern: row.get(0)?,
                file_path: row.get(1)?,
                count: row.get(2)?,
                last_seen: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(patterns)
}

pub fn get_recent_reviews(repo: &Path, n: usize) -> Result<Vec<ReviewSummary>> {
    let conn = open_db()?;
    let repo_str = repo.to_string_lossy();

    let mut stmt = conn.prepare(
        "SELECT id, timestamp, files_changed, findings, model_used FROM reviews
         WHERE repo_path = ?1
         ORDER BY timestamp DESC
         LIMIT ?2",
    )?;

    let reviews = stmt
        .query_map(params![repo_str.as_ref(), n as i64], |row| {
            let findings_json: String = row.get(3)?;
            let finding_count = serde_json::from_str::<Vec<serde_json::Value>>(&findings_json)
                .map(|v| v.len())
                .unwrap_or(0);
            Ok(ReviewSummary {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                files_changed: row.get(2)?,
                finding_count,
                model_used: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(reviews)
}

pub fn clear_history(repo: &Path) -> Result<()> {
    let conn = open_db()?;
    let repo_str = repo.to_string_lossy();
    conn.execute("DELETE FROM reviews WHERE repo_path = ?1", params![repo_str.as_ref()])?;
    conn.execute("DELETE FROM patterns WHERE repo_path = ?1", params![repo_str.as_ref()])?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn normalize_pattern(msg: &str) -> String {
    // Lowercase, strip line numbers (e.g. ":42"), strip punctuation
    let lower = msg.to_lowercase();
    let no_lines = regex_strip_line_refs(&lower);
    no_lines
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn regex_strip_line_refs(s: &str) -> String {
    // Strip patterns like ":42" or "line 42" from the message
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // skip ":NNN"
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
