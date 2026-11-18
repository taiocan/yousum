// src/main.rs — Orchestration, config loading, digest building, and email sending
// TDD: tests in #[cfg(test)] were written before implementation.

mod ai;
mod db;
mod youtube;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use db::Video;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

// ─── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Config {
    pub yt_api_key: String,
    pub gemini_api_key: String,
    pub playlist_ids: Vec<String>,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_pass: String,
    pub smtp_from: String,
    pub smtp_to: String,
    pub db_path: String,
    pub digest_hours: i64,
}

/// Load configuration exclusively from environment variables.
/// Panics with a clear message when a required variable is absent.
pub fn load_config() -> Config {
    fn require(key: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| {
            panic!("Required environment variable '{key}' is not set. See .env.example for reference.")
        })
    }
    fn optional(key: &str, default: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| default.to_string())
    }

    let playlist_ids = require("PLAYLIST_IDS")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Config {
        yt_api_key: require("YOUTUBE_API_KEY"),
        gemini_api_key: require("GEMINI_API_KEY"),
        playlist_ids,
        smtp_host: require("SMTP_HOST"),
        smtp_port: optional("SMTP_PORT", "587")
            .parse()
            .expect("SMTP_PORT must be a valid port number"),
        smtp_user: require("SMTP_USERNAME"),
        smtp_pass: require("SMTP_PASSWORD"),
        smtp_from: require("SMTP_FROM"),
        smtp_to: require("SMTP_TO"),
        db_path: optional("DB_PATH", "yousum.db"),
        digest_hours: optional("DIGEST_HOURS", "24")
            .parse()
            .expect("DIGEST_HOURS must be an integer"),
    }
}

// ─── Digest builder ──────────────────────────────────────────────────────────

