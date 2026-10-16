# Scheduling CLI

The `schedule` command connects to the existing same-user background host through `HostClient`. Run `everything --state-dir STATE serve` in its managed service or another terminal first. CLI exit does not stop the host, Scheduler or ScheduleEngine. No command opens another Store or starts foreground agent execution.

## Manage a job

Create and update read a complete `ScheduleJobDraft` JSON file. Give each new job a non-nil UUID; deleted IDs cannot be reused. Updates replace the whole draft, including its original job ID, and require the exact observed revision. Enabling a job uses update with `enabled: true`; disabling has its own command. The host calculates its next occurrence. Deletion retains its history.

```sh
everything --state-dir STATE schedule create --file weekday-reminder.json
everything --state-dir STATE schedule list --limit 20
everything --state-dir STATE schedule show 3e154b91-094f-4c70-93a3-a70164a20f0e
everything --state-dir STATE schedule update --file weekday-reminder.json --expected-revision 1
everything --state-dir STATE schedule disable 3e154b91-094f-4c70-93a3-a70164a20f0e --expected-revision 2
```

Use the current response's revision before each new mutation. A concurrent edit or manual/automatic claim can change it. Create, update, disable and delete use ordinary uniqueness/CAS rules. Reusing their IPC request ID does not convert them into replayable commands. After an ambiguous error, inspect the stored job before deciding on another mutation; the CLI never refreshes revisions or retries automatically.

List accepts `--after-id UUID`, `--enabled true|false`, `--include-deleted`, and `--limit 1..100`. Its JSON contains the actual `next_after` cursor. Request the next page explicitly; pages are current views rather than a frozen snapshot. Individual records are never silently truncated. Delete requires `schedule delete JOB_ID --expected-revision REVISION` and does not remove run records.

## Manual execution and inspection

A disabled job can still be run explicitly. Manual execution durably claims an isolated occurrence on the same host engine; the CLI does not wait for successful inference. Its stderr prints the IPC request ID and complete manual claim before dispatch. Preserve that output if the response is interrupted.

```sh
everything --state-dir STATE schedule run 3e154b91-094f-4c70-93a3-a70164a20f0e --expected-revision 3 --occurrence-id 9109b0de-19d6-4ef4-9cf6-5320527cd2e5 --command-id 9856d983-a2dc-41f1-a55b-a275456670e6 --request-id 1787d312-eafb-4599-b97d-4a8ce6a5a770
everything --state-dir STATE schedule status
everything --state-dir STATE schedule status --occurrence-id 9109b0de-19d6-4ef4-9cf6-5320527cd2e5
everything --state-dir STATE schedule history 3e154b91-094f-4c70-93a3-a70164a20f0e --limit 20
```

These explicit IDs are examples for one new occurrence. If omitted, occurrence, command and request IDs are each generated once and printed before dispatch. An exact manual retry must retain the original job ID, ORIGINAL expected revision, occurrence ID and command ID, plus the same IPC request ID. Repeating that exact command returns the saved claim even if later job revisions advanced. Changing its original revision is a changed claim and conflicts. A different occurrence is new work, subject to overlap and uncertainty guards. Replaying a claim grants no provider retry and refunds no budgets.

Engine status shows activation, blocked jobs, pending deliveries and actual errors. Exact occurrence status includes its immutable run, attempt identities/results, cumulative usage and terminal outcome digest. The accounting's `known` flag describes recorded counters; it does not establish certainty about external effects. Missing credentials and native failures remain explicit run outcomes.

History JSON supplies `next_before` with `claimed_at` and `occurrence_id`. Continue with both `--before-claimed-at MILLISECONDS --before-occurrence-id UUID`; supplying just one is rejected. History pages contain at most 100 records. Reducing page size can resolve a reply bound; an individually oversized record fails explicitly.

## Acknowledge an Unknown outcome

