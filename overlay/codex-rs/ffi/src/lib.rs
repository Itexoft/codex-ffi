//! C ABI over the Codex endpoint and authentication layer.
//!
//! The library owns exactly one thing while it runs: the request path to the
//! configured provider. Request and response payloads are passed through as
//! JSON text, so the caller decides how to build and read them.
//!
//! Nothing is persisted. Credentials live in the context and disappear with it —
//! there is no `auth.json` and no OS keyring. Every call that changes them
//! answers with the blob to keep, and [`codex_auth_import`] takes that blob back.
//!
//! Calling convention:
//!
//! - Every function that answers returns a heap JSON string. Release it with
//!   [`codex_string_free`].
//! - The envelope is `{"ok":true,"result":…}` or `{"ok":false,"error":{…}}`.
//! - Handles (`codex_init`, `codex_stream_open`, the login flows) are opaque
//!   pointers owned by the caller until the matching close function runs.
//! - Handles are safe to use from several threads only when the caller
//!   serializes calls on the same handle; different handles are independent.

mod auth;
mod calls;
mod ctx;
mod json;

use crate::auth::PendingLogin;
use crate::calls::EventStream;
use crate::ctx::Context;
use crate::ctx::InitConfig;
use crate::json::decode;
use crate::json::fail;
use crate::json::headers_from;
use http::Method;
use serde_json::Value;
use serde_json::json;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::os::raw::c_void;
#[cfg(target_os = "linux")]
use std::sync::Once;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(target_os = "linux")]
fn configure_allocator() {
    static CONFIGURED: Once = Once::new();

    CONFIGURED.call_once(|| {
        // `mi_option_purge_delay` is 15 in the pinned mimalloc v2 API. Setting
        // it before the first Rust allocation makes a page eligible for
        // decommit as soon as its last object is freed. This library is called
        // from host-owned, short-lived threads, so delaying that hand-back
        // would otherwise turn their past high-water marks into process RSS.
        const PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

        unsafe { libmimalloc_sys::mi_option_set(PURGE_DELAY, 0) };
    });
}

#[cfg(not(target_os = "linux"))]
fn configure_allocator() {}

/// Releases a string returned by this library.
///
/// # Safety
/// `text` must have been produced by this library and must not be reused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_string_free(text: *mut c_char) {
    unsafe { json::reclaim(text) };
}

/// Creates a library context.
///
/// `config_json` may be null. Recognized fields: `originator`,
/// `enable_codex_api_key_env`, `provider` (the same shape as a
/// `model_providers.<id>` table), `login`
/// (`client_id`/`issuer`/`port`/`open_browser`), and `proxy`
/// (`url` plus optional `no_proxy`).
///
/// A fresh context holds no credentials. Log in, or call [`codex_auth_import`].
///
/// # Safety
/// `out_context` must point to writable storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_init(
    config_json: *const c_char,
    out_context: *mut *mut c_void,
) -> *mut c_char {
    configure_allocator();

    answer((|| {
        if out_context.is_null() {
            return Err(fail("out_context is null"));
        }
        let config = match unsafe { json::optional(config_json, "config_json") }.map_err(fail)? {
            Some(text) => decode::<InitConfig>(text, "config_json").map_err(fail)?,
            None => InitConfig::default(),
        };
        let context = Context::open(config).map_err(fail)?;
        unsafe { *out_context = Box::into_raw(Box::new(context)).cast() };
        Ok(json!({}))
    })())
}

/// Destroys a context created by [`codex_init`].
///
/// # Safety
/// `context` must come from [`codex_init`] and must not be reused. Close every
/// stream and login opened from it first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_free(context: *mut c_void) {
    if !context.is_null() {
        drop(unsafe { Box::from_raw(context.cast::<Context>()) });
    }
}

/// Reports the current authentication state.
///
/// # Safety
/// `context` must come from [`codex_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_status(context: *mut c_void) -> *mut c_char {
    answer((|| auth::status(unsafe { as_context(context) }?).map_err(fail))())
}

