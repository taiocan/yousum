// src/youtube.rs — YouTube playlist discovery and subtitle download
// TDD: tests in #[cfg(test)] were written before implementation.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::process::Command;

// ─── Domain type ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct VideoMeta {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub upload_date: String,
    pub video_url: String,
}

// Base URL for YouTube API — overridable in tests
fn yt_base() -> String {
    std::env::var("YOUTUBE_API_BASE")
        .unwrap_or_else(|_| "https://www.googleapis.com".to_string())
}

// ─── Serde helpers ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PlaylistResponse {
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    items: Vec<PlaylistItem>,
}

#[derive(Deserialize)]
struct PlaylistItem {
    snippet: Snippet,
}

#[derive(Deserialize)]
struct Snippet {
    title: String,
    #[serde(rename = "videoOwnerChannelTitle")]
    channel: Option<String>,
    #[serde(rename = "publishedAt")]
    published_at: String,
    #[serde(rename = "resourceId")]
    resource_id: ResourceId,
}

#[derive(Deserialize)]
struct ResourceId {
    #[serde(rename = "videoId")]
    video_id: String,
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Fetch all videos in a YouTube playlist, following pagination automatically.
pub fn discover_playlist(api_key: &str, playlist_id: &str) -> Result<Vec<VideoMeta>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()?;
    let mut videos = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = reqwest::Url::parse(&format!(
            "{}/youtube/v3/playlistItems",
            yt_base()
        ))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("part", "snippet");
            q.append_pair("maxResults", "50");
            q.append_pair("playlistId", playlist_id);
            q.append_pair("key", api_key);
            if let Some(ref token) = page_token {
                q.append_pair("pageToken", token);
            }
        }

        let resp = client
            .get(url)
            .send()
            .context("YouTube API request failed")?;

        let status = resp.status();
        let body = resp.text().context("Could not read YouTube API response body")?;

        if !status.is_success() {
            return Err(anyhow!("YouTube API error {status}: {body}"));
        }

        let page: PlaylistResponse = serde_json::from_str(&body)
            .with_context(|| format!("Failed to parse YouTube API response. Raw body: {body}"))?;

        for item in page.items {
            let vid_id = &item.snippet.resource_id.video_id;
            videos.push(VideoMeta {
                video_id: vid_id.clone(),
                title: item.snippet.title,
                channel: item.snippet.channel.unwrap_or_else(|| "Unknown".to_string()),
                upload_date: item.snippet.published_at,
                video_url: format!("https://www.youtube.com/watch?v={vid_id}"),
            });
        }

        match page.next_page_token {
            Some(token) => page_token = Some(token),
            None => break,
        }
    }

    Ok(videos)
}

/// Download auto-generated English subtitles for a video using `yt-dlp`.
/// Returns the cleaned SRT content as a String.
///
/// NOTE: This function is `#[ignore]`-gated in tests because it requires a real
/// network connection and a `yt-dlp` binary on PATH.
pub fn download_subtitles(video_id: &str, video_url: &str) -> Result<String> {
    let tmp_dir = std::env::temp_dir();
    let output_template = tmp_dir.join(format!("{video_id}")).display().to_string();

    let output = Command::new("yt-dlp")
        .args([
            "--skip-download",
            "--write-auto-sub",
            "--sub-lang", "en",
            "--convert-subs", "srt",
            "-o", &output_template,
            "--",
            video_url,
        ])
        .output()
        .context("Failed to launch yt-dlp — is it installed and on PATH?")?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        if err_msg.contains("Private video") {
            return Err(anyhow!("Permanent error: Private video"));
        }
        if err_msg.contains("There are no subtitles") {
             return Err(anyhow!("Permanent error: No subtitles available"));
        }
        return Err(anyhow!("yt-dlp failed: {err_msg}"));
    }

    // yt-dlp writes <template>.en.srt
    let srt_path = format!("{output_template}.en.srt");
    let content = std::fs::read_to_string(&srt_path)
        .with_context(|| format!("Could not read subtitle file: {srt_path}"))?;

    // Best-effort cleanup
    let _ = std::fs::remove_file(&srt_path);

    Ok(content)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    fn playlist_page(next_token: Option<&str>, video_id: &str, title: &str) -> String {
        let token_field = match next_token {
            Some(t) => format!(r#","nextPageToken": "{t}""#),
            None => String::new(),
        };
        format!(
            r#"{{
                "items": [{{
                    "snippet": {{
                        "title": "{title}",
                        "videoOwnerChannelTitle": "TestChannel",
                        "publishedAt": "2024-01-15T10:00:00Z",
                        "resourceId": {{"videoId": "{video_id}"}}
                    }}
                }}]{token_field}
            }}"#
        )
    }

    // RED → GREEN: discover_playlist parses a single page correctly
    #[test]
    fn test_discover_playlist_mocked() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/youtube/v3/playlistItems")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(playlist_page(None, "abc123", "My Video"))
            .create();

        std::env::set_var("YOUTUBE_API_BASE", server.url());
        let videos = discover_playlist("fake_key", "PLtest").unwrap();
        std::env::remove_var("YOUTUBE_API_BASE");

        mock.assert();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].video_id, "abc123");
        assert_eq!(videos[0].title, "My Video");
        assert_eq!(videos[0].channel, "TestChannel");
        assert!(videos[0].video_url.contains("abc123"));
    }

    // RED → GREEN: discover_playlist follows nextPageToken until exhausted
    #[test]
    fn test_discover_playlist_pagination() {
        let mut server = Server::new();

        // First page returns a nextPageToken
        let mock1 = server
            .mock("GET", "/youtube/v3/playlistItems")
            .match_query(mockito::Matcher::UrlEncoded(
                "pageToken".into(),
                mockito::Matcher::Missing.to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(playlist_page(Some("TOKEN2"), "vid1", "Video 1"))
            .create();

        // We can't easily match "no pageToken" with mockito, so let's use a simpler approach:
        // just mount two responses in order on the same path
        let _ = mock1; // suppress warning

        // Use two separate mocks matching on the presence/absence of pageToken
        let mut server2 = Server::new();

        let _m1 = server2
            .mock("GET", "/youtube/v3/playlistItems")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(playlist_page(Some("TOKEN2"), "vid1", "Video 1"))
            .create();

        let _m2 = server2
            .mock("GET", "/youtube/v3/playlistItems")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(playlist_page(None, "vid2", "Video 2"))
            .create();

        std::env::set_var("YOUTUBE_API_BASE", server2.url());
        let videos = discover_playlist("fake_key", "PLtest").unwrap();
        std::env::remove_var("YOUTUBE_API_BASE");

        assert_eq!(videos.len(), 2, "both pages should be collected");
        assert_eq!(videos[0].video_id, "vid1");
        assert_eq!(videos[1].video_id, "vid2");
    }

    // RED → GREEN: download_subtitles — integration test, skipped in CI
    // Run manually with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_download_subtitles_real() {
        // Rick Astley – short, guaranteed subtitles
        let result =
            download_subtitles("dQw4w9WgXcQ", "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(result.is_ok(), "subtitle download should succeed: {:?}", result);
        let srt = result.unwrap();
        assert!(!srt.is_empty());
    }
}
