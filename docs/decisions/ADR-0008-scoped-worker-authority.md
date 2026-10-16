# ADR-0008: Scoped worker authority and native result admission

## Status

Accepted for the implemented initial capability profile. Combined Rust gates and qualified Host/bridge/executor credential-rejection/recovery observations passed. Successful inference and a security review remain unverified; see [worker validation](../development/worker.md#combined-host-validation-checkpoint).

## Context

Optional execution on another machine crosses a data and trust boundary. A queued encrypted packet does not establish permission, native capacity, delivery, execution or completion. Reconstructing consent from serialized task data would silently extend its lifetime. Caller disconnects and process restarts must preserve reservations and original identities without repeating uncertain work.

## Decision

The local Host remains authoritative and reuses its one Agent, Scheduler, Broker, Store and lease. Explicit owner preparation discloses the complete bounded manifest, purpose-specific executor keys, model destination, selected data and immutable caps. Short-lived one-shot consent becomes an opaque staged capability retaining original wall and monotonic expiry and actual endpoint/Host guards through native admission. A compact existing Host inbox record preserves separate original local/executor IDs before Agent side effects. Recovery recognizes actual native reservations; it never recreates expired consent.

Worker-purpose identity/pins and credentials are separate from ordinary paired devices and local model credentials. The initial executor exposes scoped filesystem tools and native model APIs. It supplies no arbitrary shell/code backend or hard OS-resource guarantee. The worker operator can read the scoped plaintext; the relay receives authenticated ciphertext. No cloud provisioning, mandatory gateway or second local scheduling service is added.

Only an authenticated original terminal packet and actual private artifact quarantine can mint a native result capability. The Agent appends its attributed event and updates the original task/conversation atomically. The result remains external data, with no invented assistant response or local model usage. A subsequent application acknowledgement requires the real opaque native receipt. Cancellation/revocation persist their original durable intent before recovery mirrors native intent. Unknown outcomes retain the reservation and charge; transport or application acknowledgements do not prove stop.

## Consequences

The first management surface delegates fresh root tasks explicitly from the owner; it does not expose model-origin or nested cloud delegation. Unadmitted partial initialization, expired Offers, unavailable credentials and legacy terminal records lacking original-source provenance are visible fail-closed boundaries. Disabling the optional service neither rejects an already admitted command nor revokes its historical acceptance. Successful inference, native credential writes, Apple interoperability, arbitrary-code isolation and exhaustive crash/security verification require evidence beyond the current failure-path walkthroughs. Automated verification remains deferred to issue #32.
