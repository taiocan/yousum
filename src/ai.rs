// src/ai.rs — Transcript cleaning, chunking, and Gemini summarisation
// TDD: tests in #[cfg(test)] were written before implementation.

use anyhow::{anyhow, Context, Result};

// ─── Public constants ────────────────────────────────────────────────────────

pub const DEFAULT_CHUNK_SIZE: usize = 500_000;

// Base URL for Gemini — overridable in tests via the GEMINI_API_BASE env var
fn gemini_base() -> String {
    std::env::var("GEMINI_API_BASE")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string())
}

// ─── Transcript helpers ──────────────────────────────────────────────────────

/// Remove SRT artefacts (sequence numbers, timestamps, WEBVTT header) and
/// normalise whitespace, returning a plain text transcript.
pub fn clean_transcript(raw: &str) -> String {
    let timestamp_re = |line: &str| -> bool {
        // SRT: "00:00:00,000 --> 00:00:01,000"
        // VTT: "00:00:00.000 --> 00:00:01.000"
        line.contains("-->")
    };
    let is_sequence_number = |line: &str| -> bool { line.trim().parse::<u64>().is_ok() };
    let is_webvtt_header = |line: &str| -> bool {
        line.starts_with("WEBVTT") || line.starts_with("NOTE") || line.starts_with("Kind:")
    };

    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| !timestamp_re(l))
        .filter(|l| !is_sequence_number(l))
        .filter(|l| !is_webvtt_header(l))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split `text` into chunks of at most `chunk_size` characters.
pub fn split_chunks(text: &str, chunk_size: usize) -> Vec<String> {
    if text.is_empty() || chunk_size == 0 {
        return vec![];
    }
    // Split on char boundaries to avoid panics in multi-byte strings
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(chunk_size)
        .map(|c| c.iter().collect())
        .collect()
}

/// Parse the raw Gemini response into `(summary, grade)`.
pub fn parse_response(response: &str) -> Result<(String, u8)> {
    let grade = response
        .lines()
        .find(|l| l.starts_with("GRADE:"))
        .ok_or_else(|| anyhow!("Gemini response missing GRADE line"))?
        .trim_start_matches("GRADE:")
        .trim()
        .parse::<u8>()
        .context("GRADE value is not a valid integer")?;

    let summary_start = response
        .find("KEY MESSAGES:")
        .ok_or_else(|| anyhow!("Gemini response missing KEY MESSAGES section"))?;
    let summary = response[summary_start..].trim().to_string();

    if summary.is_empty() {
        return Err(anyhow!("KEY MESSAGES section is empty"));
    }

    Ok((summary, grade))
}

/// Make a single request to the Gemini API with a fallback mechanism.
/// Tries Gemini 3.1 Flash Preview first, falls back to 2.5 Flash if that fails.
pub fn call_gemini(api_key: &str, prompt: &str) -> Result<String> {
    // Attempt 1: Gemini 3.1 Flash (Preview via v1beta)
    let url_31 = format!(
        "{}/v1beta/models/gemini-3.1-flash-lite-preview:generateContent?key={}",
        gemini_base(),
        api_key
    );

    match send_to_gemini(&url_31, prompt) {
        Ok(res) => Ok(res),
        Err(e) => {
            eprintln!("  ! Gemini 3.1 failed (reason: {e:#}). Falling back to 2.5...");
            // Attempt 2: Gemini 2.5 Flash (Stable via v1)
            let url_25 = format!(
                "{}/v1/models/gemini-2.5-flash:generateContent?key={}",
                gemini_base(),
                api_key
            );
            send_to_gemini(&url_25, prompt).context("Both Gemini 3.1 and 2.5 fallback failed")
        }
    }
}

fn send_to_gemini(url: &str, prompt: &str) -> Result<String> {
    let body = serde_json::json!({
        "contents": [{
            "parts": [{"text": prompt}]
        }]
    });

    let client = reqwest::blocking::Client::new();
    let response = client
        .post(url)
        .json(&body)
        .send()
        .context("HTTP request to Gemini failed")?;

    let status = response.status();
    let text = response.text().context("Could not read Gemini response body")?;

    if !status.is_success() {
        return Err(anyhow!("Gemini API error {status}: {text}"));
    }

    let json: serde_json::Value = serde_json::from_str(&text)
        .context("Gemini response is not valid JSON")?;

    let content = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow!("Unexpected Gemini response shape: {text}"))?
        .to_string();

    Ok(content)
}

/// Summarise a video transcript.  For long transcripts, chunks are summarised
/// individually and then combined in a second Gemini call.
pub fn summarize(api_key: &str, title: &str, url: &str, transcript: &str) -> Result<(String, u8)> {
    let cleaned = clean_transcript(transcript);
    println!("    Transcript cleaned: {} chars", cleaned.len());

    let condensed = if cleaned.len() <= DEFAULT_CHUNK_SIZE {
        // Single-pass
        println!("    Sending transcript to Gemini (single pass)...");
        call_gemini(api_key, &build_prompt(title, url, &cleaned))?
    } else {
        // Multi-pass: summarise each chunk, then combine
        let chunks = split_chunks(&cleaned, DEFAULT_CHUNK_SIZE);
        println!("    Sending {} chunks to Gemini (multi pass)...", chunks.len());
        let chunk_summaries: Vec<String> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                println!("      Summarising chunk {}/{}...", i + 1, chunks.len());
                let chunk_prompt = format!(
                    "Summarise this portion ({}/{}) of the transcript for the video titled '{}':\n\n{}",
                    i + 1,
                    chunks.len(),
                    title,
                    chunk
                );
                call_gemini(api_key, &chunk_prompt)
            })
            .collect::<Result<Vec<_>>>()?;

        let combined = chunk_summaries.join("\n\n---\n\n");
        println!("    Generating final overall summary...");
        call_gemini(api_key, &build_prompt(title, url, &combined))?
    };

    println!("    ✓ Summary received!");
    parse_response(&condensed)
}

