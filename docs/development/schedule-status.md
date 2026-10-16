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

## Current-main integration

This focused extraction applies only the six reviewed UI paths from PR [#90](https://github.com/izyuumi/everything/pull/90) onto actual main `7942136e1edba3b516a079a9e653639ca81e253b`. Owner schedule controls are already integrated in [#105](https://github.com/izyuumi/everything/pull/105), and both Apple client cores in #104/#106. Existing Mac CLI cleanup, iOS scene restart/relay-acceptance corrections, target settings, shared schemes and the current Apple SDK workflow remain intact. The project adds only SchedulesView's file and build membership; all other project objects are unchanged except its two additive Mac containers. No Rust API, scheduling policy, dependency or storage schema changes.

## Validation and remaining limits

Fresh source preservation, Swift syntax parsing, strict Foundation-only JSONValue/OwnerWire/OwnerRecords typechecking, OpenStep/plist/scheme/reference checks, target membership, documentation links and diff checks are recorded for this current-main extraction. They do not execute SwiftUI or owner-process interactions. Both actual schemes require fresh exact-head SDK build and analysis before integration.

Earlier PR #90 observations remain historical: an optimized disposable Foundation executable decoded real native owner-query responses from the scheduling CLI walkthrough, including first/reopened engine status, six jobs, three distinct failed runs across four history pages, an automatic failed occurrence, live Claimed/Running states and reopened Unknown with usage.known=true. Supplied changed DTO copies rejected wrong job, state/outcome, digest, cursor and page bounds. That manually invoked inspector was not rerun here and did not establish SwiftUI/view lifecycle or live macOS owner-process behavior.

The local environment has no Apple SDK, Xcode, Simulator or physical device. Public CI can now build and analyze the actual Apple schemes; earlier checks on other source heads do not establish this candidate. Native layout/accessibility, view-task lifecycle, live owner interaction, connection changes, pagination during refresh and Quit/unchanged Host PID remain unverified on macOS. Source parsing, DTO execution and SDK compilation cannot replace those runtime observations. Automated assertions remain deferred to [#32](https://github.com/izyuumi/everything/issues/32). This read-only surface is one part of #54, whose full encrypted delivery/reconnect acceptance remains separate.

On macOS with Xcode:

```sh
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build analyze
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Release -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build
```

Then inspect disposable real jobs and run history, refresh during host drain and connection changes, confirm Unknown remains distinct from accounting certainty, and quit the app while verifying the same host PID remains alive. No real user schedules are needed for validation.
