# ADR-0014: Optional Sign in with ChatGPT for the OpenAI provider

Status: Accepted. A real ChatGPT login, token refresh and live plan-usage inference were observed on the owner's Mac; revocation and live SIWC tool calls are unverified ([#201](https://github.com/izyuumi/everything/issues/201)).

## Context

OpenAI offers "ChatGPT plan usage" for open-source tools and personal projects that run locally: a user signs in with ChatGPT and eligible Responses API requests use their ChatGPT plan instead of an API key. It is a preview. Paid or remotely hosted apps must join OpenAI's [waitlist](https://openai.com/form/sign-in-with-chatgpt-interest/). The Codex CLI uses a different route: a first-party OAuth client ID, `/oauth/authorize` parameters and the private `chatgpt.com/backend-api/codex` endpoint. Reusing that from a third-party app impersonates Codex and is not an approved use.

## Decision

SIWC is one optional model credential source for the official OpenAI endpoint. It is never required, prompted for or selected automatically.

- **Selection.** A `ModelCredentialReference` gains an additive `source` field (`api_key` by default, omitted when serialized; `chatgpt` for SIWC). A `chatgpt` binding is valid only for `openai` with the official endpoint. Existing settings, wire formats and API-key keys are unchanged. Removing the binding returns OpenAI to API keys.
- **Flow.** Only the documented open-source flow: Authorization Code + PKCE S256 + OIDC nonce at `https://auth.openai.com/api/accounts/authorize`, dynamic registration with `client_id=dynamic_agent_client`, `agent_name_hint=Everything` and a persisted `ext_agent_host_id`, the issued `client_id` saved and reused, a `127.0.0.1` loopback callback, code exchange and refresh at `/api/accounts/oauth/token` with `resource=https://api.openai.com/v1`, and revocation through the discovered `revocation_endpoint`. The Codex client ID, its parameters and `chatgpt.com/backend-api` are not used.
- **Storage.** The token record goes through the existing credential boundary (session memory or the native store, chosen by the binding's `storage`), never TOML, arguments, logs, events, checkpoints or relay messages. The host ID is a nonsecret `urn:uuid` file in the state directory.
- **Inference.** The existing Responses adapter sends `Authorization: Bearer <access_token>` to the official endpoint. Only in SIWC mode it omits `truncation`, `max_output_tokens` and `temperature` and sends system messages as `developer` messages. Like Codex, SIWC mode does not require a `text/event-stream` content type: a JSON error or Responses object is mapped, and other bodies go to the SSE parser. It also skips unrecognised stream events and output items, as Codex does, and groups function tools in one `functions` namespace as the preview requires. Models come from `GET /v1/models` (`everything chatgpt models`), shown as `visibility == "list"` entries in server order. API-key requests are byte-for-byte unchanged.
- **ID-token validation** uses `ring` (already in `Cargo.lock` through rustls) for RS256 against OpenAI's JWKS. No JWT or OAuth crate is added.

## Consequences

Users without an API key can use their ChatGPT plan locally. Everything else keeps working without a ChatGPT account. The host owns refresh, serialized in-process, so long tasks get a fresh token before each model request. Preview restrictions (tool shapes, other unsupported fields) can change and may reject requests until the adapter is adjusted. One ChatGPT account per binding is supported; an account picker is not implemented.
