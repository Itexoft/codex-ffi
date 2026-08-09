//! Conversions between the C boundary and JSON.
//!
//! Every exported function answers with one JSON envelope: `{"ok":true,"result":…}`
//! or `{"ok":false,"error":{"message":…}}`. Failures coming from the endpoint
//! also carry `status` and `body`. The caller owns the returned pointer until it
//! passes it to `codex_string_free`.

use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::raw::c_char;

pub(crate) fn ok(result: Value) -> *mut c_char {
    render(json!({ "ok": true, "result": result }))
}

pub(crate) fn err(error: Value) -> *mut c_char {
    render(json!({ "ok": false, "error": error }))
}

/// Builds the error body carried by a failed envelope.
pub(crate) fn fail(message: impl std::fmt::Display) -> Value {
    json!({ "message": message.to_string() })
}

fn render(value: Value) -> *mut c_char {
    CString::new(value.to_string())
        .unwrap_or_default()
        .into_raw()
}

/// Reclaims a string handed out by [`render`].
///
/// # Safety
/// `ptr` must come from this library and must not be reused afterwards.
pub(crate) unsafe fn reclaim(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// Borrows a caller-owned string that must be present.
///
/// # Safety
/// `ptr` must be null or a NUL-terminated string that outlives the call.
pub(crate) unsafe fn required<'a>(ptr: *const c_char, field: &str) -> Result<&'a str, String> {
    match unsafe { optional(ptr, field)? } {
        Some(text) => Ok(text),
        None => Err(format!("`{field}` is required")),
    }
}

/// Borrows a caller-owned string that may be absent.
///
/// # Safety
/// `ptr` must be null or a NUL-terminated string that outlives the call.
pub(crate) unsafe fn optional<'a>(
    ptr: *const c_char,
    field: &str,
) -> Result<Option<&'a str>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(Some)
        .map_err(|err| format!("`{field}` is not valid UTF-8: {err}"))
}

pub(crate) fn decode<T: DeserializeOwned>(text: &str, field: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|err| format!("`{field}` is not valid JSON: {err}"))
}

/// Reads a `{"name":"value"}` object into request headers.
pub(crate) fn headers_from(text: Option<&str>, field: &str) -> Result<HeaderMap, String> {
    let Some(text) = text else {
        return Ok(HeaderMap::new());
    };
    let fields: serde_json::Map<String, Value> = decode(text, field)?;

    let mut headers = HeaderMap::new();
    for (name, value) in fields {
        let Value::String(value) = value else {
            return Err(format!("header `{name}` must hold a string"));
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| format!("invalid header name `{name}`: {err}"))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|err| format!("invalid value for header `{name}`: {err}"))?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

/// Renders response headers, dropping values that are not valid UTF-8.
pub(crate) fn headers_to_json(headers: &HeaderMap) -> Value {
    let fields = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), Value::String(value.to_string())))
        })
        .collect::<serde_json::Map<String, Value>>();
    Value::Object(fields)
}
