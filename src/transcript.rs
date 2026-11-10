//! A human-readable record of every proxied request and its response, appended
//! to one file per day under `server.log_dir`.
//!
//! Entries are handed to a dedicated writer thread, so the forwarding path never
//! waits on the disk. Each entry is written in a single call, which keeps
//! concurrent requests from interleaving inside one another.

use std::{
    fmt::Write as _, fs::OpenOptions, io::Write as _, path::PathBuf, sync::mpsc, time::Duration,
};

use axum::body::Bytes;
use chrono::{DateTime, Local};
use serde_json::Value;
use tracing::warn;

const HEAVY_RULE: &str =
    "════════════════════════════════════════════════════════════════════════════════";

/// Cheap to clone; a disabled transcript drops entries without formatting them.
#[derive(Clone)]
pub struct Transcript {
    sender: Option<mpsc::Sender<Entry>>,
}

/// How the response body came to an end.
#[derive(Clone, Copy)]
pub enum Outcome {
    Complete,
    /// The client hung up before the body was complete.
    Aborted,
    /// Upstream failed, before or during the body.
    Failed,
}

pub struct Entry {
    pub started: DateTime<Local>,
    pub elapsed: Duration,
    pub provider: String,
    pub label: String,
    pub model: Option<String>,
    pub streaming: bool,
    /// `None` when no upstream response arrived at all.
    pub status: Option<u16>,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub request: Bytes,
    pub response: Vec<u8>,
}

impl Transcript {
    pub fn disabled() -> Self {
        Self { sender: None }
    }

    /// Starts the writer thread. Files are named `llm-proxy-YYYY-MM-DD.log` by
    /// the local date each request started, so a new file begins at midnight.
    pub fn start(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let (sender, receiver) = mpsc::channel::<Entry>();
        std::thread::Builder::new()
            .name("transcript".into())
            .spawn(move || {
                for entry in receiver {
                    let path = dir.join(format!(
                        "llm-proxy-{}.log",
                        entry.started.format("%Y-%m-%d")
                    ));
                    let text = format_entry(&entry);
                    let written = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut file| file.write_all(text.as_bytes()));
                    if let Err(err) = written {
                        warn!("could not write transcript {}: {err}", path.display());
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.sender.is_some()
    }

    pub fn record(&self, entry: Entry) {
        if let Some(sender) = &self.sender {
            // Only fails once the writer thread is gone, i.e. at shutdown.
            let _ = sender.send(entry);
        }
    }
}

fn format_entry(entry: &Entry) -> String {
    let mut out = String::new();
    let status = match entry.status {
        Some(code) => code.to_string(),
        None => "no response".to_string(),
    };
    let outcome = match entry.outcome {
        Outcome::Complete => "complete",
        Outcome::Aborted => "aborted by client",
        Outcome::Failed => "failed",
    };
    let _ = writeln!(out, "{HEAVY_RULE}");
    let _ = writeln!(
        out,
        "{}  {}",
        entry.started.format("%Y-%m-%d %H:%M:%S%.3f"),
        entry.label
    );
    let _ = writeln!(
        out,
        "provider: {}   model: {}   stream: {}",
        entry.provider,
        entry.model.as_deref().unwrap_or("-"),
        if entry.streaming { "yes" } else { "no" }
    );
    let _ = writeln!(
        out,
        "status: {status}   {:.0} ms   {outcome}",
        entry.elapsed.as_secs_f64() * 1000.0
    );
    if let Some(error) = &entry.error {
        let _ = writeln!(out, "error: {error}");
    }

    section(&mut out, "REQUEST", &readable_body(&entry.request));

    if entry.status.is_none() {
        out.push('\n');
        return out;
    }
    match assemble_stream(&entry.response) {
        Some(stream) => {
            if !stream.reasoning.is_empty() {
                section(&mut out, "RESPONSE · reasoning", &stream.reasoning);
            }
            if !stream.text.is_empty() {
                section(&mut out, "RESPONSE · text", &stream.text);
            }
            if !stream.tools.is_empty() {
                section(&mut out, "RESPONSE · tool calls", &stream.tools);
            }
            if let Some(usage) = &stream.usage {
                section(&mut out, "RESPONSE · usage", usage);
            }
        }
        None => section(&mut out, "RESPONSE", &readable_body(&entry.response)),
    }
    out.push('\n');
    out
}

fn section(out: &mut String, title: &str, body: &str) {
    let rule = "─".repeat(74usize.saturating_sub(title.chars().count()));
    let _ = writeln!(out, "──── {title} {rule}");
    if body.is_empty() {
        let _ = writeln!(out, "(empty)");
    } else {
        let _ = writeln!(out, "{}", body.trim_end());
    }
}

/// JSON pretty-printed, anything else as (lossy) text.
fn readable_body(bytes: &[u8]) -> String {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[derive(Default)]
struct Assembled {
    reasoning: String,
    text: String,
    tools: String,
    usage: Option<String>,
}

/// Rebuilds the reply a streamed (SSE) response spelled out piece by piece, for
/// both OpenAI's chat-completion chunks and Anthropic's message events. `None`
/// when the body is not an event stream, or holds nothing recognisable - the
/// caller then shows it as it arrived.
fn assemble_stream(body: &[u8]) -> Option<Assembled> {
    let text = std::str::from_utf8(body).ok()?;
    let mut out = Assembled::default();
    let mut recognised = false;

    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        // OpenAI: {"choices":[{"delta":{"content":..., "reasoning_content":..., "tool_calls":[...]}}]}
        if let Some(delta) = event.pointer("/choices/0/delta") {
            recognised = true;
            push_str(&mut out.reasoning, delta.get("reasoning_content"));
            push_str(&mut out.reasoning, delta.get("reasoning"));
            push_str(&mut out.text, delta.get("content"));
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    start_tool(&mut out.tools, name);
                }
                push_str(&mut out.tools, call.pointer("/function/arguments"));
            }
        }

        // Anthropic: typed events, the text arriving in content_block_delta.
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                recognised = true;
                if let Some(block) = event.get("content_block")
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let Some(name) = block.get("name").and_then(Value::as_str)
                {
                    start_tool(&mut out.tools, name);
                }
            }
            Some("content_block_delta") => {
                recognised = true;
                let delta = event.get("delta");
                let field = |key| delta.and_then(|d| d.get(key));
                push_str(&mut out.text, field("text"));
                push_str(&mut out.reasoning, field("thinking"));
                push_str(&mut out.tools, field("partial_json"));
            }
            _ => {}
        }

