# Optional classifier setup

## Headless Host

The Host does not read a TOML file. Its selection configuration is the `selection` field of the revisioned owner settings (`{"kind":"local"}` by default, or `system_one` with an optional `provider`, `endpoint`, `model` and an optional `credential_reference`). A raw owner `replace_settings` that turns System One on or changes its endpoint/model returns `host_action_required` and changes nothing. The change is applied only through the native Settings route: an `update` of `tools.selection` returns `awaiting_confirmation`, and the matching `confirm_patch` applies it. The macOS Settings › Tools provider picker drives that route. Local is the default; choosing it saves `{"kind":"local"}` and creates no credential, client or request. TypeSafe fills `https://api.typesafe.ai/v1/systemone` and `jev-latest` (both editable). OpenRouter shows its fixed endpoint and a model field. OpenAI-compatible takes a base URL and a model. Local model takes a loopback base URL and a model and hides key entry. Choosing a provider again restores its saved fields. The client checks the same endpoint and model rules as the Host before Save, and if the Host still rejects the patch with "System One provider endpoint", the operation shows that message. Switching to Local leaves the stored endpoint/model but stops remote admission.

The `provider` field picks the service. It is omitted for `typesafe`, so older clients keep reading and writing TypeSafe-style settings unchanged. Native `tools.selection` update values per provider:

| Provider | Value |
| --- | --- |
| TypeSafe/Jev (default wire) | `{"kind":"system_one","endpoint":"https://api.typesafe.ai/v1/systemone","model":"jev-latest"}` |
| OpenRouter | `{"kind":"system_one","provider":"openrouter","endpoint":"https://openrouter.ai/api/v1","model":"openai/gpt-4.1-mini"}` |
| OpenAI-compatible | `{"kind":"system_one","provider":"openai_compatible","endpoint":"https://llm.example.com/v1","model":"MODEL_ID"}` |
| Local model | `{"kind":"system_one","provider":"local_model","endpoint":"http://127.0.0.1:11434/v1","model":"llama3.2"}` |

Each goes inside `{"kind":"tool_selection","selection":…}` in an `update` of `tools.selection`, followed by the matching `confirm_patch`. A changed provider is a protected change like a changed endpoint. For OpenAI-style providers the endpoint is the base URL, not the completions URL (a path ending in `/chat/completions` is rejected with "System One provider endpoint"); the Host POSTs to `<base>/chat/completions` and the disclosure `destination` shows that full URL. A key for OpenRouter or a keyed OpenAI-compatible server uses the same System One credential entry bound to the endpoint plus reference; a local model needs none. The macOS picker sets the provider. iOS shows it read-only: paired devices cannot write `tools.selection`.

With remote selection enabled, `prepare_selection` returns the exact disclosure without contacting the endpoint. Only `approve_selection` for that ticket sends one POST. The identical domain command then consumes the result, and the model receives the selected definitions plus `search_tools`. Declining with `discard_selection`, or never approving, keeps the ordinary local selection for that command. Approved or local tickets that are never submitted keep their reservation until discarded or expired. When the preflight budget is full, `prepare_selection` fails with code `busy` and "selection preflight capacity is exhausted; discard unused selection tickets". A consumed, discarded, expired or changed ticket fails with `invalid_request` and asks for a new preparation.

The macOS Settings app requests System One key entry as a native credential action bound to the configured endpoint and a reference UUID. The secret travels only over the private native pipe into the platform store. On macOS that store is the user's login Keychain. The sequence is: native `request_action` (credential entry), then `details`, then `submit_credential`. A separate revision-checked owner settings write then sets `selection.credential_reference`. The key is bound to the exact endpoint plus reference. Changing the endpoint or the provider through native update clears the reference, and the macOS picker says that a new key must be entered after confirmation. The old key is not deleted: it stays in the native store (Keychain or Secret Service) until `remove_system_one` removes it for the old endpoint and reference. Attaching the old reference to a new endpoint finds no key, so selection falls back locally and nothing is sent. To delete the key, send the credential operation below over `owner --sensitive-stdin`:

