# YouTube Video Summarizer & Digest System – Implementation Specification

## Overview
A Rust service that monitors YouTube playlists, downloads subtitles, generates summaries with importance ranking using Gemini, and emails a daily digest. Designed for robustness with a processing state machine and minimal external dependencies.

---

## Database Schema (SQLite)
```sql
CREATE TABLE videos (
    video_id TEXT PRIMARY KEY,
    title TEXT,
    channel TEXT,
    upload_date TEXT,
    video_url TEXT,
    status TEXT,              -- 'new', 'processing', 'done', 'failed'
    summary TEXT,
    importance INTEGER,       -- 1–5 scale, added later
    processed_at TEXT         -- optional timestamp
);
```

---

## Processing Workflow

### 1. Discovery (run on cron trigger)
- Fetch videos from configured YouTube playlists.
- For each video not already in DB (`video_id`), insert with `status = 'new'`.

### 2. Retry / Process Pending
- `SELECT * FROM videos WHERE status != 'done'` (includes `new`, `processing`, `failed`).
- For each video:
  - Update `status = 'processing'`.
  - Attempt processing:
    - Download subtitles (YouTube).
    - Clean transcript (remove timestamps, filler, normalize whitespace).
    - If transcript long, split into chunks (~4000 chars) and summarize hierarchically.
    - Call Gemini with prompt (see below) to get summary + importance.
    - On success: store `summary` and `importance`, set `status = 'done'`.
    - On error: set `status = 'failed'`, log error for reporting.

### 3. Digest Generation
- After processing all pending, fetch all videos with `status = 'done'` from the current run (or time window).
- Sort by `importance DESC`.
- Format email with grouped importance levels:
  - **⭐ Must Watch (5)**
  - **🔧 Highly Valuable (4)**
  - **💡 Useful Ideas (3)**
  - **Optional / Commentary (1–2)**
- Optionally include daily stats: total videos, counts per importance.
- Send email via SMTP (configured).

### 4. Scheduling
- Use **cron** (e.g., daily at 7 AM) to invoke the binary (e.g., `check-now`). No internal scheduler.

---

## Transcript Cleaning & Chunking

### Clean Transcript
```rust
fn clean_transcript(raw: &str) -> String {
    raw.lines()
        .map(|line| line.trim())
        .map(|line| line.replace(|c: char| c.is_numeric() && line.contains(":"), "")) // remove timestamps
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
```

### Chunking (if needed)
```rust
fn split_chunks(text: &str, chunk_size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + chunk_size).min(text.len());
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}
```
- Recommended chunk size: ~4000 characters.
- For each chunk, call Gemini to summarize, then combine chunk summaries into a final summary with a second Gemini call.

---

## Gemini Prompt
```
You are analyzing a YouTube video transcript for an AI software developer focused on monetizing and professionalizing their projects.

Video: "{title}"
URL: {url}

Produce a structured analysis in EXACTLY this format:

GRADE: X

KEY MESSAGES:
- [core message — include specific names, numbers, tools, repo URLs if mentioned]
- ... (up to 6 bullets)

SURPRISING FACTS:
- [counterintuitive finding, unexpected number, or insight that challenges common assumptions]
- ... (2–4 bullets; omit this section if there are no genuinely surprising facts)

STEPS: (include ONLY if video is a tutorial, tool walkthrough, or implementation guide — otherwise omit this entire section)
1. [first command or action]
2. ...

WHY IT MATTERS:
- [who this is for and in what situation it helps]
- ... (3–5 statements total)

LINKS:
Video: {url}
Article: [URL only if explicitly mentioned or shown in the transcript — otherwise omit this line entirely]

---
GRADE CRITERIA (assign one integer 1–5):
5 = Highly structured, data-rich, directly actionable for monetizing/professionalizing AI software
4 = Clear directions, quantifiable data, strong relevance to AI dev professionalism
3 = Useful knowledge with some specifics, moderate actionability
2 = General overview or opinion, few concrete facts
1 = Mostly filler or commentary, not actionable
```
- Include video title and URL as context in the prompt.
- STEPS section is conditional — only emit if the video is a tutorial/walkthrough/implementation guide.
- SURPRISING FACTS section is conditional — only emit if genuinely surprising facts exist.
- Article link in LINKS is conditional — only emit if a URL was explicitly mentioned in the transcript.

---

## Error Handling & Retries
- If processing fails (network, API error, etc.), set status to `failed`. Next run will retry it.
- If a crash occurs during processing (status left as `processing`), the next run will also retry it because `status != 'done'`.
- No complex retry logic; rely on periodic cron retries.

---

## File Structure
```
src/
  main.rs          # entry, orchestration, email
  youtube.rs       # YouTube API client, subtitle download
  ai.rs            # Gemini client, cleaning, chunking, prompt handling
  db.rs            # SQLite operations
```
- Configuration (API keys, playlist IDs, email settings) via environment variables or a simple config file (e.g., TOML).

---

## Execution Flow (main.rs)
```
load_config()
discover_new_videos()          # insert new records
let pending = fetch_pending()
for video in pending {
    update_status(video.id, "processing")
    match process_video(video) {
        Ok((summary, importance)) => {
            save_summary_and_importance(video.id, summary, importance)
            update_status(video.id, "done")
        }
        Err(e) => {
            update_status(video.id, "failed")
            record_error(video, e)
        }
    }
}
let digest = build_digest()    # fetch done videos, sort, format
send_email(digest)
```

---

## Configuration
- `YOUTUBE_API_KEY`
- `GEMINI_API_KEY`
- `PLAYLIST_IDS` (comma-separated)
- `SMTP_*` (server, port, username, password, from, to)
- `CRON_SCHEDULE` (external, e.g., in crontab)

---

## Estimated Code Size
| Module | LOC  |
|--------|------|
| main   | ~200 |
| youtube| ~150 |
| ai     | ~200 |
| db     | ~80  |
| **Total** | **~630** |

---

## Optional Enhancements (not required but noted)
- Daily stats in email.
- Use of environment variables for secrets.
- Simple `println!` logging; no heavy tracing.

---

This specification captures all necessary details for an AI agent to implement the solution.