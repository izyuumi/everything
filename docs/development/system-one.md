# Optional classifier setup

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

Native key setup is a host integration API, not a current CLI command. The owner surface creates a UUID reference, builds validated settings, obtains `SystemOneSettings::credential_key`, collects the bearer secret through ephemeral secure input, and calls `NativeCredentialStore::store` with a `CredentialSecret`. It reports the actual completion or sanitized failure, retaining only the reference in configuration. It shares that native store with the selector. Deletion uses the same derived key. Changing the canonical endpoint or its query requires setting a new endpoint-bound entry. Never route secret setup through the durable conversation/remote-command pipeline or shell arguments.

On Linux, use a real unlocked Secret Service implementation on session D-Bus. On macOS, use the user's Keychain and supported OS permission prompts. A headless host without a native store can continue offline or use an explicitly approved unauthenticated compatible endpoint. A configured reference whose entry is missing fails locally; it does not silently send an unauthenticated request.

The owner surface must display the disclosure destination, model, exact task summary and tool metadata, fingerprint and authority before calling `approve`. It then supplies the current authority to `select_approved`. Declined disclosure uses the original local selector. Approval is one consumed in-memory object, lasts at most five minutes, and is never persisted as a reusable network capability. No successful live service or Apple/native-store roundtrip is implied by configuration parsing or compilation. Deferred automated verification is tracked in [#32](https://github.com/izyuumi/everything/issues/32).

Disposable Linux verification on 2026-10-15 built the workspace with Rust 1.88, checked all targets with strict Clippy, and exercised offline selection, sanitized invalid/missing settings, ordinary query parameters, prohibited endpoints, exact allowlist rebinding, expired approval, cancellation, an explicitly approved refused literal-loopback connection, and native-store unavailability on an isolated real session D-Bus. A 16-attempt burst returned 12 busy fallbacks retaining `search_tools`, with no additional catalog workers admitted. Offline syscall tracing showed only Tokio's local Unix socketpair and no Internet socket/connect/send operation. The disposable inspection program is not repository code or automated coverage. No compatible local classifier or unlocked Secret Service daemon was installed, so positive HTTP decisions and successful credential store/read/delete remain unverified; TypeSafe, macOS and security review also remain unverified.