```json
{"kind":"credential","request_id":"UNIQUE_UUID","operation":{"kind":"remove_system_one","endpoint":"https://exact.configured/endpoint","reference":"REFERENCE_UUID"}}
```

Removal derives the same endpoint-bound key and deletes it from the native store. Deleting an absent entry also returns `applied`.

## Library configuration document

Offline discovery is the default and needs no service or key. `provider` is `local`, `system-one` or `typesafe` (TypeSafe wire), `openrouter`, `openai-compatible` or `openai_compatible`, `local-model` or `local_model`:

```toml
[tools.selection]
provider = "local"
```

A dedicated selection configuration document accepts only `tools.selection` and its documented fields. Unknown fields, including `api_key`, are rejected with sanitized errors that never reproduce the TOML line. The maximum document size is 16 KiB. A host loading a larger application configuration should extract and validate this dedicated document at its configuration boundary. `SystemOneSelector::from_toml` preserves offline operation and adds a diagnostic for malformed settings.

TypeSafe uses the same generic adapter as custom endpoints:

```toml
[tools.selection]
provider = "system-one"
endpoint = "https://api.typesafe.ai/v1/systemone"
model = "jev-latest"
fallback = "local"
credential_reference = "f1b9f4bd-0be0-4a78-95e4-7cf1bc2f4857"
```

`SelectionConfig::typesafe(reference)` is an equivalent host convenience. A custom model can be any nonempty ID of at most 256 bytes without control characters. The endpoint is the full POST URL; there is no automatic suffix or model discovery. Compatible API-version query parameters are allowed, bounded to 1024 bytes and 32 pairs. URL userinfo, fragments and conventional credential query keys are rejected. Credentials belong exclusively in the native store and authorization header; never embed a secret in the path, model or ordinary query value.

A compatible local server can omit `credential_reference`:

```toml
[tools.selection]
provider = "system-one"
endpoint = "http://127.0.0.1:8080/v1/systemone"
model = "user-installed-model"
fallback = "local"
```

A loopback OpenAI-compatible model server uses the chat-completions wire:

```toml
[tools.selection]
provider = "local-model"
endpoint = "http://127.0.0.1:11434/v1"
model = "llama3.2"
fallback = "local"
```

Plain HTTP requires a literal loopback IP such as `127.0.0.1` or `[::1]`; `localhost`, abbreviated numeric hosts, LAN addresses and public HTTP are rejected. HTTPS uses normal TLS certificate verification. Every destination, including loopback and TypeSafe, still requires a trusted owner disclosure approval for the exact payload. Custom HTTPS may refer to internal networks only after that explicit approval; configuration does not silently authorize a request.

Embedders using the library directly set up keys through the host integration API. The owner surface creates a UUID reference, builds validated settings, obtains `SystemOneSettings::credential_key`, collects the bearer secret through ephemeral secure input, and calls `NativeCredentialStore::store` with a `CredentialSecret`. It reports the actual completion or sanitized failure, retaining only the reference in configuration. It shares that native store with the selector. Deletion uses the same derived key. Changing the canonical endpoint or its query requires setting a new endpoint-bound entry. Never route secret setup through the durable conversation/remote-command pipeline or shell arguments.

On Linux, use a real unlocked Secret Service implementation on session D-Bus. On macOS, use the user's Keychain and supported OS permission prompts. A headless host without a native store can continue offline or use an explicitly approved unauthenticated compatible endpoint. A configured reference whose entry is missing fails locally; it does not silently send an unauthenticated request.