        // Both protocols report token counts, Anthropic across several events;
        // the last one seen is the most complete.
        if let Some(usage) = event
            .get("usage")
            .or_else(|| event.pointer("/message/usage"))
            .filter(|usage| !usage.is_null())
        {
            out.usage = serde_json::to_string_pretty(usage).ok();
        }
    }
    recognised.then_some(out)
}

fn push_str(target: &mut String, value: Option<&Value>) {
    if let Some(text) = value.and_then(Value::as_str) {
        target.push_str(text);
    }
}

fn start_tool(tools: &mut String, name: &str) {
    if !tools.is_empty() {
        tools.push('\n');
    }
    tools.push_str(name);
    tools.push_str(": ");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openai_stream_is_reassembled_into_text() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let assembled = assemble_stream(body.as_bytes()).expect("recognised");
        assert_eq!(assembled.reasoning, "Thinking");
        assert_eq!(assembled.text, "Hello");
        assert!(assembled.usage.unwrap().contains("\"total_tokens\": 7"));
    }

    #[test]
    fn an_anthropic_stream_is_reassembled_into_text_and_tool_calls() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"there\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Paris\\\"}\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
        );
        let assembled = assemble_stream(body.as_bytes()).expect("recognised");
        assert_eq!(assembled.text, "Hi there");
        assert_eq!(assembled.tools, "get_weather: {\"city\":\"Paris\"}");
        assert!(assembled.usage.unwrap().contains("output_tokens"));
    }

    #[test]
    fn a_plain_json_response_is_not_mistaken_for_a_stream() {
        assert!(assemble_stream(br#"{"choices":[{"message":{"content":"hi"}}]}"#).is_none());
        assert_eq!(readable_body(br#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }
}