/// Makes an OpenAI API key the active credential.
///
/// Answers `{"auth":…,"credentials":…}`; keep `credentials` to restore this
/// login later.
///
/// # Safety
/// `context` must come from [`codex_init`]; `api_key` must be a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_api_key(
    context: *mut c_void,
    api_key: *const c_char,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        let api_key = unsafe { json::required(api_key, "api_key") }.map_err(fail)?;
        auth::login_api_key(context, api_key).map_err(fail)
    })())
}

/// Accepts a personal access token or an agent-identity JWT.
///
/// Answers `{"auth":…,"credentials":…}`.
///
/// # Safety
/// `context` must come from [`codex_init`]; `access_token` must be a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_access_token(
    context: *mut c_void,
    access_token: *const c_char,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        let access_token = unsafe { json::required(access_token, "access_token") }.map_err(fail)?;
        auth::login_access_token(context, access_token).map_err(fail)
    })())
}

/// Returns the credentials this context currently holds.
///
/// Answers `{"credentials":…}`, or `{"credentials":null}` when there are none.
/// Credentials rotate on their own — a refresh triggered by an expired token
/// replaces them — so read this again after work if the stored copy matters.
///
/// # Safety
/// `context` must come from [`codex_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_export(context: *mut c_void) -> *mut c_char {
    answer((|| auth::export(unsafe { as_context(context) }?).map_err(fail))())
}

/// Takes credentials from an earlier [`codex_auth_export`] back into a context.
///
/// Answers `{"auth":…,"credentials":…}`.
///
/// # Safety
/// `context` must come from [`codex_init`]; `credentials_json` must be a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_import(
    context: *mut c_void,
    credentials_json: *const c_char,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        let credentials =
            unsafe { json::required(credentials_json, "credentials_json") }.map_err(fail)?;
        auth::import(context, credentials).map_err(fail)
    })())
}

/// Starts the ChatGPT browser login and returns `auth_url` and `port`.
///
/// # Safety
/// `context` must come from [`codex_init`]; `out_login` must point to writable
/// storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_chatgpt_begin(
    context: *mut c_void,
    out_login: *mut *mut c_void,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        if out_login.is_null() {
            return Err(fail("out_login is null"));
        }
        let (started, pending) = auth::login_chatgpt(context).map_err(fail)?;
        unsafe { *out_login = Box::into_raw(Box::new(pending)).cast() };
        Ok(started)
    })())
}

/// Starts the device-code login and returns `user_code` and `verification_url`.
///
/// # Safety
/// `context` must come from [`codex_init`]; `out_login` must point to writable
/// storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_device_begin(
    context: *mut c_void,
    out_login: *mut *mut c_void,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        if out_login.is_null() {
            return Err(fail("out_login is null"));
        }
        let (started, pending) = auth::login_device_code(context).map_err(fail)?;
        unsafe { *out_login = Box::into_raw(Box::new(pending)).cast() };
        Ok(started)
    })())
}

/// Waits for a login to finish. `timeout_ms <= 0` waits indefinitely.
///
/// Answers `{"status":"pending"}` while the user has not finished, and
/// `{"status":"completed","auth":…,"credentials":…}` once it has.
///
/// # Safety
/// `context` and `login` must come from this library and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_wait(
    context: *mut c_void,
    login: *mut c_void,
    timeout_ms: c_int,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        if login.is_null() {
            return Err(fail("login handle is null"));
        }
        let pending = unsafe { &mut *login.cast::<PendingLogin>() };
        auth::wait(context, pending, timeout_ms).map_err(fail)
    })())
}

/// Cancels a pending login and destroys the handle.
///
/// # Safety
/// `login` must come from a login-begin function and must not be reused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_login_close(login: *mut c_void) {
    if !login.is_null() {
        let pending = unsafe { Box::from_raw(login.cast::<PendingLogin>()) };
        pending.cancel();
    }
}

/// Refreshes the current credentials.
///
/// Answers `{"auth":…,"credentials":…}` with the rotated credentials.
///
/// # Safety
/// `context` must come from [`codex_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_refresh(context: *mut c_void) -> *mut c_char {
    answer((|| auth::refresh(unsafe { as_context(context) }?).map_err(fail))())
}

/// Drops the credentials this context holds.
///
/// # Safety
/// `context` must come from [`codex_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_logout(context: *mut c_void) -> *mut c_char {
    answer((|| auth::logout(unsafe { as_context(context) }?).map_err(fail))())
}

