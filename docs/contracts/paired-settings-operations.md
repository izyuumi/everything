# Paired Settings operation metadata

This source increment adds closed historical operation metadata for [#116](https://github.com/izyuumi/everything/issues/116), on the exact Originals foundation `bf94d1df7bdd25d74af8be3a17fac380bbb25eb9`. It does not implement an authenticated paired caller, remote parent, accepted private transport, paired Agent entry or configuration effect admission. The [existing Settings service](settings-service.md), [restricted runtime](restricted-settings.md) and [Originals contract](settings-originals.md) remain separate authority and ownership boundaries. Compilation, engine behavior and disposable verification of the combined increment remain pending.

## Closed identity versions

OperationIdentity version1 keeps its original binding, target, native origin encodings and validation branch. The new version2 accepts only these additional SettingsOperationOrigin metadata tags, with fields in this order:

| Tag | Fields |
| --- | --- |
| paired_settings | actor_device_id, remote_parent_id, grant_id, grant_revision, host_epoch, connection_epoch |
| paired_settings_agent | actor_device_id, remote_parent_id, grant_id, grant_revision, host_epoch, connection_epoch, session_id |

IDs are their existing typed DeviceId/UUID values; grant_revision is positive and within signed64 range. Parent, grant, epochs and an Agent child are nonnil. The actual Host remains the Host-scope owner. The paired binding principal is the actor device UUID, which must differ from that Host, and its client_session_id is the remote parent. These are recorded facts, not a serialized permission or a live parent. Version1 rejects paired origins; version2 rejects native origins; unknown versions/fields fail closed.

Version2 targets only a confirmation-free Host SettingsPatch containing one or two sorted unique `memory.embeddings.enabled`/`memory.embeddings.dimensions` changes. Binding request/scope/revision match the actual patch. The existing request digest is recomputed over that exact public typed target; no chat text, raw tool arguments or private body hash is introduced. Existing patch/value, positive revision, original interval and whole identity bounds apply. There is no credential, device, native action, owner replacement or provider-selection target.

Historical decoding validates the recorded interval and metadata consistency without consulting the current grant, wall clock, connection, credentials or provider. An expired/rotated historical record remains decodable. Its actual live source, original full pin, owner consent, current grant and original monotonic bounds must be proven separately before any future effect path is implemented. A successfully decoded origin, grant UUID or stored reservation never creates SettingsCaller or authority.

## History and inspection

The existing bounded repository read, change-history and inspection validators understand the paired identity branch. Paired phases are Applying or Finished only. Completed native-action receipts and pending native confirmation/action/InFlight phases are rejected. Applied, PendingApplication and RestartRequired require the corresponding actual file-write metadata; existing revision/resolution/application consistency checks remain. Conflict, HostActionRequired, Unavailable, Failed, Unknown and Cancelled retain their existing typed shape and original correlation. These values describe journal metadata; model text or DTO validation cannot supply a trusted receipt.

Paired write metadata has no native request ID, selection change, pairing material or native observation. Its sorted changed keys remain within the two memory preferences, and the existing write intent must exactly match its public patch and file fingerprints. No native-request row may attach to a paired operation. Identity18KiB, update2048bytes and inspection32KiB limits remain unchanged. Stored canonical JSON is decoded and re-encoded through the existing exact-byte checks, without rewriting historical rows.

## Admission stays closed

Existing repository prepare and begin_write entries explicitly accept version1 only. The current Authority.check_origin and native service principal gate reject both paired metadata variants. Native proofs, CallerSource, native Agent creation, protected confirmations, action Details, pairing/credential entry and owner recovery are unchanged. Exact native-origin comparisons at those consumers continue to reject paired metadata; no remote UUID is placed in native_session_id.

The seventeen-field paired reservation, native eleven-field reservation, version1 fence/hash and six-field owner cleanup remain unchanged from the Originals foundation. No schema, live capability factory, paired writer, new service/runtime/Broker, crypto primitive or Apple route is added here. A later cohesive source increment needs the genuine verified ingress owner, same-Authority parent/child, same-kernel producer and actual same-transaction current-grant/pin checks. This metadata increment remains held unpublished until its actual consumers and required integration checks are ready. Deferred automated validation remains [#32](https://github.com/izyuumi/everything/issues/32); no new tests or fixtures are introduced.
