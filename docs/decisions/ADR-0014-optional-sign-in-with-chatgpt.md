# ADR-0014: Optional Sign in with ChatGPT for the OpenAI provider

Status: Accepted. A real ChatGPT login, token refresh and live plan-usage inference were observed on the owner's Mac; revocation and live SIWC tool calls are unverified ([#201](https://github.com/izyuumi/everything/issues/201)).

## Context

OpenAI offers "ChatGPT plan usage" for open-source tools and personal projects that run locally: a user signs in with ChatGPT and eligible Responses API requests use their ChatGPT plan instead of an API key. It is a preview. Paid or remotely hosted apps must join OpenAI's [waitlist](https://openai.com/form/sign-in-with-chatgpt-interest/). The Codex CLI uses a different route: a first-party OAuth client ID, `/oauth/authorize` parameters and the private `chatgpt.com/backend-api/codex` endpoint. Reusing that from a third-party app impersonates Codex and is not an approved use.

## Decision

SIWC is one optional model credential source for the official OpenAI endpoint. It is never required, prompted for or selected automatically.

- **Selection.** A `ModelCredentialReference` gains an additive `source` field (`api_key` by default, omitted when serialized; `chatgpt` for SIWC). A `chatgpt` binding is valid only for `openai` with the official endpoint. Existing settings, wire formats and API-key keys are unchanged. Removing the binding returns OpenAI to API keys.
- **Flow.** Only the documented open-source flow: Authorization Code + PKCE S256 + OIDC nonce at `https://auth.openai.com/api/accounts/authorize`, dynamic registration with `client_id=dynamic_agent_client`, `agent_name_hint=Everything` and a persisted `ext_agent_host_id`, the issued `client_id` saved and reused, a `127.0.0.1` loopback callback, code exchange and refresh at `/api/accounts/oauth/token` with `resource=https://api.openai.com/v1`, and revocation through the discovered `revocation_endpoint`. The Codex client ID, its parameters and `chatgpt.com/backend-api` are not used.
- **Storage.** The token record goes through the existing credential boundary (session memory or the native store, chosen by the binding's `storage`; new CLI bindings default to `native`), never TOML, arguments, logs, events, checkpoints or relay messages. The host ID is a nonsecret `urn:uuid` file in the state directory.
- **Inference.** The existing Responses adapter sends `Authorization: Bearer <access_token>` to the official endpoint. Only in SIWC mode it omits `truncation`, `max_output_tokens` and `temperature` and sends system messages as `developer` messages. Like Codex, SIWC mode does not require a `text/event-stream` content type: a JSON error or Responses object is mapped, and other bodies go to the SSE parser. It also skips unrecognised stream events and output items, as Codex does, and groups function tools in one `functions` namespace as the preview requires. Models come from `GET /v1/models` (`everything chatgpt models`), shown as `visibility == "list"` entries in server order. API-key requests are byte-for-byte unchanged.
- **ID-token validation** uses `ring` (already in `Cargo.lock` through rustls) for RS256 against OpenAI's JWKS. No JWT or OAuth crate is added.

## Consequences

Users without an API key can use their ChatGPT plan locally. Everything else keeps working without a ChatGPT account. The host owns refresh, serialized in-process, so long tasks get a fresh token before each model request. Preview restrictions (tool shapes, other unsupported fields) can change and may reject requests until the adapter is adjusted. One ChatGPT account per binding is supported; an account picker is not implemented.

## Credential durability

Amended 2026-10-31. A sign-in must survive host restarts, rebuilds and updates.

- **Refresh is never dropped.** The in-memory record is replaced before the native write and marked unsaved until the write succeeds. A failed refresh write (for example a busy store or a keychain access denial) is logged as a warning, the rotated tokens keep serving requests for this host run, and every later bearer use retries the save. Sign-in and sign-out writes still return the unsaved error. Previously the cache entry was removed before the write, so a failed write lost the rotated refresh token and the next start hit `refresh_token_reused`.
- **Native by default.** `everything chatgpt sign-in` without `--storage` creates a `native` binding. The Mac app already did.
- **Stable code signing on macOS.** The keychain item's access list trusts the creating binary's designated requirement. Binaries are signed with the fixed identifier `to.yumi.everything.cli` and one Apple certificate through `scripts/sign_macos.sh`, which local builds and future release CI share.

Alternatives rejected: an access list that trusts every application removes the only per-item protection against other processes of the user; the data protection keychain needs a keychain access group entitlement and a provisioning profile, which a plain CLI binary cannot carry; a file outside the keychain would be custom secret storage.

Observed on macOS 27.2 (2026-10-31) with a disposable probe using the same `keyring` 3.6.3 `MacCredential` path, service `ai.everything.acltest.disposable`, user interaction disabled so prompts surface as errors; build 1 and build 2 differ only in an embedded string, so their code hashes differ:

| Writer | Reader | Result |
| --- | --- | --- |
| ad-hoc build 1 | ad-hoc build 1 | read ok |
| ad-hoc build 1 | ad-hoc build 2 | read `-25293` errSecAuthFailed; write `-25299` duplicate item (update denied) |
| Apple Development build 1 | Apple Development build 2 (same identifier) | read ok; update ok, and build 1 then read build 2's value |
| Developer ID build 1 | Developer ID build 2 (same identifier) | read ok |
| Apple Development build 1 | Developer ID build 2 | read `-25293` |
| ad-hoc build 1 | Apple Development build 2 | read `-25293` |

Interactive runs were not exercised, to avoid leaving a dialog on the owner's screen; by the platform's documented behavior the `-25293` cases become a keychain access prompt, which a host reading in the background meets as the three-second read timeout or a failure. A real ChatGPT sign-in across a rebuild was not repeated, because sign-in needs a browser.
