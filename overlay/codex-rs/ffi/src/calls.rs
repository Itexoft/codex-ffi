//! Requests to the provider endpoint.
//!
//! Bodies cross this module untouched: the caller supplies request JSON and
//! receives response JSON or raw SSE payloads. Address, headers, credentials,
//! retries, and SSE framing all come from the Codex request path.

use crate::ctx::Context;
use crate::json::fail;
use crate::json::headers_to_json;
use codex_api::ApiError;
use codex_api::Compression;
use codex_client::TransportError;
use codex_client::sse_stream;
use codex_http_client::StreamError;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::header::ACCEPT;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Matches the capacity `codex-api` uses for its own responses SSE channel
/// (see `codex-api/src/sse/responses.rs`).
const EVENT_CHANNEL_CAPACITY: usize = 1600;

/// An open server-sent-event stream.
pub(crate) struct EventStream {
    runtime: Arc<Runtime>,
    events: mpsc::Receiver<Result<String, StreamError>>,
}

/// Opens a streaming POST and returns the response head plus the live stream.
pub(crate) fn open(
    ctx: &Context,
    path: &str,
    body: Option<Value>,
    mut headers: HeaderMap,
) -> Result<(Value, EventStream), Value> {
    if !headers.contains_key(ACCEPT) {
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    }

    let runtime = Arc::clone(&ctx.runtime);
    runtime.block_on(async {
        let client = ctx.client(path).await.map_err(fail)?;
        let idle_timeout = client.provider().stream_idle_timeout;
        let response = client
            .stream(Method::POST, path, headers, body, Compression::None)
            .await
            .map_err(describe)?;

        let opened = json!({
            "status": response.status.as_u16(),
            "headers": headers_to_json(&response.headers),
        });
        let (sender, events) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        sse_stream(response.bytes, idle_timeout, sender);

        Ok((
            opened,
            EventStream {
                runtime: Arc::clone(&ctx.runtime),
                events,
            },
        ))
    })
}

/// Reads the next stream frame. A non-positive timeout waits indefinitely.
///
/// `status` is one of `event` (payload in `data`), `pending` (timeout expired),
/// `closed` (producer finished), or `error`. Note that the SSE reader reports
/// the end of an HTTP stream as an error frame, so a completed response ends
/// with `response.completed` in `data` followed by an `error` frame.
pub(crate) fn next(stream: &mut EventStream, timeout_ms: i32) -> Value {
    let runtime = Arc::clone(&stream.runtime);
    let events = &mut stream.events;

    let received = runtime.block_on(async {
        if timeout_ms > 0 {
            let limit = Duration::from_millis(u64::from(timeout_ms.unsigned_abs()));
            timeout(limit, events.recv()).await.ok()
        } else {
            Some(events.recv().await)
        }
    });

    match received {
        None => json!({ "status": "pending" }),
        Some(None) => json!({ "status": "closed" }),
        Some(Some(Ok(data))) => json!({ "status": "event", "data": data }),
        Some(Some(Err(error))) => json!({ "status": "error", "message": error.to_string() }),
    }
}

/// Performs a single non-streaming request.
pub(crate) fn request(
    ctx: &Context,
    method: Method,
    path: &str,
    body: Option<Value>,
    headers: HeaderMap,
) -> Result<Value, Value> {
    ctx.runtime.block_on(async {
        let client = ctx.client(path).await.map_err(fail)?;
        let response = client
            .execute(method, path, headers, body)
            .await
            .map_err(describe)?;
        let body = String::from_utf8(response.body.to_vec())
            .map_err(|err| fail(format!("response body is not valid UTF-8: {err}")))?;

        Ok(json!({
            "status": response.status.as_u16(),
            "headers": headers_to_json(&response.headers),
            "body": body,
        }))
    })
}

/// Keeps the endpoint status code and body reachable for the caller.
fn describe(error: ApiError) -> Value {
    match &error {
        ApiError::Transport(TransportError::Http { status, body, .. }) => json!({
            "message": error.to_string(),
            "status": status.as_u16(),
            "body": body,
        }),
        _ => fail(error),
    }
}
