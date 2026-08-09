//! Library context: the runtime, the auth manager, and the model provider.
//!
//! Everything the exported functions need is resolved once in [`Context::open`]
//! and reused afterwards, so a caller keeps a single opaque handle.

use codex_api::RawClient;
use codex_config::ManagedAuthPolicy;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::ReqwestTransport;
use codex_login::AuthConfig;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::ServerOptions;
use codex_login::default_client::create_client_for_route;
use codex_login::default_client::set_default_originator;
use codex_login::oauth_client_id;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_model_provider_info::built_in_model_providers;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::runtime::Builder;
use tokio::runtime::Runtime;

/// Credentials live in memory for the lifetime of a context and nowhere else:
/// no `auth.json`, no OS keyring. Whatever a login produces is handed back to
/// the caller, and the caller hands it in again through `codex_auth_import`.
const STORE_MODE: AuthCredentialsStoreMode = AuthCredentialsStoreMode::Ephemeral;

/// The executor carries no context-owned state, so every context can use the
/// same worker pool. Keeping it for the process lifetime also keeps opening and
/// closing contexts from repeatedly creating OS threads and leaving their
/// allocator arenas resident after those threads have gone away.
static RUNTIME: OnceLock<Result<Arc<Runtime>, String>> = OnceLock::new();

/// Caller-supplied startup settings.
///
/// `provider` is the same table shape as `model_providers.<id>` in
/// `config.toml`; when it is absent the built-in `openai` provider is used.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub(crate) struct InitConfig {
    pub originator: Option<String>,
    pub enable_codex_api_key_env: Option<bool>,
    pub provider: Option<ModelProviderInfo>,
    pub login: Option<LoginConfig>,
    pub proxy: Option<ProxyConfig>,
}

/// Proxy that carries every request this library makes, including the login and
/// token-refresh traffic.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) struct ProxyConfig {
    /// `http`, `https`, `socks5` or `socks5h` URL, credentials included when the
    /// proxy wants them.
    pub url: String,
    /// Comma-separated hosts that bypass the proxy, like the `NO_PROXY` variable.
    pub no_proxy: Option<String>,
}

/// Settings for the interactive login flows.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub(crate) struct LoginConfig {
    pub client_id: Option<String>,
    pub issuer: Option<String>,
    pub port: Option<u16>,
    pub open_browser: Option<bool>,
}

pub(crate) struct Context {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) provider: SharedModelProvider,
    http_client_factory: HttpClientFactory,
    store_key: PathBuf,
    login: LoginSettings,
}

struct LoginSettings {
    client_id: String,
    issuer: Option<String>,
    port: Option<u16>,
    open_browser: bool,
    auth_route_config: AuthRouteConfig,
}

