# codex-ffi

A native library exposing C functions over the Codex layer that talks to the model
endpoint and handles authentication. No agent, no tools, no sandbox — only the URL,
the headers, the credentials, the retries and the SSE framing. Request and response
bodies pass through as JSON text: the caller builds them and the caller parses them.

The library writes nothing anywhere. There is no `auth.json`, no OS keyring, no
config file: credentials live inside a context, every call that changes them answers
with the blob to keep, and `codex_auth_import` takes that blob back. Where it is
stored is the caller's business.

There are no Codex sources here. This repository holds only what we place **on top of**
upstream, plus the automation that builds six binaries for every stable Codex release
and publishes them as a GitHub Release.

## Layout

```
overlay/                 tree copied over a Codex checkout
  codex-rs/ffi/          our crate
  codex-rs/codex-api/…   one new file: raw.rs
patches/                 two patches against Codex sources
scripts/                 fetch-upstream → compose → build
.github/workflows/       release automation
```

Almost everything in the overlay is **new files**, and new files never conflict. The
patches are deliberately small and separate, so one of them failing to apply says
exactly what broke:

- `0001` — one line registering the crate as a workspace member, three lines in
  `codex-api` that publish `raw.rs`.
- `0002` — an optional pinned proxy on the HTTP client factory, which is what lets a
  caller route every request through a proxy of its choosing.

If upstream touches the surrounding lines, a patch will not apply and the build fails
with a clear message — nothing breaks silently.

The crate is a **member of the Codex workspace** on purpose. That way it picks up the
exact `Cargo.lock` and the exact `[patch.crates-io]` block Codex is built and tested
against. Keeping it in a separate workspace was tried and failed: a fresh dependency
resolution pulled in incompatible alpha versions and the build broke.

## Automation

Every six hours the workflow asks GitHub for the newest **stable** `openai/codex`
release (alphas are ignored — several ship per day). If that version has not been
released here yet, it clones Codex at the upstream tag, applies the overlay, builds
all six targets and publishes a release of its own.

Versions are carried over from Codex, names are not: upstream `rust-v0.147.0` becomes
tag `codex-ffi-v0.147.0`, titled `codex-ffi v0.147.0`, with the upstream tag recorded
in the release notes.

No state is stored anywhere: the existence of our release for a version is the state.

Target matrix:

| RID | Rust target | Runner |
| --- | --- | --- |
| `osx-arm64` | aarch64-apple-darwin | macos-15 |
| `osx-x64` | x86_64-apple-darwin | macos-15 (cross, same as upstream does) |
| `linux-x64` | x86_64-unknown-linux-gnu | ubuntu-latest |
| `linux-arm64` | aarch64-unknown-linux-gnu | ubuntu-24.04-arm |
| `win-x64` | x86_64-pc-windows-msvc | windows-latest |
| `win-arm64` | aarch64-pc-windows-msvc | windows-11-arm |

A release carries exactly six files named `codex_ffi-osx-arm64.dylib`,
`codex_ffi-linux-x64.so`, `codex_ffi-win-x64.dll` and so on — no archives, no header.
If any target fails to build, nothing is published at all.

Manual runs go through `workflow_dispatch`: `tag` builds a specific upstream version,
`force` rebuilds one that was already released.

## Building locally

```bash
scripts/fetch-upstream.sh rust-v0.147.0 upstream
scripts/compose.sh upstream
scripts/build.sh upstream aarch64-apple-darwin osx-arm64 dist
```

Skip the first step if you already have a Codex checkout: `compose.sh` accepts any
directory containing `codex-rs/Cargo.toml` and works in place. Note that it modifies
that directory.

## When it breaks

**A patch does not apply.** Upstream moved lines in one of the four files the patches
touch — `codex-rs/Cargo.toml`, `codex-api/src/endpoint/mod.rs`, `codex-api/src/lib.rs`
or `http-client/src/outbound_proxy.rs`. Open the file in a fresh checkout, find where
our lines belong now, and regenerate that patch.

**A dependency disappeared.** The crate takes its dependencies through
`{ workspace = true }`. If upstream renames or drops one of them (`codex-api`,
`codex-login`, `codex-model-provider`, …), the build fails during resolution — fix it
in `overlay/codex-rs/ffi/Cargo.toml`.

**A target stops building.** Linux and Windows builds have only ever run in CI, so a
platform-specific failure shows up in that job's log while the rest still finish
(`fail-fast: false`).

---

# Library API

Every function that answers returns a heap-allocated JSON string; release it with
`codex_string_free`. There is a single envelope:

```json
{"ok": true,  "result": { ... }}
{"ok": false, "error": {"message": "...", "status": 429, "body": "..."}}
```

`status` and `body` appear only when the endpoint itself returned the error.

Handles are opaque pointers. Calls against one handle must be serialized by the caller;
separate handles are independent of each other.

## Initialization

```c
void *ctx = NULL;
char *answer = codex_init("{\"originator\":\"my_app\"}", &ctx);
```

Every field of `config_json` is optional:

| Field | Meaning |
| --- | --- |
| `originator` | Value of the originator header. Set once per process. |
| `enable_codex_api_key_env` | Allow picking the key up from the environment. |
| `provider` | Provider table in the same shape as `model_providers.<id>` in `config.toml`. Defaults to the built-in `openai`. |
| `login` | `client_id`, `issuer`, `port`, `open_browser`. |
| `proxy` | `url` plus optional `no_proxy`. See below. |

