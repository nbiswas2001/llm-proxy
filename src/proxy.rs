use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::Stream;
use serde_json::{Value, json};
use tracing::{Level, debug, enabled, trace, warn};

use crate::AppState;
use crate::config::{Config, Provider};
use crate::transcript::{self, Outcome};

/// Headers that belong to a single hop and must not be forwarded.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Anthropic rejects a request that carries no API version. A client speaking
/// their protocol always sends one; this is the documented default for anything
/// that does not.
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const DEFAULT_ANTHROPIC_VERSION: HeaderValue = HeaderValue::from_static("2023-06-01");

/// Headers we always replace rather than pass through.
const STRIPPED_REQUEST_HEADERS: [&str; 5] = [
    "host",
    "authorization",
    "content-length",
    "accept-encoding",
    "x-api-key",
];

pub async fn health() -> impl IntoResponse {
    axum::Json(json!({ "status": "ok" }))
}

/// Catch-all handler: forwards any OpenAI-style request to the routed provider
/// and streams the upstream response straight back to the client.
pub async fn proxy(State(state): State<AppState>, request: Request) -> Response {
    let started = Instant::now();
    let started_at = chrono::Local::now();
    let (parts, body) = request.into_parts();

    if let Some(response) = client_auth_error(&state.config, &parts.headers) {
        return response;
    }

    let limit = state.config.server.max_body_mb * 1024 * 1024;
    let body_bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                format!("could not read request body: {err}"),
            );
        }
    };

    // The body is forwarded byte for byte; it is parsed only to describe the
    // request in the log and to spot a streaming call.
    let parsed: Option<Value> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&body_bytes).ok()
    };
    let field = |key: &str| parsed.as_ref().and_then(|value| value.get(key));
    let requested_model = field("model").and_then(Value::as_str).map(str::to_owned);
    let streaming = field("stream").and_then(Value::as_bool).unwrap_or(false);

    if let Some(model) = &requested_model
        && let Err(message) = state.config.reject_stale_prefix(model)
    {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", message);
    }
    let (provider_name, provider) = state.config.active();

    let url = provider.upstream_url(parts.uri.path(), parts.uri.query());
    let label = format!("{} {}", parts.method, parts.uri.path());
    debug!(
        "-> {label}  provider={provider_name}  model={}{}",
        requested_model.as_deref().unwrap_or("-"),
        if streaming { "  stream" } else { "" }
    );
    debug!("   upstream {url}");
    if enabled!(Level::TRACE) && !body_bytes.is_empty() {
        trace!("   request body: {}", preview(&body_bytes));
    }

    let mut upstream = state.http.request(parts.method.clone(), &url);
    let mut client_set_version = false;
    for (name, value) in parts.headers.iter() {
        if !is_stripped_request_header(name) {
            client_set_version |= name == ANTHROPIC_VERSION;
            upstream = upstream.header(name, value);
        }
    }
    upstream = apply_provider_auth(upstream, provider);
    if needs_anthropic_version(provider, client_set_version) {
        upstream = upstream.header(ANTHROPIC_VERSION, DEFAULT_ANTHROPIC_VERSION);
    }
    // Bytes clones share the buffer, so keeping the request for the transcript is free.
    let mut record = state.transcript.is_enabled().then(|| transcript::Entry {
        started: started_at,
        elapsed: Default::default(),
        provider: provider_name.to_owned(),
        label: label.clone(),
        model: requested_model.clone(),
        streaming,
        status: None,
        outcome: Outcome::Complete,
        error: None,
        request: body_bytes.clone(),
        response: Vec::new(),
    });
    let upstream = upstream.body(body_bytes);

    let response = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!(provider = provider_name, %url, error = %err, "upstream request failed");
            if let Some(mut entry) = record.take() {
                entry.elapsed = started.elapsed();
                entry.outcome = Outcome::Failed;
                entry.error = Some(err.to_string());
                state.transcript.record(entry);
            }
            let status = if err.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            return error_response(
                status,
                "upstream_error",
                format!("upstream provider \"{provider_name}\" failed: {err}"),
            );
        }
    };

    let status = response.status();
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers().iter() {
        if !is_stripped_response_header(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    if let Ok(value) = HeaderValue::from_str(provider_name) {
        headers.insert(HeaderName::from_static("x-llm-proxy-provider"), value);
    }
    if streaming {
        // Keep intermediaries from buffering the SSE stream.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
    }

    debug!(
        "<- {status} for {label}  headers in {}",
        millis(started.elapsed())
    );

    if let Some(entry) = &mut record {
        entry.status = Some(status.as_u16());
    }
    let body = LoggedStream {
        inner: Box::pin(response.bytes_stream()),
        label,
        started,
        bytes: 0,
        chunks: 0,
        ended: false,
        transcript: state.transcript.clone(),
        record,
    };
    (status, headers, Body::from_stream(body)).into_response()
}

/// Wraps the upstream body so the size and duration of a response - a long
/// stream especially - show up on screen once it finishes. It also collects the
/// body for the transcript, which is written once the stream ends either way.
struct LoggedStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    label: String,
    started: Instant,
    bytes: u64,
    chunks: u64,
    ended: bool,
    transcript: transcript::Transcript,
    /// `None` when the transcript is off, and once it has been written.
    record: Option<transcript::Entry>,
}

