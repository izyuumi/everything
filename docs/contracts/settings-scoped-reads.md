# Scoped Settings read DATA

The Rust `devices/settings_ingress/read.rs` and `devices/settings_read_reply.rs` leaves define closed version-one DATA for scoped Host values and fresh permanent Original inspection. They are an unregistered foundation at this source freeze. They implement no authenticated read Source, current parent/grant authority, receipt writer, shared query history, BUILD arm, projection, sealing, private send or client adoption. No remote endpoint is enabled. AccessSummary remains grant metadata rather than setting values.

## Requests and correlation

The existing outer grammar remains `schema_version:1,payload:{kind:"settings",version:1,host_id,message}`. A read message is `kind:"query",query_id,body`. Snapshot body is `kind:"snapshot",parent,keys,time`; inspection body is `kind:"inspect_original",parent,request_id,operation_id,time`. Parent has actual existing `parent_id,generation`; generation has `grant_id,grant_revision,host_epoch,connection_epoch`. Time has `issued_at,expires_at`.

The pure decoder accepts at most32768 UTF-8 bytes. Objects are closed, including every nested parent/generation/time record; canonical nonnil UUID syntax, actual integer ranges, duplicate fields and trailing input are checked through established serde decoding. The nullable inspection operation_id is required: omission, a nil/noncanonical UUID and a non-string/non-null value fail. Host/query/target identities and positive signed-range times are validated. The inner interval is at most30000ms. Snapshot keys must be a nonempty, unique, declaration-order-sorted subset of the exact five version-one Host keys; notification/OS keys are rejected. Inspection query_id differs from its target original request_id.

This is DATA validation only. The future real envelope boundary must independently enforce the49152 complete encrypted request family, existing private-frame/ordinary limits, full requesting peer/current Host identity, fixed original wall/Instant floors, live current Bind parent/generation and current R. Query UUID and encrypted request/response IDs remain different identities; the encoder can check the supplied request MessageId differs from query_id, while the actual future sealer must own and check its new response ID.

## Endpoint-free values

The response wrapper uses the existing `response,in_reply_to,correlation:{family:"query",query_id},generation,body` fields. Constructors take the typed original DATA request and derive Host/query/current-generation and inspection target/selection from it. A snapshot can satisfy only a snapshot request and must contain exactly its requested key sequence. Inspection can satisfy only inspection. Denial has generation null and only fixed grant_unavailable/outside_scope; a DATA constructor is not evidence that a denial is authorized.

Snapshot carries actual positive revision/observed_at, stale and1–5 sorted entries. Entry fields are key/default/desired/effective/state/unavailable_reason. The five exact key/value/default shapes are:

| Key | Value | Default |
|---|---|---|
| models.selection | model with null selection or provider+model | null selection |
| tools.selection | tool_selection with local or system_one+model | local |
| memory.embeddings.enabled | boolean | false |
| memory.embeddings.dimensions | dimensions with null or1–65536 | null/backend default |
| memory.embeddings.model | embedding_model with null or model | null |

Provider reuses the actual anthropic/openai finite type. Model text is at most256 actual UTF-8 bytes, nonempty after trimming and NUL-free. No endpoint, recipient, credential reference/presence, provider digest, proof, native receipt/action, framework DTO or arbitrary JSON field exists in these value types. They are display DATA, not a full recipient/binding identity or protected-update candidate.

Default is exactly the existing registry default for that key. Desired/effective values must have the correct key type. The read-only embedding model has no desired value. State/reason semantics reuse effective/pending_application/restart_required/unavailable and existing finite reasons; unavailable iff reason exists, and effective requires an effective value and compatible desired value. Endpoint redaction can make pending visible values equal; the encoder does not promote that state to effective. Typed null dimensions is distinct from an absent effective value.

The caller must observe current R before capturing even one private entry, preserve actual desired/effective/state and settings revision, and perform real current authorization before serialization/publication. Constructors validate that the supplied observed_at is inside the request's fixed inner interval; this does not validate the clock, current grant, source provenance or actual Host state. Receipt updated_at and observation wall times are not causal ordering tokens.

## Fresh historical inspection

Inspection body is `kind:"original_inspection",request_id,operation_id,observed_at,lookup`. Lookup is exactly `state:"not_recorded"` or `state:"recorded",original_generation,view`. Response generation comes from the new read's current parent; original_generation is independent immutable historical DATA. Reading an expired Original uses a new current read authority, not a renewed old Source, parent, slot or mutation.

The existing [OriginalView and tiny receipt](settings-original-replies.md) remain the only payload shape and validator. The old validator body is unchanged. One sibling-visible checked OriginalView encoder now shares its2048-byte receipt and4096-byte view checks with both the old command packet and this query wrapper. No native identity/confirmation/receipt/proof or old command input is projected.

Direct Update and ChatOpen requests use null operation selection. A linked Update's actual operation ID must equal the target Original ID. A Send without operation selection returns service not_requested; a selection must appear in its bounded sorted mutation-ID list and service must be not_recorded or the exact selected recorded operation. Existing kind/link/service/phase/write/application truth checks still run. These DATA relationships cannot establish actual ownership/fence membership: the genuine same-Core historical reader must prove own Host/full-peer provenance, current R over every original target and every disclosed changed key, actual operation origin and immutable link.

Missing Original, recorded Original with terminal Unknown, and recorded Original with service NotRecorded are distinct values. None proves NoEffects, permits retry, changes immutable mutation identity or equates an ACK/terminal Completed with Saved. ChatOpen/Send DATA does not create a restricted session; their real source/membership paths remain separate work.

## Limits, errors and integration

Checked actual serialization measures complete wrappers and escaping: snapshot≤32768 bytes, Original inspection including absence≤65536, denial≤2048. Existing ciphertext reservation families remain default49152/inspection90112/denial4096. This leaf performs no crypto/family-cap assertion or reservation; future producers must measure their actual encrypted envelope against its real reserved family without truncation or fallback. Every successful encoded byte vector is Zeroizing. Whole reply/snapshot/value/lookup aggregates have no Clone or Deserialize implementation; descriptions are fixed redacted.

SettingsReadContractError retains Invalid, TooLarge, actual serde_json Decoding/Encoding causes and the actual existing OriginalData error. Unexpected storage/authentication/rollback/commit/ownership failures cannot be mapped by this leaf because it performs none of those operations. Future source must preserve typed failure and noACK/quarantine rules rather than manufacture denial or NotRecorded.

Root must register both leaves and supply a narrow visible read module for the reply module's actual DATA imports. The [private ingress](private-settings-ingress.md), [command receive](settings-command-receive.md), [Access receipt](settings-access-receipts.md) and [transport storage](settings-transport-storage.md) contracts retain genuine Source/currentness/receipt/owned-join/build/send requirements. No query history, replay disposition, policy capability, cache or settings mutation is created by the decoder or constructor. Offline display, reconnect inspection, restricted sessions, change hints, local-owner-only cards and native foreground actions still require their real consumers. #116/#117 remain open; no compiler, codec/serializer, SQL/crypto/runtime, Apple SDK/UI or security verification is claimed. Automated coverage remains deferred to #32.
