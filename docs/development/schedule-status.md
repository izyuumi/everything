# Native schedule status

The macOS menu bar app has a read-only Schedules tab backed by the existing trusted Everything CLI and authenticated owner IPC. It displays actual ScheduleEngine activation/stopped state, blocked-job diagnostics, pending result-delivery count and engine/delivery errors. A job opens its durable occurrence history with status, recorded summaries/reasons, attempt count and recorded usage. No generic configuration or response JSON is rendered in this flow.

The host remains authoritative. Opening, refreshing or closing the view does not activate the engine, claim an occurrence, retry inference, acknowledge an Unknown outcome, edit a schedule or stop the host. Explicit scheduling mutations remain in the [owner CLI](scheduling-cli.md). Native schedule results and encrypted conversation delivery are existing host responsibilities; this UI adds no notification service or execution loop. The iOS client consumes the separate encrypted scheduled-result event; this change does not modify it.

## Owner queries and bounds

The app sends only `OwnerRequest.query`, through the existing bounded OwnerIPCClient:

- `schedule_engine` reads the actual EngineStatus.
- `schedule` with `query.kind = list` requests ten jobs, including deleted jobs, with the returned exclusive `after_id` cursor.
- `schedule` with `query.kind = history` requests ten runs for the selected immutable job ID, with the returned exclusive `(claimed_at, occurrence_id)` cursor.

Job pages validate ascending UUID order. History pages validate descending timestamp/UUID order and match every run to the selected job. A continuation must match the final returned record. Duplicate rows, mismatched job identity, unsupported states, invalid timestamps, oversized pages, inconsistent terminal outcomes and malformed outcome digests fail visibly. The existing two-MiB owner transport and native bounded query replies remain in force.

The view retains at most 100 job projections and 100 run projections. Explicit Refresh starts at the first page; Load more/Load older advances the actual host cursor. After a successful additional page, automatic host-refresh hints preserve that manually browsed snapshot instead of resetting it every fifteen seconds. Engine status still refreshes. The view labels the saved snapshot and offers explicit Refresh for current jobs or newest runs. Connection or job selection resets browsing; a failed fetch preserves the prior snapshot and browsing flag. Full native definitions are not retained in these UI projections. Native diagnostic, name and outcome text limits are enforced before display. Unknown external outcomes stay Unknown even when the accounting's independent `known` flag is true. Usage counters do not certify absence or completion of external effects.

HostModel owns snapshots, cursors, loading flags and diagnostics separately from SwiftUI views. View tasks follow the existing connection and host-refresh hints. Async results recheck captured connection, connection generation, selected job and query generation before replacing state. Selection clears the previous job history synchronously and records its immutable job identity. Rendering, pagination and append guards require that same history identity, and history task identity includes the job UUID. Connection changes clear the schedule projections. Cancellation affects the owned read-only CLI request; no scheduler cancellation command is sent. App shutdown retains the existing owner-client cleanup and leaves the background host running.

## Validation and remaining limits

The official Swift 6.0.3 Linux frontend parsed all macOS and OwnerIPC sources in explicit Swift 6 mode. Actual Foundation-only JSONValue, OwnerWire and OwnerRecords passed strict Swift 6 typechecking. An optimized disposable executable compiled those production sources and decoded retained genuine native owner-query responses from the verified scheduling CLI walkthrough: first/reopened engine status, six jobs, three distinct failed runs across four history pages, an automatic failed occurrence, live Claimed/Running states and a reopened Unknown outcome with `usage.known = true`. The Unknown state remained unchanged. Changed in-memory copies of those actual records rejected wrong job identity, contradictory state/outcome, invalid digest, inconsistent history cursor and more than ten records. This was a manual DTO inspection, not a SwiftUI or owner-process runtime walkthrough.

Linux Foundation parsed the OpenStep project. Metadata inspection verified exactly one new macOS source file/build membership, preserved existing source order and left every iOS object, target setting and shared scheme unchanged.

No Apple SDK, Xcode, simulator or physical Apple device is available. SwiftUI/framework typechecking, build/analyze, native layout/accessibility, view-task lifecycle and live macOS owner-process interaction remain unverified. Required platform checks and normal repository protection still apply; source parsing and Foundation execution cannot replace them. Automated verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32). This read-only surface is one part of #54, whose full remote delivery and reconnect acceptance remains separate.

On macOS with Xcode:

```sh
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build analyze
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Release -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build
```

Then inspect disposable real jobs and run history, refresh during host drain and connection changes, confirm Unknown remains distinct from accounting certainty, and quit the app while verifying the same host PID remains alive. No real user schedules are needed for validation.
