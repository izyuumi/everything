# ADR-0012: Scoped Settings grants retain immutable owner decisions

Status: Accepted; grant storage and genuine local-owner consumers merged in #156 after source review and current-head Ubuntu/Apple checks. The transaction-local reader merged in #164 after Ubuntu/compatibility success with a legitimate SDK skip. A separate system-SQLite empty-schema preparation does not verify Rust Store mutations, replay/capacity or remote authority; those behavior gates remain.

## Context

Existing pairing grants conversation access and does not authorize remote Settings. Optional remote Settings needs a separate explicit owner decision bound to exact pinned public keys, limited settings, a provider recipient and a finite lifetime. Revocation must remain possible when immutable history capacity is exhausted. The existing Store is the authoritative local persistence boundary; native Settings identities and cleanup must retain their current semantics.

## Decision

Use a closed crate-private grant domain and two new Store tables. Immutable versions retain the complete canonical public change once and one separately bounded immutable receipt. A head references the exact next version. One SQL statement validates expected head and projected capacity, inserts version/receipt and installs the head atomically. Public SHA-256 identifies the retained public request; it never conveys authority.

Use dedicated existing local Query/OwnerOperation ingress to derive Host/owner from authenticated LocalCall and compare configured provider consent and its original settings revision under the existing owned Host mutation guard. Retain that guard and the real Host/Agent/lease through blocking work. Exact replay precedes fresh wall/configuration checks. Storage checks actual Host role and exact full device pin. Existing devices receive no default grant. Only five Host settings are readable and two memory preferences can be writable; native protected actions, credentials and device trust remain outside this grant.

Bound immutable history by count and complete encoded bytes. Each Active head reserves one future2048-byte Revoked version and512-byte receipt before activation. Replacements transfer the reserve atomically; revocation consumes it and permits the exact already-revoked pin. Never evict original decisions or recycle retired grant IDs to regain capacity. The closed revoked record's derived worst-case716 bytes fits that explicit reserve.

## Consequences

Genuine owner CLI/IPC can store and inspect original decisions without introducing remote ingress, a second service/runtime, credential resolution or device-trust side effects. Direct owner routes bypass the generic durable HostInbox quota and preserve original request identity. Their complete 32768-byte wire bounds do not promise parser/RSS or physical disk limits. Caller-derived trusted context remains internal; only necessary public data DTOs cross the existing local IPC. Remote admission, encrypted private payload routing and actual generation retirement/cancellation require later independently reviewed seams before any Active record can grant remote authority.

Finite history can refuse new activations and renewals, while existing reserved revocation remains available. Encoded metadata limits do not bound SQLite/WAL growth or process RSS. Native schema1–14 records remain unchanged. Existing ciphertext accepted by a relay cannot be recalled by a later revocation. Compilation, schema execution, disposable behavior observations and later automated/security review remain distinct gates.
