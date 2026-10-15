# Apple remote-result event decoding

The shared Foundation wire decoder accepts the native `ConversationEvent::RemoteTaskFinished` shape: the event payload contains `kind: "remote_task_finished"` and one `report` object. Rust's boxed report adds no extra JSON wrapper. Existing event and encrypted synchronization handling remain unchanged.

`WireRemoteTaskReport` validates the actual `src/protocol/remote.rs` data contract: non-nil domain UUIDs; bounded Unix millisecond timestamps; lowercase SHA-256 digests; the complete 71-byte, eight-group executor fingerprint; known outcome/source variants; an 8192-byte summary without NUL; at most64 artifacts; and the 64-KiB serialized report bound. Report, source and usage objects reject unknown fields. Optional Rust fields accept null or omission. Model/tool counters fit UInt32; token counters retain the full UInt64 range.

A result source requires its exact result digest and finish timestamp, excludes Rejected outcome, and binds any quarantine digest to that result digest. Nonzero artifacts require quarantine metadata. Unknown and Rejected have no artifacts or quarantine digest. A rejection source requires a nonempty control-free reason within256 UTF-8 bytes, Rejected outcome, no finish timestamp and exactly default usage, including absent reserved-output accounting. Supplied usage is informational and cannot refund native reservations.

This decoder validates data shape and bounds. It authenticates no worker source, admits no native result, creates no receipt, acknowledges no result, grants no permission and starts no execution. The existing encrypted transport and authoritative Host/Agent/Scheduler remain responsible for those boundaries. The report is untrusted external output, not an assistant response or user instruction.

Validation is pending the focused official Swift6 Linux grammar/Foundation walkthrough. Apple SDK builds, application UI, encrypted device roundtrips and successful remote inference remain unverified. No new automated tests or fixtures are introduced; deferred assertions remain in [#32](https://github.com/izyuumi/everything/issues/32).
