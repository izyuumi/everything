# Native Settings assistant UI bridges

This contract covers owning-layer source bridges for the macOS Settings assistant. The pane, actual destination navigation and covering-presentation composition are separate work. Source inspection and syntax parsing do not establish Apple UI behaviour or successful provider execution.

## Original owner and cleanup

An `AssistantOpenAction` is created only by the original Store and binds its exact Owner. `isCurrentAssistantOwner` checks that sealed object against the same Store and current Owner incarnation; a matching connection UUID alone is insufficient. This is a presentation ownership check, not service authority or a foreground grant.

`assistantCleanup` returns only the original action’s observed Open state, parent-retired marker and actual parent drain report. A retired Owner can release its Store registry entry without losing those observations while its original sealed action is retained by the UI. Missing state/report remains unknown. A clean generic report is never converted into a fabricated per-child Closed. Actual drain Open states come from the same original `closeAssistantOriginal` replies; existing errors and unresolved IDs remain preserved. No question, answer, proof or invitation is included.

## Sealed device handoff

`presentAssistantDevice` accepts only a sealed `AssistantDetailsReceipt` already materialized through the SAME original child/turn and Store. It requires the exact current inspection, saved scope, unchanged native foreground owner, no competing unresolved mutation and an actual uncovered parent window. It never allocates another creation request. Repeated presentation of the same live context returns its original handle; a retired context is not reopened.

The Agent path is separate from the existing NativeUI-only preparation predicate. It captures the minimum original child/reference/parent wall and monotonic lifetime. Pair inserts its context only after reserving ONE additional approval identity under the same64 aggregate bound, eight Pair contexts and single live ceremony. The existing creation slot is already prepaid; no reservation is refunded by dismissal. Actual final approval captures the same sealed receipt bounds again without renewing its original deadline. Invitation retention and final delivery use the original bounded deadline. Existing full identity/access selection, held invitation callback, real acceptance verification and UI retirement remain unchanged.

Rust permits this same-parent handoff: `SettingsAuthority::session` returns the original native parent for a SettingsAgent; `operation_identity` records it; `check_principal`, `admit_native_action` and `confirm_device_action` require that exact authenticated native caller/session. Scoped `agent_action_details` validates original parent/chat/turn membership and origin. `validate_current` still checks the real Agent origin is live. These source predicates are not a runtime observation.

Covering the private assistant with its OWN authenticated device ceremony must only pause reads/private publication and retire the pending review. It must not automatically CloseOpen: closing that child cancels its unadmitted Awaiting operation. Actual collapse/hide/application/window/Owner retirement retains genuine close semantics. Root owns that composition; this bridge does not implement the pane or covering gate.
