//! Authentication actions.
//!
//! Every flow here is the one Codex itself runs: refresh follows the same rules
//! and the resulting request headers are produced by the same provider code.
//! The one deliberate difference is where credentials end up — they stay in
//! memory, and every call that changes them returns the blob to persist.

use crate::ctx::Context;
use codex_login::AuthDotJson;
use codex_login::CodexAuth;
use codex_login::ShutdownHandle;
use codex_login::complete_device_code_login;
use codex_login::load_auth_dot_json;
use codex_login::login_with_access_token;
use codex_login::login_with_api_key;
use codex_login::request_device_code;
use codex_login::run_login_server;
use codex_login::save_auth;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::timeout;

/// A login attempt that is waiting on the user.
pub(crate) struct PendingLogin {
    outcome: oneshot::Receiver<Result<(), String>>,
    abort: AbortHandle,
    shutdown: Option<ShutdownHandle>,
}

impl PendingLogin {
    pub(crate) fn cancel(&self) {
        if let Some(shutdown) = self.shutdown.as_ref() {
            shutdown.shutdown();
        }
        self.abort.abort();
    }
}

pub(crate) fn status(ctx: &Context) -> Result<Value, String> {
    let Some(auth) = ctx.runtime.block_on(ctx.auth_manager.auth()) else {
        return Ok(json!({ "authenticated": false }));
    };

    Ok(json!({
        "authenticated": true,
        "mode": to_json(auth.auth_mode())?,
        "api_mode": to_json(auth.api_auth_mode())?,
        "account_id": auth.get_account_id(),
        "email": auth.get_account_email(),
        "plan": to_json(auth.account_plan_type())?,
        "uses_codex_backend": auth.uses_codex_backend(),
        "token_available": token_available(&auth),
    }))
}

pub(crate) fn login_api_key(ctx: &Context, api_key: &str) -> Result<Value, String> {
    login_with_api_key(
        ctx.store_key(),
        api_key,
        ctx.store_mode(),
        ctx.keyring_backend(),
    )
    .map_err(|err| format!("failed to accept the api key: {err}"))?;
    settled(ctx)
}

/// Accepts a personal access token or an agent-identity JWT.
pub(crate) fn login_access_token(ctx: &Context, access_token: &str) -> Result<Value, String> {
    ctx.runtime
        .block_on(login_with_access_token(
            ctx.store_key(),
            access_token,
            ctx.store_mode(),
            None,
            None,
            ctx.keyring_backend(),
            ctx.auth_route_config(),
        ))
        .map_err(|err| format!("failed to accept the access token: {err}"))?;
    settled(ctx)
}

/// Returns the credentials the caller has to keep, or null when there are none.
pub(crate) fn export(ctx: &Context) -> Result<Value, String> {
    Ok(json!({ "credentials": stored(ctx)? }))
}

/// Takes credentials produced by an earlier call back into this context.
pub(crate) fn import(ctx: &Context, credentials: &str) -> Result<Value, String> {
    let credentials: AuthDotJson = serde_json::from_str(credentials)
        .map_err(|err| format!("failed to decode credentials: {err}"))?;
    save_auth(
        ctx.store_key(),
        &credentials,
        ctx.store_mode(),
        ctx.keyring_backend(),
    )
    .map_err(|err| format!("failed to accept credentials: {err}"))?;
    settled(ctx)
}

/// Starts the browser flow and returns the URL the user has to open.
pub(crate) fn login_chatgpt(ctx: &Context) -> Result<(Value, PendingLogin), String> {
    let options = ctx.server_options();
    let guard = ctx.runtime.enter();
    let server = run_login_server(options)
        .map_err(|err| format!("failed to start the login callback server: {err}"))?;
    drop(guard);

    let started = json!({ "auth_url": server.auth_url, "port": server.actual_port });
    let shutdown = server.cancel_handle();
    let (sender, outcome) = oneshot::channel();
    let task = ctx.runtime.spawn(async move {
        let result = server
            .block_until_done()
            .await
            .map_err(|err| format!("login failed: {err}"));
        let _ = sender.send(result);
    });

    Ok((
        started,
        PendingLogin {
            outcome,
            abort: task.abort_handle(),
            shutdown: Some(shutdown),
        },
    ))
}

