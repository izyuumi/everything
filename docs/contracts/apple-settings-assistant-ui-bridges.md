# Native Settings assistant UI bridges

This contract covers owning-layer source bridges for the macOS Settings assistant. The pane, actual destination navigation and covering-presentation composition are separate work. Source inspection and syntax parsing do not establish Apple UI behaviour or successful provider execution.

## Original owner and cleanup

An `AssistantOpenAction` is created only by the original Store and binds its exact Owner. `isCurrentAssistantOwner` checks that sealed object against the same Store and current Owner incarnation; a matching connection UUID alone is insufficient. This is a presentation ownership check, not service authority or a foreground grant.

`assistantCleanup` returns only the original action’s observed Open state, parent-retired marker and actual parent drain report. A retired Owner can release its Store registry entry without losing those observations while its original sealed action is retained by the UI. Missing state/report remains unknown. A clean generic report is never converted into a fabricated per-child Closed. Actual drain Open states come from the same original `closeAssistantOriginal` replies; existing errors and unresolved IDs remain preserved. No question, answer, proof or invitation is included.
