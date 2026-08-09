/* C ABI over the Codex endpoint and authentication layer.
 *
 * Every function that answers returns a heap JSON string that the caller
 * releases with codex_string_free. The envelope is either
 *   {"ok":true,"result":...}
 * or
 *   {"ok":false,"error":{"message":"...","status":<int>,"body":"..."}}
 * where "status" and "body" appear only for failures reported by the endpoint.
 *
 * Handles are opaque. Calls on one handle must be serialized by the caller;
 * separate handles are independent.
 *
 * Nothing is written to disk or to the OS keyring. Credentials live in the
 * context and vanish with it: every call that changes them answers with the
 * blob to keep, and codex_auth_import takes that blob back.
 */

#ifndef CODEX_FFI_H
#define CODEX_FFI_H

#ifdef __cplusplus
extern "C" {
#endif

/* Releases a string returned by this library. */
void codex_string_free(char *text);

/* Creates a context. config_json may be NULL. */
char *codex_init(const char *config_json, void **out_context);

/* Destroys a context. Close its streams and logins first. */
void codex_free(void *context);

/* Authentication. The calls that change credentials answer with
 * {"auth":...,"credentials":...}; keep "credentials" to restore the login. */
char *codex_auth_status(void *context);
char *codex_auth_login_api_key(void *context, const char *api_key);
char *codex_auth_login_access_token(void *context, const char *access_token);
char *codex_auth_export(void *context);
char *codex_auth_import(void *context, const char *credentials_json);
char *codex_auth_login_chatgpt_begin(void *context, void **out_login);
char *codex_auth_login_device_begin(void *context, void **out_login);
char *codex_auth_login_wait(void *context, void *login, int timeout_ms);
void codex_auth_login_close(void *login);
char *codex_auth_refresh(void *context);
char *codex_auth_logout(void *context);
char *codex_auth_headers(void *context);

/* Streaming request. path is relative to the provider base URL, e.g. "responses". */
char *codex_stream_open(void *context,
                        const char *path,
                        const char *body_json,
                        const char *headers_json,
                        void **out_stream);
char *codex_stream_next(void *stream, int timeout_ms);
void codex_stream_close(void *stream);

/* Single non-streaming request. */
char *codex_request(void *context,
                    const char *method,
                    const char *path,
                    const char *body_json,
                    const char *headers_json);

#ifdef __cplusplus
}
#endif

#endif /* CODEX_FFI_H */