This operation is available only through the authenticated same-user owner route. Inspect the exact occurrence and its outcome digest first. Acknowledgement records an operator decision to release that occurrence's overlap hold. It does not change its Unknown outcome, start another attempt, authorize provider retry, clear broker quarantine, or prove an external effect stopped. Model-facing scheduling tools and paired execution controls do not expose it.

```sh
everything --state-dir STATE schedule acknowledge-unknown 9109b0de-19d6-4ef4-9cf6-5320527cd2e5 --outcome-digest DIGEST --note 'Inspected the affected local resources and accepted this recorded uncertainty'
```

Replace `DIGEST` with the exact 64-character lowercase hexadecimal digest returned by occurrence status. The note is 1–1024 UTF-8 bytes without NUL and is printed, JSON-escaped, in the local stderr replay record. The CLI generates a command ID, IPC request ID and acceptance timestamp once, prints the complete acknowledgement before dispatch, then contacts the host. The host derives the real effective UID; no client UID is accepted.

For exact retry supply `--command-id UUID --request-id UUID --accepted-at-ms MILLISECONDS` from that record, together with the identical occurrence, digest and note. A changed timestamp, note or identity conflicts. A fresh timestamp after an ambiguous response is not an exact replay. Acknowledgement does not require successful model inference.

## Credential-free draft examples

Both examples below are disabled, isolated, read-only, zero-tool, one-attempt jobs. Create `/tmp/everything-schedule-workspace` as an existing physical directory, or replace the path with the intended authorized physical workspace. The host validates that workspace and requested authority. There are no credential values or credential references in a draft. Stored execution configuration does not follow later host defaults.

The first example uses the native official OpenAI endpoint. CRUD performs no model request. Explicit execution requires the host's matching configured credential and can disclose its captured instruction to that provider. The second uses a literal loopback endpoint for local failure inspection; it does not create a model server or simulate a successful model. Neither example asserts provider/model availability.

`weekday-reminder.json`:

```json
{
  "id": "3e154b91-094f-4c70-93a3-a70164a20f0e",
  "name": "weekday-reminder",
  "enabled": false,
  "definition": {
    "schema_version": 1,
    "schedule": {
      "kind": "cron",
      "expression": "0 9 * * 1-5",
      "time_zone": "UTC"
    },
    "target": {
      "configuration": {
        "provider": "openai",
        "model": "gpt-4.1-mini",
        "instructions": "You are Everything, a helpful assistant.",
        "endpoint": {
          "kind": "official"
        },
        "max_output_tokens": 256,
        "temperature": null
      },
      "instruction": "Write a short reminder to review the public project notes.",
      "workspace_root": "/tmp/everything-schedule-workspace",
      "permissions": {
        "mode": "read_only",
        "allowed_tools": []
      },
      "budget": {
        "max_model_requests": 1,
        "max_tool_calls": 0,
        "max_output_tokens": 256,
        "timeout_ms": 10000
      },
      "delivery": {
        "kind": "isolated"
      }
    },
    "policies": {
      "overlap": {
        "kind": "skip"
      },
      "missed": {
        "kind": "skip"
      },
      "retry": {
        "max_attempts": 1,
        "initial_backoff_ms": 5000,
        "max_backoff_ms": 300000
      }
    }
  }
}
```

`hourly-local-failure.json`:

```json
{
  "id": "60bc9c4d-59c0-4a2f-8e08-b2289229034b",
  "name": "hourly-local-failure-observation",
  "enabled": false,
  "definition": {
    "schema_version": 1,
    "schedule": {
      "kind": "interval",
      "anchor": 0,
      "every_ms": 3600000
    },
    "target": {
      "configuration": {
        "provider": "openai",
        "model": "gpt-4.1-mini",
        "instructions": "You are Everything, a helpful assistant.",
        "endpoint": {
          "kind": "loopback",
          "url": "http://127.0.0.1:9/"
        },
        "max_output_tokens": 256,
        "temperature": null
      },
      "instruction": "Write a short reminder to review the public project notes.",
      "workspace_root": "/tmp/everything-schedule-workspace",
      "permissions": {
        "mode": "read_only",
        "allowed_tools": []
      },
      "budget": {
        "max_model_requests": 1,
        "max_tool_calls": 0,
        "max_output_tokens": 256,
        "timeout_ms": 10000
      },
      "delivery": {
        "kind": "isolated"
      }
    },
    "policies": {
      "overlap": {
        "kind": "skip"
      },
      "missed": {
        "kind": "skip"
      },
      "retry": {
        "max_attempts": 1,
        "initial_backoff_ms": 5000,
        "max_backoff_ms": 300000
      }
    }
  }
}
```

