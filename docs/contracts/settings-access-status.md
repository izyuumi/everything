# Held AccessStatus request source

`src/devices/settings_ingress/access.rs` defines the strict generation-free AccessStatus request codec and its genuine owned verification source. This prerequisite does not enable accepted Delivery, store a receipt, register a build, read grants or produce a summary/denial. The original Bind/command decoders, factories, routing, crypto and receive guard remain unchanged.

## Closed request

The complete JSON shape is:

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version=1`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=query`, `query_id`, `body` |
| Body | `kind=access_status`, `time` |
| Time | `issued_at`, `expires_at` |

Every object is required to be an object; positional arrays, missing/unknown/duplicate fields and nonstring kind values fail. The decoder uses existing serde object and canonical UUID helpers, requires complete input consumption, and rejects unsupported versions or trailing JSON. Complete input is at most32768 bytes. Host/query IDs are canonical lower-case nonzero UUIDs. Times must fit the existing positive signed-range interval, with expiry strictly after issue and duration at most30000 milliseconds. The typed query UUID is correlation, never a task/session or permission.

There is no grant, generation, parent, actor, caller, credential, arbitrary target or request text. The decoder returns only the finite `SettingsAccessStatusIntent` or Invalid; it exposes no serde diagnostic or copied private packet. Decoding a valid intent does not authenticate or authorize it.

## Genuine verification owner

`OwnedVerifiedSettingsAccess::from_verified` is visible only to the existing Settings ingress module and its descendants. It consumes the actual private `VerifiedSettingsEnvelope`, the parsed intent, the actual `SettingsReceiptAuthority` and the original `PrivateWitness`. Public DTOs or matching IDs cannot construct that envelope or replace its original context. No new crypto/verification path or public factory is introduced.

The factory derives the fixed request deadline from the envelope's original captured wall/monotonic clock. It accepts no caller-supplied deadline, current time or now-plus-remaining value. It validates the same actual leased Host Store/context, live connection, Host identity, existing active receive guard and original outer/inner interval. The outer duration is positive and at most30000ms, outer expiry does not exceed inner expiry, and both original wall and monotonic deadlines must remain current. An observation preceding the original clock floor is Unavailable; expiry is Expired; scope mismatch is OutsideScope.

The source owns the real verified envelope and a unique in-process invocation identity. Read-only accessors expose actual context matching, connection, verified complete peer keys, intent, invocation, canonical pinned registration, transport receipt metadata, original clock floor and fixed deadline. The owner exposes its actual source and receipt authority. Source/owner are neither Clone nor serialized; Debug is fixed redacted. Factory inputs are first moved into a data-first record with the original witness last, before invalid-intent or deadline errors can return. The full owner is then constructed before the fallible receive check. Both early-error records and the successful owner destroy actual authority/envelope/source data before the final original private witness. It retains the existing32-global/four-peer/1MiB charge, not a new queue or budget.

`check_precommit` is a receive-time verification check: it requires the original active receive guard. A future post-join publication gate must use its own actual same-Core/current Host guard and cannot reuse this inactive receive check. Receipt metadata contains the existing ciphertext replay digest, not a plaintext hash. These accessors do not read a Settings grant, mint a Caller, query SQLite, commit a marker or authorize private output.

## Remaining producers and correlation policy

Root's later correlation policy is512 cells total per real connection and64 per exact peer, with no same-epoch eviction. It must retain immutable query target/time and the original fixed clocks. Exact read replay may observe newer current status without deadline renewal; changed identity conflicts. This codec/owner slice does not implement those cells or replay decisions.

The actual authenticated crypto callback, same-transaction receiver/full-pin receipt and reply reservation, genuine receive-join event, shared one-BUILD-slot eligibility, current own-grant/summary/denial gate, response DATA, publication/send and cleanup remain missing consumers. Bind cannot obtain generation labels merely from this decoder. No placeholder response, fabricated BindingIntent/parent/grant, SQL seed, provider call or second runtime is supplied. The source's final private witness is retained; this leaf does not expose its mutation or fabricate true-join eligibility.

Accepted Delivery must stay refused until these genuine owners and consumers are integrated and reviewed. Standalone Rust1.88 formatting, diff and whole existing-file inverses verify source syntax/preservation only; compilation/strict CI, actual malformed-input behavior, crypto/transport/runtime, populated storage, Apple and security acceptance remain unverified. No tests or source comments are added. Issues #116/#117 and deferred #32 remain open.
