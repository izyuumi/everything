# Optional classifier setup

## Headless Host

The Host does not read a TOML file. Its selection configuration is the `selection` field of the revisioned owner settings (`{"kind":"local"}` by default, or `system_one` with `endpoint`, `model` and an optional `credential_reference`). A raw owner `replace_settings` that turns System One on or changes its endpoint/model returns `host_action_required` and changes nothing. The change is applied only through the native Settings route: an `update` of `tools.selection` returns `awaiting_confirmation`, and the matching `confirm_patch` applies it. The macOS Settings Tool Selection controls drive that route, including the "Use TypeSafe preset" button, which fills `https://api.typesafe.ai/v1/systemone` and `jev-latest` into the same fields as a custom endpoint. Turning System One off through the same route leaves the stored endpoint/model but stops remote admission.

With remote selection enabled, `prepare_selection` returns the exact disclosure without contacting the endpoint. Only `approve_selection` for that ticket sends one POST. The identical domain command then consumes the result, and the model receives the selected definitions plus `search_tools`. Declining with `discard_selection`, or never approving, keeps the ordinary local selection for that command. Approved or local tickets that are never submitted keep their reservation until discarded or expired. When the preflight budget is full, `prepare_selection` fails with code `busy` and "selection preflight capacity is exhausted; discard unused selection tickets". A consumed, discarded, expired or changed ticket fails with `invalid_request` and asks for a new preparation.

The macOS Settings app requests System One key entry as a native credential action bound to the configured endpoint and a reference UUID. The secret travels only over the private native pipe into the platform store. On macOS that store is the user's login Keychain.

## Library configuration document

Offline discovery is the default and needs no service or key:

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

Plain HTTP requires a literal loopback IP such as `127.0.0.1` or `[::1]`; `localhost`, abbreviated numeric hosts, LAN addresses and public HTTP are rejected. HTTPS uses normal TLS certificate verification. Every destination, including loopback and TypeSafe, still requires a trusted owner disclosure approval for the exact payload. Custom HTTPS may refer to internal networks only after that explicit approval; configuration does not silently authorize a request.

Embedders using the library directly set up keys through the host integration API; there is no plain CLI key command. The owner surface creates a UUID reference, builds validated settings, obtains `SystemOneSettings::credential_key`, collects the bearer secret through ephemeral secure input, and calls `NativeCredentialStore::store` with a `CredentialSecret`. It reports the actual completion or sanitized failure, retaining only the reference in configuration. It shares that native store with the selector. Deletion uses the same derived key. Changing the canonical endpoint or its query requires setting a new endpoint-bound entry. Never route secret setup through the durable conversation/remote-command pipeline or shell arguments.

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

The TypeSafe endpoint was never approved or contacted. Not exercised: native Keychain store/read with a configured `credential_reference`, the macOS Settings UI itself, HTTPS endpoints, and an actual successful model turn. This walkthrough is not automated coverage. The Host's `lsof` snapshots sample socket state at a few points; they are not a full network trace.