## Bounds and validation

Draft input is at most 256 KiB and must be a regular file, opened without following a final symlink. Nonblocking open and descriptor metadata reject FIFO/device/directory input without waiting for a writer. A bounded read also catches growth past the limit. File reading and typed JSON validation run off Tokio workers; strict draft and configuration fields reject unknown additions. Credentials are configured through the separate owner credential route. The existing definition validator checks cron syntax, IANA timezones, absolute workspace syntax, native model configuration, budgets and policies. Host Controls and Authorizer remain authoritative for permissions, actual tool availability and mutation admission.

Canonical definitions are bounded to 128 KiB. Names are 1–256 bytes without controls; instructions 1–32768 bytes without NUL. Intervals range from 1 second through 366 days. Cron accepts five/six printable ASCII fields, no year, with explicit IANA timezone. Model/tool/output/time ceilings are cumulative across captured attempts; per-response output cannot exceed the occurrence ceiling. Default policies skip overlap and missed runs. Acknowledgement and manually claiming an occurrence do not grant execution permission or model credentials.

Cancellation before dispatch returns without contacting the host. After dispatch, disconnect or cancellation can accompany a committed mutation; inspect the saved job/occurrence or reuse the exact replay identity where supported. Owner responses remain within the existing Controls and IPC byte bounds. This leaf supplies CLI parsing and proxy calls; host registration, runtime ownership and native tool authorization are documented in [scheduling controls](../contracts/scheduling-controls.md) and [scheduled execution](../contracts/scheduled-execution.md).

The integrated host containing this exact command module passed Rust 1.88 formatting, default/all-feature workspace all-target checks, strict Clippy in both configurations and an optimized all-feature build. The reviewed aggregate copied Linux release completed 119 actual CLI calls across all ten commands. A fresh copied release of the focused extraction completed the same walkthrough with 115 calls; status polling makes that count vary. Disposable observations covered bounded job/history cursors, CRUD and stale CAS, invalid parser/file/cron/timezone/budget inputs, disabled manual claims, exact original-revision replay after a workspace edit, soft deletion, real automatic one-shot admission and an append-only scheduled-result event without a model-history message. Both complete draft examples were also accepted with only the documented scratch-workspace substitution and produced zero runs.

An owned non-listening literal-loopback endpoint with a public disposable session token produced an actual native connection refusal, terminal Unknown, one request and 128 reserved output tokens. Wrong-digest and changed-timestamp acknowledgements failed; exact acknowledgement replay survived graceful host drain and reopening. Removing that token permitted a fresh, credential-free factory failure after the acknowledged overlap hold was released. No native retry or budget refund was inferred. The aggregate walkthrough’s three disposable databases passed integrity and foreign-key checks after reopening. The focused extraction’s retained schema-9 database independently passed those checks, and the public session token was absent from its persisted host state. No records were seeded.

These observations performed no successful inference and do not establish different-UID rejection, cancellation after transaction admission, simultaneous edit races, every quota or crash boundary, Apple behavior or production security. Automated failure, race, crash and platform verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

The historical walkthroughs above predate the current-main extraction. Current prerequisite preservation and pending validation are recorded in the [controls contract](../contracts/scheduling-controls.md#validation). No successful inference or fresh runtime result is inferred from source parity.