/// Returns the credential headers the provider attaches to requests.
///
/// # Safety
/// `context` must come from [`codex_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_auth_headers(context: *mut c_void) -> *mut c_char {
    answer((|| auth::headers(unsafe { as_context(context) }?).map_err(fail))())
}

/// Opens a streaming POST to `path` under the provider base URL.
///
/// `path` is relative, for example `responses`. `body_json` is sent verbatim.
/// `headers_json` is an optional `{"name":"value"}` object; `Accept` defaults to
/// `text/event-stream`. Answers the response `status` and `headers`.
///
/// # Safety
/// `context` must come from [`codex_init`]; `out_stream` must point to writable
/// storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_stream_open(
    context: *mut c_void,
    path: *const c_char,
    body_json: *const c_char,
    headers_json: *const c_char,
    out_stream: *mut *mut c_void,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        if out_stream.is_null() {
            return Err(fail("out_stream is null"));
        }
        let path = unsafe { json::required(path, "path") }.map_err(fail)?;
        let body = unsafe { as_json(body_json, "body_json") }?;
        let headers = unsafe { as_headers(headers_json) }?;

        let (opened, stream) = calls::open(context, path, body, headers)?;
        unsafe { *out_stream = Box::into_raw(Box::new(stream)).cast() };
        Ok(opened)
    })())
}

/// Reads the next stream frame. `timeout_ms <= 0` waits indefinitely.
///
/// # Safety
/// `stream` must come from [`codex_stream_open`] and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_stream_next(stream: *mut c_void, timeout_ms: c_int) -> *mut c_char {
    answer((|| {
        if stream.is_null() {
            return Err(fail("stream handle is null"));
        }
        Ok(calls::next(
            unsafe { &mut *stream.cast::<EventStream>() },
            timeout_ms,
        ))
    })())
}

/// Closes a stream and destroys the handle.
///
/// # Safety
/// `stream` must come from [`codex_stream_open`] and must not be reused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_stream_close(stream: *mut c_void) {
    if !stream.is_null() {
        drop(unsafe { Box::from_raw(stream.cast::<EventStream>()) });
    }
}

/// Performs a single non-streaming request to `path` under the provider base URL.
///
/// Answers the response `status`, `headers`, and `body` text.
///
/// # Safety
/// `context` must come from [`codex_init`]; the string arguments must be C
/// strings or null where optional.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn codex_request(
    context: *mut c_void,
    method: *const c_char,
    path: *const c_char,
    body_json: *const c_char,
    headers_json: *const c_char,
) -> *mut c_char {
    answer((|| {
        let context = unsafe { as_context(context) }?;
        let method = unsafe { json::required(method, "method") }.map_err(fail)?;
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|err| fail(format!("invalid http method `{method}`: {err}")))?;
        let path = unsafe { json::required(path, "path") }.map_err(fail)?;
        let body = unsafe { as_json(body_json, "body_json") }?;
        let headers = unsafe { as_headers(headers_json) }?;

        calls::request(context, method, path, body, headers)
    })())
}

fn answer(result: Result<Value, Value>) -> *mut c_char {
    match result {
        Ok(result) => json::ok(result),
        Err(error) => json::err(error),
    }
}

/// # Safety
/// `handle` must be null or a pointer produced by [`codex_init`].
unsafe fn as_context<'a>(handle: *mut c_void) -> Result<&'a Context, Value> {
    if handle.is_null() {
        return Err(fail("context handle is null"));
    }
    Ok(unsafe { &*handle.cast::<Context>() })
}

/// # Safety
/// `ptr` must be null or a NUL-terminated string that outlives the call.
unsafe fn as_json(ptr: *const c_char, field: &str) -> Result<Option<Value>, Value> {
    match unsafe { json::optional(ptr, field) }.map_err(fail)? {
        Some(text) => Ok(Some(decode(text, field).map_err(fail)?)),
        None => Ok(None),
    }
}

/// # Safety
/// `ptr` must be null or a NUL-terminated string that outlives the call.
unsafe fn as_headers(ptr: *const c_char) -> Result<http::HeaderMap, Value> {
    let text = unsafe { json::optional(ptr, "headers_json") }.map_err(fail)?;
    headers_from(text, "headers_json").map_err(fail)
}
