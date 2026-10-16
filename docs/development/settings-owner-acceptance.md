# Owner Settings and local selection acceptance

These 2026-10-16 observations cover genuine default Linux CLI operations on fresh disposable RAM state. They support part of [#118](https://github.com/izyuumi/everything/issues/118), [#114](https://github.com/izyuumi/everything/issues/114) and [#20](https://github.com/izyuumi/everything/issues/20); all three remain open. They complement the separately attributed [earlier Settings walkthrough](settings-validation.md). Supported Apple SDK build/analyze results do not establish native UI behavior.

## Executable and source identity

Both observations used the same authenticated default CLI from [Extended run 38953156479](https://github.com/izyuumi/everything/actions/runs/38953156479), artifact `12068959905`:

| Identity | Exact value |
| --- | --- |
| Source commit | `8061a7a140c1b5c43eb1e5bac46a719cb15b1572` |
| CLI SHA-256 | `9832fcc31d5d03772e9ff8766701e0c9b365c78a562d9bd96ef7d32dd2175a94` |
| Root artifact-gate SHA-256 | `fab52a25f762a0e3d3661d41ec0ebbb93bae5c27d780fe7a4b2f15b3118a304b` |

For local selection, the entire tracked source tree equals main `acffd5396f9bef7279561e70c61458d47aca3f5d`: tree `de9150794459f6f9d203ce6e6cec4305e7dbb5a5`. The commit identities remain distinct.

For the later CAS observation, comparison main is `6eab60fb7a2fdc885518bcfe3cfeec9ef70dae56`, tree `d26869326072e3d89a5235d2b2d7261ff1b3c4c7`. All Rust, Cargo and 15 embedded migration inputs match the executable's source. Only three Apple and three Markdown paths differ; the complete tracked trees differ. Neither observation executes unpublished remote Settings source.

## Shared owner CAS and original replay

The real owner CLI reached the existing authenticated SettingsService and authoritative revisioned file. The observed sequence was:

1. Start an empty Host with explicit private state/cwd/TMPDIR and only LANG plus TMPDIR in the child environment. Omit HOME, PATH, provider, DBus and XDG variables. Query owner runtime readiness and the actual Settings snapshot; preserve the initial socket-absent response.
2. Launch two owner replacements from that same revision-1 snapshot, changing only `context_tokens` to 96000 or 64000 under different original request IDs. Both client processes were recorded live after launch. The 96000 request returned Applied at revision 2; the other returned the actual identity/revision conflict.
3. Retry the unchanged winning request file with its original ID. Its receipt is byte-identical. Reuse that ID with a changed body and observe rejection. Public operation inspection retains the original result. Only revision and context_tokens change; optional composition stays equal.
4. Gracefully stop and join the original Host, reopen the same state exactly once, and retry the same original request. The durable receipt remains byte-identical, its inspection unchanged and saved revision still 2. This is a clean-restart observation, not a lost-response or crash experiment.

All 18 CLI children are recorded joined: 14 exited successfully; four rejected as observed, including each start's initial socket race. Both Hosts are recorded joined with exit 0 about 8 ms after original-pidfd SIGINT, without emergency termination. Independent review matched all 56 evidence files, 47 protected inputs, actual raw requests/outputs and the immutable CAS/replay source paths; all 20 recorded Host/client numeric PIDs were absent afterward.

The CAS driver ran inline and was not retained as a file. The independent filesystem review therefore cannot audit its complete spawning/pidfd/join implementation. Those facts remain producer-recorded, supported by raw outputs and later PID absence. Observers did not open or hash generated database/private-key contents.

## Local permission-aware selection

A separate fresh Host created one idle conversation through actual CLI initialization; this was configuration metadata only. The same complete proposed root command was never submitted. Its first local preflight genuinely failed with tools disabled. A real owner CAS then enabled tools while retaining ReadOnly, Local selection and absent external features/credentials.

Actual `prepare_selection` returned method Local with `read_file` and `agent_status`, without write, shell or MCP names, omissions or diagnostics. Composition reported one ticket, zero active requests and 1,616,856 reserved bytes. Discard returned Applied; subsequent composition reported zero tickets, active requests and reserved bytes. All 12 clients joined; the original Host stayed alive after clients exited and joined exit 0 after original-pidfd SIGINT. No reopen was performed in this selection observation.

The retained producer and independent audit cover the original process ownership and 53 evidence files/21 protected inputs. SelectionSummary omits the fallback definition: this does not observe `search_tools` delivery/execution, Broker tool execution, task submission or positive Jev/TypeSafe/custom classification. No provider or classifier was invoked.

## Evidence and remaining acceptance

Operator-retained evidence is identified below without embedding disposable paths, request/device IDs or private state in the repository. Hashes identify reviewed records; they do not promise a public evidence download.

| Record | SHA-256 |
| --- | --- |
| CAS actual receipt | `22b2e1141eee2ea6f9d61b148e1faf12eb2543107dc85a23ac22e9c029e01f10` |
| CAS independent retained-evidence/source review | `c68b9dcd1853f8a3a7a2885645e19e8300406e8cb6dd78617afc7c2b81b4951f` |
| Local selection actual receipt | `0f2af99243f132bc3c4951eb91b0a7012d8e31324bca98fcb2bf489329654a3a` |
| Local selection handoff | `85e785d8988873875779d936a0ae0c7b9b463e20f26e4810823f57c6a7335993` |
| Local selection independent review | `65faa4448c565ae1546a922a235c8396a6035f29ddd8feace6fdc2279dd9fb1d` |
| Retained local selection producer | `115b4bdbff673efdfd65be2e3f83ebeb243c85a8ec993374ccdbeb88fbaadaa8` |

Public review anchors are the [#118 CAS observation](https://github.com/izyuumi/everything/issues/118#issuecomment-6223600938), [#20 local observation](https://github.com/izyuumi/everything/issues/20#issuecomment-6223330309), [#118 preceding integration checkpoint](https://github.com/izyuumi/everything/issues/118#issuecomment-6223243365) and [#167 shutdown correction](https://github.com/izyuumi/everything/issues/167#issuecomment-6223105687). The earlier `87c6a64655cf5ae653e7591f153a1d4740f3c3cb` observation timed out after SIGINT and required an unclean owned-process kill; no reopen occurred. PR #168 separately corrected the owner-stop wake, and its exact-source executable observed successful joins and one same-state reopen. That correction does not relabel the earlier failure or prove active-work shutdown races.

These records establish focused owner-service CAS/replay and local selection behavior. Native forms, stale-editor UX, assistant/control cross-updates, protected confirmations, secure credential entry, OS dialogs, positive classification/native-key persistence, remote Settings and wider platform acceptance remain separate. There was no packet trace, database integrity audit, exhaustive race analysis or security certification. Deferred automated replay, cancellation, scope and UI coverage remains [#32](https://github.com/izyuumi/everything/issues/32).
