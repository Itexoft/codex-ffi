use super::session::EndpointSession;
use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_client::Response;
use codex_client::StreamResponse;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use tracing::instrument;

/// Endpoint client that reaches any path under the provider base URL without
/// interpreting request or response payloads.
///
/// The caller owns the JSON on both sides. Everything else — base URL, default
/// headers, query params, retry policy, credential application — still comes
/// from the provider, so requests are built the same way the typed clients
/// build theirs. Streaming responses are returned before SSE decoding so the
/// caller can consume the wire bytes directly.
pub struct RawClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> RawClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    pub fn provider(&self) -> &Provider {
        self.session.provider()
    }

    #[instrument(
        name = "raw.execute",
        level = "info",
        skip_all,
        fields(transport = "raw_http", http.method = %method, api.path = path)
    )]
    pub async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        self.session.execute(method, path, extra_headers, body).await
    }

    #[instrument(
        name = "raw.stream",
        level = "info",
        skip_all,
        fields(transport = "raw_http", http.method = %method, api.path = path)
    )]
    pub async fn stream(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        compression: Compression,
    ) -> Result<StreamResponse, ApiError> {
        let body = match body {
            Some(body) => Some(EncodedJsonBody::encode(&body).map_err(|e| {
                ApiError::Stream(format!("failed to encode raw request body: {e}"))
            })?),
            None => None,
        };
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        self.session
            .stream_encoded_json_with(method, path, extra_headers, body, |req| {
                req.compression = request_compression;
            })
            .await
    }
}