/// Names one context's slot in the in-process credential store.
///
/// The ephemeral store hashes this path to separate contexts from one another.
/// Nothing is read from it and nothing is written to it, so it does not have to
/// exist on any filesystem.
fn next_store_key() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!(
        "/codex-ffi/context-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

impl Context {
    pub(crate) fn open(config: InitConfig) -> Result<Self, String> {
        let runtime = shared_runtime()?;

        if let Some(originator) = config.originator {
            set_default_originator(originator)
                .map_err(|err| format!("failed to set the originator: {err:?}"))?;
        }

        let store_key = next_store_key();
        let http_client_factory = match config.proxy {
            // A pinned proxy has to travel with `RespectSystemProxy`, because
            // that is the policy under which Codex lets a client honor the
            // factory at all. No system lookup happens regardless: the pinned
            // route answers before any resolution starts.
            Some(proxy) => HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy)
                .with_explicit_proxy(proxy.url, proxy.no_proxy),
            // Without one, reqwest keeps its own behavior, which reads
            // HTTP_PROXY, HTTPS_PROXY, ALL_PROXY and NO_PROXY.
            None => HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        };
        let login = LoginSettings {
            client_id: config
                .login
                .as_ref()
                .and_then(|login| login.client_id.clone())
                .unwrap_or_else(oauth_client_id),
            issuer: config.login.as_ref().and_then(|login| login.issuer.clone()),
            port: config.login.as_ref().and_then(|login| login.port),
            open_browser: config
                .login
                .as_ref()
                .and_then(|login| login.open_browser)
                .unwrap_or(false),
            auth_route_config: AuthRouteConfig::from_http_client_factory(
                http_client_factory.clone(),
            ),
        };

        let auth_manager = runtime
            .block_on(AuthManager::shared_from_auth_config(
                AuthConfig {
                    codex_home: store_key.clone(),
                    auth_credentials_store_mode: STORE_MODE,
                    keyring_backend_kind: keyring_backend(),
                    forced_login_method: None,
                    chatgpt_base_url: None,
                    forced_chatgpt_workspace_id: None,
                    managed_auth_policy: ManagedAuthPolicy::default(),
                    auth_route_config: login.auth_route_config.clone(),
                },
                config.enable_codex_api_key_env.unwrap_or(false),
            ))
            .map_err(|err| format!("failed to initialize authentication: {err}"))?;

        let provider_info = match config.provider {
            Some(provider_info) => provider_info,
            None => built_in_model_providers(None)
                .remove(OPENAI_PROVIDER_ID)
                .ok_or_else(|| "built-in openai provider is unavailable".to_string())?,
        };
        provider_info.validate()?;
        let provider = create_model_provider(provider_info, Some(Arc::clone(&auth_manager)));

        Ok(Self {
            runtime,
            auth_manager,
            provider,
            http_client_factory,
            store_key,
            login,
        })
    }

    pub(crate) fn store_key(&self) -> &Path {
        &self.store_key
    }

    pub(crate) fn store_mode(&self) -> AuthCredentialsStoreMode {
        STORE_MODE
    }

    pub(crate) fn keyring_backend(&self) -> AuthKeyringBackendKind {
        keyring_backend()
    }

    pub(crate) fn auth_route_config(&self) -> &AuthRouteConfig {
        &self.login.auth_route_config
    }

    /// Builds the login-flow settings for one attempt.
    pub(crate) fn server_options(&self) -> ServerOptions {
        let mut options = ServerOptions::new(
            self.store_key.clone(),
            self.login.client_id.clone(),
            None,
            STORE_MODE,
            keyring_backend(),
            self.login.auth_route_config.clone(),
        );
        if let Some(issuer) = self.login.issuer.clone() {
            options.issuer = issuer;
        }
        if let Some(port) = self.login.port {
            options.port = port;
        }
        options.open_browser = self.login.open_browser;
        options
    }

    /// Builds an endpoint client for `path` using the provider's current
    /// address, headers, retry policy, and credentials.
    pub(crate) async fn client(&self, path: &str) -> Result<RawClient<ReqwestTransport>, String> {
        let provider = self
            .provider
            .api_provider()
            .await
            .map_err(|err| format!("failed to resolve the provider endpoint: {err}"))?;
        let auth = self
            .provider
            .api_auth()
            .await
            .map_err(|err| format!("failed to resolve provider credentials: {err}"))?;

        let url = provider.url_for_path(path);
        let http_client =
            create_client_for_route(&self.http_client_factory, &url, ClientRouteClass::Api)
                .map_err(|err| format!("failed to build the http client: {err}"))?;

        Ok(RawClient::new(
            ReqwestTransport::from_http_client(http_client),
            provider,
            auth,
        ))
    }
}

fn shared_runtime() -> Result<Arc<Runtime>, String> {
    match RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .map(Arc::new)
            .map_err(|err| format!("failed to start the async runtime: {err}"))
    }) {
        Ok(runtime) => Ok(Arc::clone(runtime)),
        Err(error) => Err(error.clone()),
    }
}

impl Drop for Context {
    /// The ephemeral store outlives individual contexts, so a closed context
    /// takes its credentials with it instead of leaving them in the process.
    fn drop(&mut self) {
        let _ = codex_login::logout(&self.store_key, STORE_MODE, keyring_backend());
    }
}

/// Demanded by the storage API, unused by the ephemeral store, which has no
/// keyring behind it.
fn keyring_backend() -> AuthKeyringBackendKind {
    AuthKeyringBackendKind::default()
}