/// Starts the device-code flow and returns the code the user has to enter.
pub(crate) fn login_device_code(ctx: &Context) -> Result<(Value, PendingLogin), String> {
    let options = ctx.server_options();
    let device_code = ctx
        .runtime
        .block_on(request_device_code(&options))
        .map_err(|err| format!("failed to request a device code: {err}"))?;

    let started = json!({
        "user_code": device_code.user_code,
        "verification_url": device_code.verification_url,
    });
    let (sender, outcome) = oneshot::channel();
    let task = ctx.runtime.spawn(async move {
        let result = complete_device_code_login(options, device_code)
            .await
            .map_err(|err| format!("login failed: {err}"));
        let _ = sender.send(result);
    });

    Ok((
        started,
        PendingLogin {
            outcome,
            abort: task.abort_handle(),
            shutdown: None,
        },
    ))
}

/// Waits for a pending login. A non-positive timeout waits indefinitely.
pub(crate) fn wait(
    ctx: &Context,
    pending: &mut PendingLogin,
    timeout_ms: i32,
) -> Result<Value, String> {
    let outcome = ctx.runtime.block_on(async {
        if timeout_ms > 0 {
            let limit = Duration::from_millis(u64::from(timeout_ms.unsigned_abs()));
            timeout(limit, &mut pending.outcome).await.ok()
        } else {
            Some((&mut pending.outcome).await)
        }
    });

    match outcome {
        None => Ok(json!({ "status": "pending" })),
        Some(Err(_)) => Err("the login task stopped without reporting an outcome".to_string()),
        Some(Ok(Err(message))) => Err(message),
        Some(Ok(Ok(()))) => {
            let mut settled = settled(ctx)?;
            settled["status"] = Value::from("completed");
            Ok(settled)
        }
    }
}

pub(crate) fn refresh(ctx: &Context) -> Result<Value, String> {
    ctx.runtime
        .block_on(ctx.auth_manager.refresh_token())
        .map_err(|err| format!("failed to refresh credentials: {err}"))?;
    settled(ctx)
}

pub(crate) fn logout(ctx: &Context) -> Result<Value, String> {
    let removed = ctx
        .runtime
        .block_on(ctx.auth_manager.logout())
        .map_err(|err| format!("failed to clear credentials: {err}"))?;
    Ok(json!({ "removed": removed }))
}

/// Returns the credential headers the provider would attach to a request.
pub(crate) fn headers(ctx: &Context) -> Result<Value, String> {
    let auth = ctx
        .runtime
        .block_on(ctx.provider.api_auth())
        .map_err(|err| format!("failed to resolve provider credentials: {err}"))?;
    Ok(crate::json::headers_to_json(&auth.to_auth_headers()))
}

/// The answer every credential-changing call gives: the resulting state, plus
/// the blob the caller has to persist if it wants this login to survive.
fn settled(ctx: &Context) -> Result<Value, String> {
    ctx.runtime.block_on(ctx.auth_manager.reload());
    Ok(json!({ "auth": status(ctx)?, "credentials": stored(ctx)? }))
}

fn stored(ctx: &Context) -> Result<Value, String> {
    let credentials =
        load_auth_dot_json(ctx.store_key(), ctx.store_mode(), ctx.keyring_backend())
            .map_err(|err| format!("failed to read credentials: {err}"))?;
    to_json(credentials)
}

fn token_available(auth: &CodexAuth) -> bool {
    auth.get_token().is_ok()
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("failed to encode auth state: {err}"))
}
