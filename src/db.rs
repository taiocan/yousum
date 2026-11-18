// src/db.rs — SQLite persistence layer
// TDD: all tests in the #[cfg(test)] block below were written before implementation.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

// ─── Domain type ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Video {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub upload_date: String,
    pub video_url: String,
    pub status: String,
    pub summary: Option<String>,
    pub importance: Option<i64>,
    pub processed_at: Option<String>,
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Open (or create) the SQLite database at `path` and ensure the schema exists.
pub fn open_db(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open database at '{path}'"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS videos (
            video_id     TEXT PRIMARY KEY,
            title        TEXT,
            channel      TEXT,
            upload_date  TEXT,
            video_url    TEXT,
            status       TEXT,
            summary      TEXT,
            importance   INTEGER,
            processed_at TEXT
        );",
    )
    .context("Failed to create videos table")?;
    Ok(conn)
}

/// Insert a new video record if `video_id` is not already present (idempotent).
pub fn insert_if_new(
    conn: &Connection,
    video_id: &str,
    title: &str,
    channel: &str,
    upload_date: &str,
    video_url: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO videos (video_id, title, channel, upload_date, video_url, status)
         VALUES (?1, ?2, ?3, ?4, ?5, 'new')",
        params![video_id, title, channel, upload_date, video_url],
    )
    .context("insert_if_new failed")?;
    Ok(())
}

/// Return all videos whose status is not 'done' (i.e., new / processing / failed).
pub fn fetch_pending(conn: &Connection) -> Result<Vec<Video>> {
    let mut stmt = conn.prepare(
        "SELECT video_id, title, channel, upload_date, video_url,
                status, summary, importance, processed_at
         FROM videos WHERE status != 'done'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Video {
            video_id: row.get(0)?,
            title: row.get(1)?,
            channel: row.get(2)?,
            upload_date: row.get(3)?,
            video_url: row.get(4)?,
            status: row.get(5)?,
            summary: row.get(6)?,
            importance: row.get(7)?,
            processed_at: row.get(8)?,
        })
    })?;
    rows.map(|r| r.map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()
}

/// Update the `status` column of a single video.
pub fn update_status(conn: &Connection, video_id: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE videos SET status = ?1 WHERE video_id = ?2",
        params![status, video_id],
    )
    .context("update_status failed")?;
    Ok(())
}

/// Persist the AI-generated summary and importance score; mark the video 'done'.
pub fn save_result(
    conn: &Connection,
    video_id: &str,
    summary: &str,
    importance: u8,
    processed_at: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE videos
         SET summary = ?1, importance = ?2, processed_at = ?3, status = 'done'
         WHERE video_id = ?4",
        params![summary, importance as i64, processed_at, video_id],
    )
    .context("save_result failed")?;
    Ok(())
}

/// Fetch all 'done' videos whose `processed_at` is >= `since_iso` (ISO-8601 string).
pub fn fetch_done_since(conn: &Connection, since_iso: &str) -> Result<Vec<Video>> {
    let mut stmt = conn.prepare(
        "SELECT video_id, title, channel, upload_date, video_url,
                status, summary, importance, processed_at
         FROM videos
         WHERE status = 'done' AND processed_at >= ?1
         ORDER BY importance DESC",
    )?;
    let rows = stmt.query_map(params![since_iso], |row| {
        Ok(Video {
            video_id: row.get(0)?,
            title: row.get(1)?,
            channel: row.get(2)?,
            upload_date: row.get(3)?,
            video_url: row.get(4)?,
            status: row.get(5)?,
            summary: row.get(6)?,
            importance: row.get(7)?,
            processed_at: row.get(8)?,
        })
    })?;
    rows.map(|r| r.map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> Connection {
        open_db(":memory:").expect("in-memory db should open")
    }

    // RED → GREEN: schema creation doesn't error
    #[test]
    fn test_schema_created() {
        let conn = in_memory_db();
        // Check the table exists by querying sqlite_master
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='videos'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // RED → GREEN: inserting the same video_id twice does not panic or duplicate
    #[test]
    fn test_insert_if_new_idempotent() {
        let conn = in_memory_db();
        insert_if_new(&conn, "vid1", "Title", "Channel", "2024-01-01", "http://yt/1").unwrap();
        insert_if_new(&conn, "vid1", "Title", "Channel", "2024-01-01", "http://yt/1").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM videos WHERE video_id = 'vid1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "duplicate insert must be silently ignored");
    }

    // RED → GREEN: fetch_pending excludes 'done' rows
    #[test]
    fn test_fetch_pending_excludes_done() {
        let conn = in_memory_db();
        insert_if_new(&conn, "new1", "T", "C", "2024-01-01", "u").unwrap();
        insert_if_new(&conn, "done1", "T", "C", "2024-01-01", "u").unwrap();
        update_status(&conn, "done1", "done").unwrap();

        let pending = fetch_pending(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].video_id, "new1");
    }

    // RED → GREEN: update_status changes the status column
    #[test]
    fn test_update_status() {
        let conn = in_memory_db();
        insert_if_new(&conn, "v1", "T", "C", "2024-01-01", "u").unwrap();
        update_status(&conn, "v1", "processing").unwrap();

        let status: String = conn
            .query_row("SELECT status FROM videos WHERE video_id = 'v1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "processing");
    }

    // RED → GREEN: save_result writes summary, importance, timestamps and marks done
    #[test]
    fn test_save_result() {
        let conn = in_memory_db();
        insert_if_new(&conn, "v2", "T", "C", "2024-01-01", "u").unwrap();
        save_result(&conn, "v2", "Great content", 4, "2024-01-02T08:00:00").unwrap();

        let (summary, importance, status): (String, i64, String) = conn
            .query_row(
                "SELECT summary, importance, status FROM videos WHERE video_id = 'v2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(summary, "Great content");
        assert_eq!(importance, 4);
        assert_eq!(status, "done");
    }

    // RED → GREEN: fetch_done_since respects the time window
    #[test]
    fn test_fetch_done_since() {
        let conn = in_memory_db();
        insert_if_new(&conn, "old", "T", "C", "2023-12-01", "u").unwrap();
        insert_if_new(&conn, "new", "T", "C", "2024-01-15", "u").unwrap();
        save_result(&conn, "old", "summary", 3, "2023-12-01T00:00:00").unwrap();
        save_result(&conn, "new", "summary", 5, "2024-01-15T00:00:00").unwrap();

        let results = fetch_done_since(&conn, "2024-01-01T00:00:00").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].video_id, "new");
    }
}
