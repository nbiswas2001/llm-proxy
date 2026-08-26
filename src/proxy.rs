use std::{
    collections::BTreeSet,
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

    // Only JSON bodies carry a model name; anything else is forwarded untouched.
    let parsed: Option<Value> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&body_bytes).ok()
    };
    let requested_model = parsed
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let route = match state.config.resolve(requested_model.as_deref()) {
        Ok(route) => route,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                err.to_string(),
            );
        }
    };

    // Rewrite the model name when the client-facing name differs upstream.
    let outgoing_bytes = match (&route.upstream_model, parsed) {
        (Some(upstream), Some(mut value))
            if Some(upstream) != requested_model.as_ref() && value.get("model").is_some() =>
        {
            value["model"] = json!(upstream);
            match serde_json::to_vec(&value) {
                Ok(bytes) => bytes.into(),
                Err(err) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        format!("could not re-encode request body: {err}"),
                    );
                }
            }
        }
        _ => body_bytes,
    };

    let url = route
        .provider
        .upstream_url(parts.uri.path(), parts.uri.query());
    let streaming = is_stream_request(&outgoing_bytes);
    let label = format!("{} {}", parts.method, parts.uri.path());
    debug!(
        "-> {label}  provider={}  model={}{}",
        route.name,
        requested_model.as_deref().unwrap_or("-"),
        if streaming { "  stream" } else { "" }
    );
    debug!("   upstream {url}");
    if enabled!(Level::TRACE) && !outgoing_bytes.is_empty() {
        trace!("   request body: {}", preview(&outgoing_bytes));
    }

    let mut upstream = state.http.request(parts.method.clone(), &url);
    for (name, value) in parts.headers.iter() {
        if !is_stripped_request_header(name) {
            upstream = upstream.header(name, value);
        }
    }
    upstream = apply_provider_auth(upstream, route.provider);
    let upstream = upstream.body(outgoing_bytes);

    let response = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!(provider = route.name, %url, error = %err, "upstream request failed");
            let status = if err.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            return error_response(
                status,
                "upstream_error",
                format!("upstream provider \"{}\" failed: {err}", route.name),
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
    if let Ok(value) = HeaderValue::from_str(route.name) {
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

    let body = LoggedStream {
        inner: Box::pin(response.bytes_stream()),
        label,
        started,
        bytes: 0,
        chunks: 0,
        ended: false,
    };
    (status, headers, Body::from_stream(body)).into_response()
}

/// Wraps the upstream body so the size and duration of a response - a long
/// stream especially - show up on screen once it finishes.
struct LoggedStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    label: String,
    started: Instant,
    bytes: u64,
    chunks: u64,
    ended: bool,
}

impl Stream for LoggedStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes += chunk.len() as u64;
                this.chunks += 1;
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

/// `GET /v1/models`: the union of every provider's models, so a client's model
/// picker shows what this proxy can actually route to.
pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = client_auth_error(&state.config, &headers) {
        return response;
    }

    let lookups = state
        .config
        .providers
        .iter()
        .map(|(name, provider)| models_for_provider(&state, name, provider));
    let per_provider = futures_util::future::join_all(lookups).await;

    let mut seen = BTreeSet::new();
    let mut data = Vec::new();
    for (name, ids) in per_provider {
        for id in ids {
            if seen.insert(id.clone()) {
                data.push(json!({
                    "id": id,
                    "object": "model",
                    "created": 0,
                    "owned_by": name,
                }));
            }
        }
    }

    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

async fn models_for_provider(
    state: &AppState,
    name: &str,
    provider: &Provider,
) -> (String, Vec<String>) {
    // Aliases are client-facing names, so they are always exposed verbatim.
    let mut ids: Vec<String> = provider.model_aliases.keys().cloned().collect();

    let upstream_ids: Vec<String> = if !provider.models.is_empty() {
        provider.models.clone()
    } else if provider.fetch_models {
        fetch_upstream_models(state, name, provider).await
    } else {
        Vec::new()
    };

    // Unprefixed names route to the default provider, so only it can claim them.
    let bare = state.config.is_default_provider(name);
    ids.extend(
        upstream_ids
            .into_iter()
            .map(|id| if bare { id } else { format!("{name}:{id}") }),
    );
    (name.to_string(), ids)
}

async fn fetch_upstream_models(state: &AppState, name: &str, provider: &Provider) -> Vec<String> {
    let url = provider.upstream_url("/v1/models", None);
    let mut request = state.http.get(&url);
    request = apply_provider_auth(request, provider);
    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<Value>().await
    }
    .await;

    match result {
        Ok(value) => value
            .get("data")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.get("id").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        Err(err) => {
            warn!(provider = name, %url, error = %err, "could not fetch upstream model list");
            Vec::new()
        }
    }
}

fn apply_provider_auth(
    mut request: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    if let Some(key) = provider.api_key() {
        request = request.bearer_auth(key);
    }
    for (name, value) in &provider.headers {
        request = request.header(name, value);
    }
    request
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

fn is_stream_request(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
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