A fresh context holds no credentials — log in, or import a blob you kept earlier.
Two contexts never see each other's credentials, and closing one discards its own.

Pointing at your own endpoint:

```json
{
  "provider": {
    "name": "local",
    "base_url": "http://127.0.0.1:8788/v1",
    "wire_api": "responses",
    "requires_openai_auth": true
  }
}
```

## Proxies

One `proxy` setting covers everything the library sends — model requests, the OAuth
login round trips, and background token refreshes:

```json
{
  "proxy": {
    "url": "socks5h://user:secret@127.0.0.1:1080",
    "no_proxy": "localhost,127.0.0.1"
  }
}
```

`http`, `https`, `socks5` and `socks5h` are accepted, credentials may be embedded in
the URL, and `no_proxy` takes the same comma-separated host list as the environment
variable of that name. The setting belongs to one context, so two contexts can use
different proxies in the same process.

Omit `proxy` and reqwest's own behavior applies, which reads `HTTP_PROXY`,
`HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` from the environment.

## Authentication

```c
codex_auth_status(ctx);                        /* mode, account, email, plan */
codex_auth_login_api_key(ctx, "sk-...");
codex_auth_login_access_token(ctx, "...");     /* PAT or agent-identity JWT */
codex_auth_refresh(ctx);
codex_auth_logout(ctx);
codex_auth_headers(ctx);                       /* ready-made auth headers */
```

Everything that changes credentials — the two logins above, `codex_auth_refresh`,
`codex_auth_import`, and a finished interactive login — answers in the same shape:

```json
{"auth": {"authenticated": true, "mode": "chatgpt", "…": "…"},
 "credentials": {"…": "…"}}
```

`auth` is state you can show a user; `credentials` is the opaque blob to persist.
Its shape belongs to Codex, so treat it as a token: store it, do not interpret it.

```c
codex_auth_export(ctx);                        /* {"credentials": … } or null */
codex_auth_import(ctx, credentials_json);      /* restores a kept blob */
```

Credentials rotate on their own: a ChatGPT access token that expires is refreshed
in the middle of a request, and the blob you kept goes stale. Read
`codex_auth_export` again after doing work if the stored copy has to stay valid.

Interactive login takes two steps: get the URL, then wait for the outcome. The
bounded wait can be repeated as many times as needed.

```c
void *login = NULL;
codex_auth_login_chatgpt_begin(ctx, &login);   /* -> {"auth_url":"https://...","port":1455} */
codex_auth_login_wait(ctx, login, 1000);       /* {"status":"pending"} … then status/auth/credentials */
codex_auth_login_close(login);                 /* cancels if unfinished, then frees */
```

Device code works the same way, except `codex_auth_login_device_begin` returns
`user_code` and `verification_url`.

## Requests

```c
codex_request(ctx, "GET", "models", NULL, NULL);
/* {"status":200,"headers":{...},"body":"{...}"} */

void *stream = NULL;
codex_stream_open(ctx, "responses", body_json, NULL, &stream);
codex_stream_next(stream, 0);                  /* 0 — wait as long as it takes */
codex_stream_close(stream);
```

A frame's `status` takes one of four values: `event` (`data` holds the SSE payload
exactly as it came off the wire), `pending` (the timeout elapsed), `closed` (the source
is done), `error`.

One behavior worth knowing: the Codex SSE reader reports the end of the HTTP stream as
an error. A normally finished response therefore looks like a `response.completed`
event followed by an `error` frame reading `stream closed before completion`. That is
upstream behavior and it is passed through unchanged.

Headers are given as a `{"name":"value"}` object. For streams `Accept` defaults to
`text/event-stream` unless the caller sets its own.

## C# example

```csharp
using System.Runtime.InteropServices;

internal static class Native
{
    private const string Lib = "codex_ffi";

    [DllImport(Lib)] internal static extern IntPtr codex_init(string? configJson, out IntPtr context);
    [DllImport(Lib)] internal static extern void codex_free(IntPtr context);
    [DllImport(Lib)] internal static extern IntPtr codex_auth_status(IntPtr context);
    [DllImport(Lib)] internal static extern IntPtr codex_auth_export(IntPtr context);
    [DllImport(Lib)] internal static extern IntPtr codex_auth_import(IntPtr context, string credentialsJson);
    [DllImport(Lib)] internal static extern IntPtr codex_stream_open(IntPtr context, string path, string? bodyJson, string? headersJson, out IntPtr stream);
    [DllImport(Lib)] internal static extern IntPtr codex_stream_next(IntPtr stream, int timeoutMs);
    [DllImport(Lib)] internal static extern void codex_stream_close(IntPtr stream);
    [DllImport(Lib)] internal static extern void codex_string_free(IntPtr text);

    internal static string Take(IntPtr pointer)
    {
        try { return Marshal.PtrToStringUTF8(pointer) ?? string.Empty; }
        finally { codex_string_free(pointer); }
    }
}
```

The full list of functions lives in `overlay/codex-rs/ffi/include/codex_ffi.h`.