The owner surface must display the disclosure destination, model, exact task summary and tool metadata, fingerprint and authority before calling `approve`. It then supplies the current authority to `select_approved`. Declined disclosure uses the original local selector. Approval is one consumed in-memory object, lasts at most five minutes, and is never persisted as a reusable network capability. No successful live service or Apple/native-store roundtrip is implied by configuration parsing or compilation. Deferred automated verification is tracked in [#32](https://github.com/izyuumi/everything/issues/32).

Disposable Linux verification on 2026-10-15 built the workspace with Rust 1.88, checked all targets with strict Clippy, and exercised offline selection, sanitized invalid/missing settings, ordinary query parameters, prohibited endpoints, exact allowlist rebinding, expired approval, cancellation, an explicitly approved refused literal-loopback connection, and native-store unavailability on an isolated real session D-Bus. A 16-attempt burst returned 12 busy fallbacks retaining `search_tools`, with no additional catalog workers admitted. Offline syscall tracing showed only Tokio's local Unix socketpair and no Internet socket/connect/send operation. The disposable inspection program is not repository code or automated coverage. No compatible local classifier or unlocked Secret Service daemon was installed, so positive HTTP decisions and successful credential store/read/delete remain unverified; TypeSafe, macOS and security review also remain unverified.

## macOS loopback observations

On 2026-10-17 a debug build of the branch ran on macOS (Apple Silicon) as a real `serve` Host with fresh disposable state, using the real `owner` CLI and the native `owner --sensitive-stdin` route over private pipes. A scratch loopback server, not repository code, implemented the System One contract and also recorded requests to a stand-in OpenAI-compatible model endpoint that always returned HTTP 503. Observed:

- Local mode and configuration made no classifier requests. `lsof` showed no Internet sockets on the Host in local mode, after System One was configured, and while a TypeSafe-preset disclosure was pending. Local `prepare_selection` returned `read_file`/`agent_status` without write, shell or MCP tools.
- Raw `replace_settings` enabling System One and disabling it both returned `host_action_required`. Native update plus confirmation applied the custom loopback endpoint with model `user/custom-model:v7`, then the TypeSafe preset, then off, then the custom endpoint again.
- Each approval sent exactly one POST whose body equalled the disclosed payload, with the arbitrary model ID and no `Authorization` header. A disposable session key configured for the loopback model endpoint reached only that endpoint.
- A choice answer for `read_file` and a probabilities answer for `search_files` returned `method: system_one` with that tool. A `none` answer returned no tools. After the identical command was submitted, the model request listed `read_file` and `search_tools`.
- An unknown choice (`shell`), inconsistent probabilities, malformed JSON, HTTP 500, a response over 64 KiB, and a response held for 14 s each returned `local_fallback` with the local tools and a static diagnostic. The held response gave up at 10.0 s. After an unknown choice, the model received the local tools plus `search_tools`. The service's choice never added a tool outside the permitted candidates.
- A discarded ticket could not be approved, made no POST, and its command later ran with local tools plus `search_tools`. A second approval of the same ticket made no extra POST. Killing the approving CLI during a held request left zero tickets and active requests. Five unsubmitted tickets filled the 8 MiB budget, the sixth preparation returned the new `busy` message, and discarding them restored zero reservation.
- With an approved request held open, SIGINT stopped the Host with exit 0 in about 0.02 s. The held client received `cancelled`. The disposable model key did not appear in any state, settings or temporary file afterwards.

A second fresh-state run used the dummy key `dummy-sysone-key-20` and the Host with the real user `HOME` so it could reach the login Keychain. Observed:

- Native credential entry completed: `request_action`, `details`, then `submit_credential`, with a `completed` receipt. Linking the reference through the owner settings write was applied.
- An approved classification to `/v1/systemone` carried `Authorization: Bearer` with the dummy key.
- A ticket prepared before a native endpoint change to `/v2/systemone` could no longer be approved and sent nothing. The endpoint change cleared the reference, so the next approved request to `/v2` had no `Authorization` header.
- Attaching the same reference to `/v2` through an owner write fell back with "System One credential is missing" and sent no request.
- `remove_system_one` returned `applied`. After relinking on `/v1`, selection fell back with "credential is missing" and sent nothing. A second removal also returned `applied`.
- With a private temporary `HOME` in an earlier run, the native lookup failed and selection fell back locally without sending anything.
- The dummy key appeared in no file under the disposable state, temporary or log directories, and in no captured CLI stdout/stderr. Keychain contents were not read independently.

The TypeSafe endpoint was never approved or contacted. Not exercised: the macOS Settings UI itself, HTTPS endpoints, and an actual successful model turn. This walkthrough is not automated coverage. The Host's `lsof` snapshots sample socket state at a few points; they are not a full network trace.

## Provider choice observations

On 2026-11-01 a debug build of commit `b350db7` ([#292](https://github.com/izyuumi/everything/issues/292)) ran on macOS (Apple Silicon) as a real `serve` Host with fresh disposable state and a cleared environment. Tools were enabled with a raw owner `replace_settings`. Each provider was configured through native `update` plus `confirm_patch` of `tools.selection` over `owner --sensitive-stdin`. A scratch loopback chat-completions server, not repository code, logged every request. Observed:

- Never configured, `prepare_selection` returned `method: local`. The scratch server received nothing, and `lsof` showed no Internet sockets on the Host.
- The stored TypeSafe selection omitted `provider`. OpenRouter, OpenAI-compatible and local-model selections stored `provider` with the endpoint and model and no secret. OpenRouter with another endpoint and local-model with a non-loopback HTTPS endpoint were rejected as invalid patches.
- The TypeSafe disclosure named provider `typesafe`, its endpoint and the TypeSafe body. The OpenRouter disclosure named `https://openrouter.ai/api/v1/chat/completions` and the full chat-completions body with the `json_schema` enum of option IDs. Both were discarded without approval. The Host held no Internet socket, and a later approval of the discarded ticket was rejected.
- For OpenAI-compatible and local-model against the scratch server, approval sent one POST to `/v1/chat/completions` with no `Authorization` header. Its body was byte-identical to the disclosed payload. The answer `tool_000` returned `method: system_one` with `read_file`.
- An approved request to a loopback port with no listener, and an out-of-set answer `{"choice":"shell"}`, each returned `local_fallback` with the local tools and the static transport or malformed-answer diagnostic. A discarded local-model ticket sent nothing.
- The Host stopped on SIGINT with exit 0.

Live TypeSafe and OpenRouter inference was not observed. Not exercised: HTTPS endpoints, a Keychain credential with the new providers, the identical domain command consuming the result in a model turn, and the macOS Settings UI.

## Provider picker observations

On 2026-11-01 ([#296](https://github.com/izyuumi/everything/issues/296)) a debug build of the branch ran as a disposable `serve` Host with a temporary state directory and `HOME`. A Debug VorraMac build with a separate bundle identifier pointed at it. In Settings › Tools, the picker was driven through accessibility, then Save and Confirm original changes were pressed. Observed:

- Fresh settings showed `Local`, and the owner `settings` query returned `{"kind":"local"}`.
- Choosing OpenRouter showed the fixed endpoint and a model field. Before a model was entered, the model warning blocked Save. Saving `openai/gpt-4.1-mini` and confirming the change, whose text read `System One · OpenRouter · openai/gpt-4.1-mini · https://openrouter.ai/api/v1`, stored `provider: openrouter` with that endpoint and model and `credential_reference: null` at revision 2.
- Choosing OpenAI-compatible with base URL `http://127.0.0.1:18921/v1/Chat/Completions` showed "Enter the base URL, not the /chat/completions URL." and Save stayed disabled.
- Choosing Local model hid key entry. Saving `http://127.0.0.1:11434/v1` with `llama3.2` stored `provider: local_model` at revision 3. No model server was running, and no classification was requested.
- A native `update` with that mixed-case completions URL, sent directly through `owner --sensitive-stdin`, exited 1 with `invalid native settings patch: System One provider endpoint` on stderr. The macOS client maps this message to its inline rejection. The same update with `/v1` was accepted and left unconfirmed.
- A scratch library program accepted both TOML spellings of the two providers and rejected `/v1/Chat/Completions` and `/v1/CHAT/COMPLETIONS/` with `ProviderEndpoint`.

Light and dark appearance were both checked on macOS. Not exercised: iOS screens (no Simulator UI was available for taps, so no paired phone read the Host value), the Mac's inline display of an actual Host rejection (client validation blocks the known cases first), key entry with the new providers, and any live provider.