impl LoggedStream {
    fn finish_record(&mut self, outcome: Outcome, error: Option<String>) {
        if let Some(mut entry) = self.record.take() {
            entry.elapsed = self.started.elapsed();
            entry.outcome = outcome;
            entry.error = error;
            self.transcript.record(entry);
        }
    }
}

impl Stream for LoggedStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes += chunk.len() as u64;
                this.chunks += 1;
                if let Some(entry) = &mut this.record {
                    entry.response.extend_from_slice(&chunk);
                }
                if enabled!(Level::TRACE) {
                    trace!(
                        "   chunk {} ({} B): {}",
                        this.chunks,
                        chunk.len(),
                        preview(&chunk)
                    );
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.ended = true;
                warn!("   {} failed mid-body: {err}", this.label);
                this.finish_record(Outcome::Failed, Some(err.to_string()));
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.ended = true;
                debug!(
                    "   done {}  {} B in {} chunk(s), {}",
                    this.label,
                    this.bytes,
                    this.chunks,
                    millis(this.started.elapsed())
                );
                this.finish_record(Outcome::Complete, None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for LoggedStream {
    fn drop(&mut self) {
        if !self.ended {
            // The client hung up (or timed out) before the body was complete.
            debug!(
                "   aborted {}  {} B in {} chunk(s), {}",
                self.label,
                self.bytes,
                self.chunks,
                millis(self.started.elapsed())
            );
            self.finish_record(Outcome::Aborted, None);
        }
    }
}

fn millis(elapsed: std::time::Duration) -> String {
    format!("{:.0}ms", elapsed.as_secs_f64() * 1000.0)
}

/// A short, single-line excerpt of a body for trace logging.
fn preview(bytes: &[u8]) -> String {
    const LIMIT: usize = 500;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]);
    let flattened = text.replace(['\n', '\r'], " ");
    if bytes.len() > LIMIT {
        format!("{flattened}... (+{} B)", bytes.len() - LIMIT)
    } else {
        flattened
    }
}

pub fn apply_provider_auth(
    mut request: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    if let Some(key) = provider.api_key() {
        // Anthropic authenticates with its own header, not a bearer token.
        request = if provider.is_anthropic() {
            request.header("x-api-key", key)
        } else {
            request.bearer_auth(key)
        };
    }
    for (name, value) in &provider.headers {
        request = request.header(name, value);
    }
    request
}

/// Whether the proxy should supply Anthropic's version header itself. Only when
/// nobody else did: a second copy of the header is an upstream 400, and a client
/// that pins a version must keep its own.
fn needs_anthropic_version(provider: &Provider, client_set_it: bool) -> bool {
    provider.is_anthropic() && !client_set_it && !provider.sets_anthropic_version()
}

/// `None` when the caller may proceed, otherwise the 401 to send back.
fn client_auth_error(config: &Config, headers: &HeaderMap) -> Option<Response> {
    if config.server.api_keys.is_empty() {
        return None;
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer ").or(Some(value)))
        .map(str::trim)
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));

    if config.client_key_accepted(presented) {
        None
    } else {
        Some(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            "invalid or missing API key for this proxy",
        ))
    }
}

fn is_stripped_request_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    STRIPPED_REQUEST_HEADERS.contains(&name) || HOP_BY_HOP.contains(&name)
}

fn is_stripped_response_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    // Length is dropped because the body is re-framed as a stream.
    name == "content-length" || HOP_BY_HOP.contains(&name)
}

fn error_response(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": kind,
                "param": Value::Null,
                "code": status.as_u16(),
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(toml_text: &str) -> Provider {
        toml::from_str(toml_text).expect("parse provider")
    }

    fn headers_of(provider: &Provider) -> HeaderMap {
        let request = apply_provider_auth(
            reqwest::Client::new().get("https://example.invalid/v1/messages"),
            provider,
        );
        request.build().expect("build request").headers().clone()
    }

    #[test]
    fn an_openai_provider_authenticates_with_a_bearer_token() {
        let headers = headers_of(&provider(
            r#"
api = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "sk-test"
"#,
        ));
        assert_eq!(headers[header::AUTHORIZATION], "Bearer sk-test");
        assert!(!headers.contains_key("x-api-key"));
    }

    #[test]
    fn an_anthropic_provider_authenticates_with_x_api_key() {
        let headers = headers_of(&provider(
            r#"
api = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-test"
"#,
        ));
        assert_eq!(headers["x-api-key"], "sk-ant-test");
        assert!(!headers.contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn the_version_header_is_supplied_only_when_nobody_else_did() {
        let anthropic = provider(
            r#"
api = "anthropic"
base_url = "https://api.anthropic.com/v1"
"#,
        );
        assert!(needs_anthropic_version(&anthropic, false));
        // A client pinning its own version keeps it.
        assert!(!needs_anthropic_version(&anthropic, true));

        let pinned = provider(
            r#"
api = "anthropic"
base_url = "https://api.anthropic.com/v1"

[headers]
"anthropic-version" = "2023-01-01"
"#,
        );
        assert!(!needs_anthropic_version(&pinned, false));

        let openai = provider(
            r#"
api = "openai"
base_url = "https://api.deepseek.com/v1"
"#,
        );
        assert!(!needs_anthropic_version(&openai, false));
    }
}