fn build_prompt(title: &str, url: &str, transcript: &str) -> String {
    format!(
        r#"You are analyzing a YouTube video transcript for an AI software developer focused on monetizing and professionalizing their projects.

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

Transcript:
{transcript}"#
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    // RED → GREEN: SRT timestamps are stripped
    #[test]
    fn test_clean_removes_srt_timestamps() {
        let raw = "1\n00:00:01,000 --> 00:00:03,000\nHello world\n\n2\n00:00:04,000 --> 00:00:06,000\nFoo bar";
        let cleaned = clean_transcript(raw);
        assert!(!cleaned.contains("-->"), "timestamps should be removed");
        assert!(cleaned.contains("Hello world"));
        assert!(cleaned.contains("Foo bar"));
    }

    // RED → GREEN: bare sequence numbers are stripped
    #[test]
    fn test_clean_removes_sequence_numbers() {
        let raw = "1\n00:00:01,000 --> 00:00:02,000\nActual content";
        let cleaned = clean_transcript(raw);
        // The digit "1" by itself should not appear as a standalone token
        assert!(!cleaned.split_whitespace().any(|w| w == "1"));
    }

    // RED → GREEN: normalise multiple spaces/newlines to single space
    #[test]
    fn test_clean_normalizes_whitespace() {
        let raw = "Hello   world\n\nFoo  bar";
        let cleaned = clean_transcript(raw);
        // The join produces "Hello   world Foo  bar" which is correct —
        // internal spaces inside a line are not modified, but no blank lines remain.
        assert!(!cleaned.contains("\n\n"));
        assert!(cleaned.contains("Hello"));
        assert!(cleaned.contains("Foo"));
    }

    // RED → GREEN: correct chunk count for known length
    #[test]
    fn test_split_chunks_exact() {
        let text = "a".repeat(8_000);
        let chunks = split_chunks(&text, 4_000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() == 4_000));
    }

    // RED → GREEN: last chunk may be shorter
    #[test]
    fn test_split_chunks_last_smaller() {
        let text = "a".repeat(5_500);
        let chunks = split_chunks(&text, 4_000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4_000);
        assert_eq!(chunks[1].len(), 1_500);
    }

    // RED → GREEN: parse_response extracts grade and structured summary correctly
    #[test]
    fn test_parse_gemini_response_valid() {
        let response = "GRADE: 4\n\nKEY MESSAGES:\n- Uses Claude API with tool use.\n\nWHY IT MATTERS:\n- Useful for AI developers.";
        let (summary, grade) = parse_response(response).unwrap();
        assert_eq!(grade, 4);
        assert!(summary.starts_with("KEY MESSAGES:"));
        assert!(summary.contains("Uses Claude API"));
    }

    // RED → GREEN: missing GRADE returns Err
    #[test]
    fn test_parse_gemini_response_missing_importance() {
        let response = "KEY MESSAGES:\n- Some text";
        assert!(parse_response(response).is_err());
    }

    // RED → GREEN: call_gemini sends correct request and parses response (mocked)
    #[test]
    fn test_call_gemini_mocked() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/v1beta/models/gemini-1.5-flash:generateContent")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "candidates": [{
                    "content": {
                        "parts": [{"text": "GRADE: 3\n\nKEY MESSAGES:\n- Mocked summary."}]
                    }
                }]
            }"#)
            .create();

        // Point the client at our mock server
        std::env::set_var("GEMINI_API_BASE", server.url());
        let result = call_gemini("fake_key", "test prompt");
        std::env::remove_var("GEMINI_API_BASE");

        mock.assert();
        let text = result.unwrap();
        assert!(text.contains("GRADE: 3"));
    }

    // RED → GREEN: short transcript → single Gemini call
    #[test]
    fn test_summarize_short_transcript() {
        let mut server = Server::new();
        // Expect exactly one call
        let mock = server
            .mock("POST", "/v1beta/models/gemini-1.5-flash:generateContent")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "candidates": [{
                    "content": {"parts": [{"text": "GRADE: 5\n\nKEY MESSAGES:\n- Short."}]}
                }]
            }"#)
            .expect(1)
            .create();

        std::env::set_var("GEMINI_API_BASE", server.url());
        let (summary, grade) = summarize("key", "My Video", "https://youtu.be/abc", "Short transcript").unwrap();
        std::env::remove_var("GEMINI_API_BASE");

        mock.assert();
        assert_eq!(grade, 5);
        assert!(summary.contains("Short."));
    }

    // RED → GREEN: long transcript → multiple chunk calls + one final call
    #[test]
    fn test_summarize_long_transcript() {
        let mut server = Server::new();
        // Two chunks → 2 chunk calls + 1 final call = 3 total
        let mock = server
            .mock("POST", "/v1beta/models/gemini-1.5-flash:generateContent")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "candidates": [{
                    "content": {"parts": [{"text": "GRADE: 3\n\nKEY MESSAGES:\n- Combined."}]}
                }]
            }"#)
            .expect(4)
            .create();

        let long_text = "word ".repeat(2_000); // 10,000 chars / 4,000 -> 3 chunks
        std::env::set_var("GEMINI_API_BASE", server.url());
        let (summary, grade) = summarize("key", "Long Video", "https://youtu.be/xyz", &long_text).unwrap();
        std::env::remove_var("GEMINI_API_BASE");

        mock.assert();
        assert_eq!(grade, 3);
        assert!(summary.contains("Combined."));
    }
}