/// Build an HTML email body from a list of processed videos, grouped by importance.
pub fn build_digest(videos: &[Video]) -> String {
    if videos.is_empty() {
        return "<p>No new videos were summarised in this digest period.</p>".to_string();
    }

    let tier = |min: u8, max: u8| -> Vec<&Video> {
        videos
            .iter()
            .filter(|v| {
                v.importance
                    .map(|i| i >= min as i64 && i <= max as i64)
                    .unwrap_or(false)
            })
            .collect()
    };

    let mut html = String::from(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { font-family: sans-serif; max-width: 700px; margin: auto; color: #222; }
    h1 { color: #333; }
    h2 { border-bottom: 2px solid #eee; padding-bottom: 4px; }
    h3 { font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.04em; color: #555; margin: 14px 0 4px; }
    .video { margin-bottom: 28px; border-left: 3px solid #e0e0e0; padding-left: 12px; }
    .video a { font-size: 1.1em; font-weight: bold; color: #1a73e8; text-decoration: none; }
    .video a:hover { text-decoration: underline; }
    .channel { color: #888; font-size: 0.9em; margin-bottom: 6px; }
    .summary ul, .summary ol { margin: 2px 0 8px; padding-left: 20px; }
    .summary li { margin-bottom: 3px; }
    .summary a { color: #1a73e8; }
    .stats { background: #f5f5f5; padding: 12px; border-radius: 6px; margin-top: 30px; }
  </style>
</head>
<body>
<h1>🎬 YouSum Daily Digest</h1>
"#,
    );

    let total = videos.len();

    let sections: &[(&str, u8, u8)] = &[
        ("⭐ Monetize / Professionalize (Grade 5)", 5, 5),
        ("🔧 High Actionability (Grade 4)", 4, 4),
        ("💡 Useful with Caveats (Grade 3)", 3, 3),
        ("📝 Background / Commentary (Grade 1–2)", 1, 2),
    ];

    for (label, min, max) in sections {
        let group = tier(*min, *max);
        if group.is_empty() {
            continue;
        }
        html.push_str(&format!("<h2>{label}</h2>\n"));
        for v in group {
            let summary = v.summary.as_deref().unwrap_or("No summary available.");
            let importance = v.importance.unwrap_or(0);
            html.push_str(&format!(
                r#"<div class="video">
  <a href="{url}">{title}</a>
  <div class="channel">{channel} &middot; {date} &middot; grade {imp}/5</div>
  <div class="summary">{summary_html}</div>
</div>
"#,
                url = v.video_url,
                title = html_escape(&v.title),
                channel = html_escape(&v.channel),
                date = &v.upload_date[..10.min(v.upload_date.len())],
                imp = importance,
                summary_html = summary_to_html(summary),
            ));
        }
    }

    // Daily stats
    html.push_str(&format!(
        r#"<div class="stats">
  <strong>Stats:</strong> {total} video(s) summarised this period.
</div>
</body>
</html>"#
    ));

    html
}

/// Convert a structured summary (new format) or plain text (old format) to HTML.
fn summary_to_html(raw: &str) -> String {
    if !raw.contains("KEY MESSAGES:") {
        // Old format: wrap in paragraphs
        return format!(
            "<p style=\"margin:0\">{}</p>",
            html_escape(raw)
                .replace("\n\n", "</p><p style=\"margin:6px 0\">")
                .replace('\n', "<br>")
        );
    }

    let section_defs: &[(&str, &str, bool)] = &[
        ("KEY MESSAGES:", "Key Messages", false),
        ("SURPRISING FACTS:", "Surprising Facts", false),
        ("STEPS:", "Steps", true),
        ("WHY IT MATTERS:", "Why It Matters", false),
        ("LINKS:", "Links", false),
    ];

    // Find which sections exist and their positions
    let mut found: Vec<(usize, &str, &str, bool)> = section_defs
        .iter()
        .filter_map(|(hdr, lbl, ord)| raw.find(hdr).map(|p| (p, *hdr, *lbl, *ord)))
        .collect();
    found.sort_by_key(|f| f.0);

    let mut html = String::new();

    for i in 0..found.len() {
        let (pos, hdr, label, ordered) = found[i];
        let content_start = pos + hdr.len();
        let content_end = if i + 1 < found.len() { found[i + 1].0 } else { raw.len() };
        let content = raw[content_start..content_end].trim();

        if content.is_empty() {
            continue;
        }

        html.push_str(&format!("<h3>{label}</h3>\n"));

        if label == "Links" {
            html.push_str("<div style=\"font-size:0.9em\">\n");
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(idx) = line.find("http") {
                    let prefix = line[..idx].trim_end_matches(':').trim();
                    let url = line[idx..].trim();
                    if prefix.is_empty() {
                        html.push_str(&format!("<div><a href=\"{url}\">{url}</a></div>\n", url = html_escape(url)));
                    } else {
                        html.push_str(&format!(
                            "<div><strong>{}</strong>: <a href=\"{url}\">{url}</a></div>\n",
                            html_escape(prefix),
                            url = html_escape(url)
                        ));
                    }
                } else {
                    html.push_str(&format!("<div>{}</div>\n", html_escape(line)));
                }
            }
            html.push_str("</div>\n");
        } else if ordered {
            html.push_str("<ol style=\"margin:0;padding-left:20px\">\n");
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // Strip "1. " numeric prefix if present
                let text = line
                    .find(". ")
                    .filter(|&dot| line[..dot].chars().all(|c| c.is_ascii_digit()))
                    .map(|dot| &line[dot + 2..])
                    .unwrap_or(line);
                html.push_str(&format!("<li>{}</li>\n", html_escape(text)));
            }
            html.push_str("</ol>\n");
        } else {
            html.push_str("<ul style=\"margin:0;padding-left:20px\">\n");
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let text = if line.starts_with("- ") || line.starts_with("* ") {
                    &line[2..]
                } else {
                    line
                };
                html.push_str(&format!("<li>{}</li>\n", html_escape(text)));
            }
            html.push_str("</ul>\n");
        }
    }

    html
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─── Email sender ─────────────────────────────────────────────────────────────

pub fn send_email(cfg: &Config, subject: &str, html_body: &str) -> Result<()> {
    let email = Message::builder()
        .from(cfg.smtp_from.parse().context("Invalid SMTP_FROM address")?)
        .to(cfg.smtp_to.parse().context("Invalid SMTP_TO address")?)
        .subject(subject)
        .header(ContentType::TEXT_HTML)
        .body(html_body.to_string())
        .context("Failed to build email message")?;

    let creds = Credentials::new(cfg.smtp_user.clone(), cfg.smtp_pass.clone());

    let mailer = SmtpTransport::starttls_relay(&cfg.smtp_host)
        .context("Failed to create SMTP transport")?
        .port(cfg.smtp_port)
        .timeout(Some(std::time::Duration::from_secs(60)))
        .credentials(creds)
        .build();

    mailer.send(&email).context("Failed to send email")?;
    Ok(())
}

// ─── Main orchestration ───────────────────────────────────────────────────────

fn main() -> Result<()> {
    // Load .env in development (no-op if file absent)
    let _ = dotenvy::dotenv();

    let cfg = load_config();
    let conn = db::open_db(&cfg.db_path)?;

    // 1. Discovery
    for playlist_id in &cfg.playlist_ids {
        println!("Discovering playlist: {playlist_id}");
        let videos = youtube::discover_playlist(&cfg.yt_api_key, playlist_id)
            .with_context(|| format!("Failed to discover playlist {playlist_id}"))?;

        for v in &videos {
            db::insert_if_new(&conn, &v.video_id, &v.title, &v.channel, &v.upload_date, &v.video_url)?;
        }
        println!("  → {} video(s) found", videos.len());
    }

    // 2. Process pending
    let pending = db::fetch_pending(&conn)?;
    println!("Processing {} pending video(s)…", pending.len());

    let mut errors: Vec<(String, String)> = Vec::new();

    for video in &pending {
        // Politeness delay: wait 3 seconds between videos to avoid YouTube/Gemini rate limits
        std::thread::sleep(std::time::Duration::from_secs(3));

        db::update_status(&conn, &video.video_id, "processing")?;
        println!("  Processing: {} ({})", video.title, video.video_id);

        match process_video(&cfg, video) {
            Ok((summary, importance)) => {
                let now = Utc::now().to_rfc3339();
                db::save_result(&conn, &video.video_id, &summary, importance, &now)?;
                println!("    ✓ importance={importance}");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("Permanent error") || msg.contains("Could not read subtitle file") {
                    println!("    ⚠ Marking as skipped (permanent failure: {msg})");
                    db::save_result(&conn, &video.video_id, &format!("(Skipped: {msg})"), 0, &Utc::now().to_rfc3339())?;
                } else {
                    db::update_status(&conn, &video.video_id, "failed")?;
                    println!("    ✗ {msg}");
                    errors.push((video.video_id.clone(), msg));
                }
            }
        }
    }

    // 3. Digest
    let since = (Utc::now() - Duration::hours(cfg.digest_hours)).to_rfc3339();
    let done_videos = db::fetch_done_since(&conn, &since)?;
    println!("Found {} video(s) for the digest.", done_videos.len());

    if done_videos.is_empty() && errors.is_empty() {
        println!("Nothing to report — skipping email.");
        return Ok(());
    }

    // Split into chunks of 10 videos (even smaller) to avoid SMTP timeouts
    let chunk_size = 10;
    let chunks: Vec<_> = done_videos.chunks(chunk_size).collect();
    let total_parts = chunks.len();

    if total_parts == 0 && !errors.is_empty() {
        // Only errors to report
        let mut error_body = String::from("<h1>❌ YouSum Error Report</h1><ul>");
        for (id, err) in &errors {
            error_body.push_str(&format!("<li><b>{id}</b>: {err}</li>"));
        }
        error_body.push_str("</ul>");
        send_email(&cfg, "Error Report", &error_body).context("Error email send failed")?;
    } else {
        for (i, chunk) in chunks.iter().enumerate() {
            let part_num = i + 1;
            let subject = if total_parts > 1 {
                format!("YouSum Daily Digest (Part {}/{})", part_num, total_parts)
            } else {
                "YouSum Daily Digest".to_string()
            };

            println!("Building digest part {}/{} ({} videos)…", part_num, total_parts, chunk.len());
            let digest = build_digest(chunk);
            send_email(&cfg, &subject, &digest).context(format!("Email send failed for part {part_num}"))?;
            println!("  ✓ Part {part_num} sent.");
            
            // 5 second pause to be very gentle on SMTP server
            if part_num < total_parts {
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }

    if !errors.is_empty() {
        println!("\n{} video(s) failed processing:", errors.len());
        for (id, err) in &errors {
            println!("  {id}: {err}");
        }
    }

    Ok(())
}

fn process_video(cfg: &Config, video: &Video) -> Result<(String, u8)> {
    let raw = youtube::download_subtitles(&video.video_id, &video.video_url)
        .with_context(|| format!("Subtitle download failed for {}", video.video_id))?;
    ai::summarize(&cfg.gemini_api_key, &video.title, &video.video_url, &raw)
        .with_context(|| format!("Summarisation failed for {}", video.video_id))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_video(id: &str, title: &str, importance: i64, processed_at: &str) -> Video {
        Video {
            video_id: id.to_string(),
            title: title.to_string(),
            channel: "TestChannel".to_string(),
            upload_date: "2024-01-01T00:00:00Z".to_string(),
            video_url: format!("https://youtu.be/{id}"),
            status: "done".to_string(),
            summary: Some(format!("Summary of {title}")),
            importance: Some(importance),
            processed_at: Some(processed_at.to_string()),
        }
    }

    // RED → GREEN: missing required env var should panic with a clear message
    #[test]
    #[should_panic(expected = "Required environment variable")]
    fn test_load_config_missing_key() {
        // Clear all relevant vars to guarantee clean state
        for key in &[
            "YOUTUBE_API_KEY", "GEMINI_API_KEY", "PLAYLIST_IDS",
            "SMTP_HOST", "SMTP_USERNAME", "SMTP_PASSWORD", "SMTP_FROM", "SMTP_TO",
        ] {
            std::env::remove_var(key);
        }
        load_config();
    }

    // RED → GREEN: optional vars have correct defaults
    #[test]
    fn test_load_config_defaults() {
        // Set all required vars
        std::env::set_var("YOUTUBE_API_KEY", "yk");
        std::env::set_var("GEMINI_API_KEY", "gk");
        std::env::set_var("PLAYLIST_IDS", "PL1");
        std::env::set_var("SMTP_HOST", "smtp.example.com");
        std::env::set_var("SMTP_USERNAME", "u");
        std::env::set_var("SMTP_PASSWORD", "p");
        std::env::set_var("SMTP_FROM", "a@b.com");
        std::env::set_var("SMTP_TO", "c@d.com");
        // Ensure optional vars are NOT set
        std::env::remove_var("DB_PATH");
        std::env::remove_var("DIGEST_HOURS");
        std::env::remove_var("SMTP_PORT");

        let cfg = load_config();

        assert_eq!(cfg.db_path, "yousum.db");
        assert_eq!(cfg.digest_hours, 24);
        assert_eq!(cfg.smtp_port, 587);

        // Cleanup
        for key in &[
            "YOUTUBE_API_KEY", "GEMINI_API_KEY", "PLAYLIST_IDS",
            "SMTP_HOST", "SMTP_USERNAME", "SMTP_PASSWORD", "SMTP_FROM", "SMTP_TO",
        ] {
            std::env::remove_var(key);
        }
    }

    // RED → GREEN: digest sorts videos by importance DESC
    #[test]
    fn test_build_digest_sorted_by_importance() {
        let videos = vec![
            make_video("v1", "Low Video", 1, "2024-01-01T00:00:00Z"),
            make_video("v2", "Top Video", 5, "2024-01-01T00:00:00Z"),
            make_video("v3", "Mid Video", 3, "2024-01-01T00:00:00Z"),
        ];
        let html = build_digest(&videos);
        let pos5 = html.find("Top Video").expect("Top Video must be in digest");
        let pos3 = html.find("Mid Video").expect("Mid Video must be in digest");
        let pos1 = html.find("Low Video").expect("Low Video must be in digest");
        assert!(pos5 < pos3, "importance-5 must appear before importance-3");
        assert!(pos3 < pos1, "importance-3 must appear before importance-1");
    }

    // RED → GREEN: correct importance tier headers appear
    #[test]
    fn test_build_digest_grouping() {
        let videos = vec![
            make_video("v1", "A", 5, "2024-01-01T00:00:00Z"),
            make_video("v2", "B", 3, "2024-01-01T00:00:00Z"),
        ];
        let html = build_digest(&videos);
        assert!(html.contains("⭐ Monetize / Professionalize"), "tier 5 header missing");
        assert!(html.contains("💡 Useful with Caveats"), "tier 3 header missing");
        assert!(!html.contains("🔧 High Actionability"), "tier 4 header should not appear");
    }

    // RED → GREEN: empty video list produces a graceful message
    #[test]
    fn test_build_digest_empty() {
        let html = build_digest(&[]);
        assert!(html.contains("No new videos"), "empty digest must say so");
    }
}
