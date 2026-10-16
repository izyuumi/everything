# ADR-0012: Scoped Settings grants retain immutable owner decisions

Status: Accepted direction; source foundation is awaiting independent review, genuine owner integration and required validation.

## Context

Existing pairing grants conversation access and does not authorize remote Settings. Optional remote Settings needs a separate explicit owner decision bound to exact pinned public keys, limited settings, a provider recipient and a finite lifetime. Revocation must remain possible when immutable history capacity is exhausted. The existing Store is the authoritative local persistence boundary; native Settings identities and cleanup must retain their current semantics.

## Decision

Use a closed crate-private grant domain and two new Store tables. Immutable versions retain the complete canonical public change once and one separately bounded immutable receipt. A head references the exact next version. One SQL statement validates expected head and projected capacity, inserts version/receipt and installs the head atomically. Public SHA-256 identifies the retained public request; it never conveys authority.

Require the future genuine local-owner ingress to derive Host/owner context and compare configured provider consent under the existing Host mutation guard. Storage checks actual Host role and exact full device pin. Existing devices receive no default grant. Only five Host settings are readable and two memory preferences can be writable; native protected actions, credentials and device trust remain outside this grant.

Bound immutable history by count and complete encoded bytes. Each Active head reserves one future2048-byte Revoked version and512-byte receipt before activation. Replacements transfer the reserve atomically; revocation consumes it and permits the exact already-revoked pin. Never evict original decisions or recycle retired grant IDs to regain capacity. The closed revoked record's derived worst-case716 bytes fits that explicit reserve.

## Consequences

The foundation can store and inspect original owner decisions without introducing remote ingress, a second service/runtime, credential resolution or trust side effects. It cannot alone authenticate a caller, cancel a session or activate remote authority. Genuine owner CLI/IPC integration precedes strict CI of the combined feature; remote admission, encrypted private payload routing and actual generation retirement require later independently reviewed seams.

Finite history can refuse new activations and renewals, while existing reserved revocation remains available. Encoded metadata limits do not bound SQLite/WAL growth or process RSS. Native schema1–14 records remain unchanged. Existing ciphertext accepted by a relay cannot be recalled by a later revocation. Compilation, schema execution, disposable behavior observations and later automated/security review remain distinct gates.
