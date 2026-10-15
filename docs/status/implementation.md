# Implementation status

Assessment date: 2026-10-15. The initial main branch contained only LICENSE and .gitignore. No pre-existing implementation or automated tests were present. All 31 open issues and comments were read before implementation. Issue #30 remains the authoritative roadmap.

## Current integration

Foundation issues #1–#11, relay #13, Tool Broker #14, context #16, and embedding contracts #31 are merged through PRs #33–#57 after Linux/macOS CI. The CLI supports durable text conversations and explicit recovery of pending turns. Background service #18 has a validated library awaiting CLI integration. Reviewed local integration includes built-in tools #15, device synchronization #22, private model-attempt checkpoints and wiki maintenance #26; their focused PRs await GitHub Actions access. Local embedding #12 is merged in PR #49. Offline discovery #17 is merged. The reviewed local integration also includes MCP #19 and native tool loop #23; their focused PRs await required GitHub checks. Optional System One #20 and schedule persistence #50 are reviewed and integrated locally. Hybrid retrieval #21 has an isolated workstream.

No live remote model requests have been made. EmbeddingGemma 2 produced finite normalized vectors and relevant-document rankings locally at all four dimensions; its native backend is merged, and Linux/macOS measurements have been inspected. The combined native CLI preserved an uncertain local-only model attempt across restart, refused an implicit retry, and rejected saved-setup overrides. No live provider success was inferred from this failure-only observation. Disposable manual checks covered storage rollback/dedup/restart, wiki CRUD/consolidation/provenance/prepared-write recovery, crypto/replay/sync, permission restrictions, broker approval/replay/quarantine, context budgets and native local embedding cancellation. These observations are not automated coverage or a security audit.

Rust 1.99.0 and the supported Rust 1.88.0 compiler are installed. Apple SDKs and Mac hardware are unavailable locally; Earlier successful GitHub macOS CI supplied Rust builds and native embedding measurements; new jobs currently cannot start because of account billing. Swift interoperability and live model behavior remain unverified. Normal GitHub merge enforcement and passing CI are required.

## Dependency graph and ownership

| Issue | Objective | Prerequisites | State |
| --- | --- | --- | --- |
| [#1](https://github.com/izyuumi/everything/issues/1) | Bootstrap Rust workspace and CI | None | Merged (#33) |
| [#2](https://github.com/izyuumi/everything/issues/2) | Define versioned domain and event contracts | None | Merged (#34) |
| [#3](https://github.com/izyuumi/everything/issues/3) | Implement SQLite storage and migrations | None | Merged (#37, corrected in #44) |
| [#4](https://github.com/izyuumi/everything/issues/4) | Implement model-neutral provider types | None | Merged (#35) |
| [#5](https://github.com/izyuumi/everything/issues/5) | Define Tool trait, registry, and argument validation | None | Merged (#36) |
| [#6](https://github.com/izyuumi/everything/issues/6) | Implement Markdown wiki parser and FTS5 index | None | Merged (#38) |
| [#7](https://github.com/izyuumi/everything/issues/7) | Define relay protocol and crypto test vectors | None | Merged (#39) |
| [#8](https://github.com/izyuumi/everything/issues/8) | Implement Anthropic native Messages adapter | #4 | Merged (#40); live API unverified |
| [#9](https://github.com/izyuumi/everything/issues/9) | Implement OpenAI native Responses adapter | #4 | Merged (#41); live API unverified |
| [#10](https://github.com/izyuumi/everything/issues/10) | Build persistent CLI conversation loop | #2, #3, #4, #8 | Merged (#57); live provider success unverified |
| [#11](https://github.com/izyuumi/everything/issues/11) | Implement permission policy and persistent grants | #2, #3, #5 | Merged (#43) |
| [#12](https://github.com/izyuumi/everything/issues/12) | Implement local EmbeddingGemma 2 inference spike | #31 | Merged (#49); actual native inference and release benchmarks verified on Linux/macOS |
| [#13](https://github.com/izyuumi/everything/issues/13) | Implement E2EE relay transport server | #7 | Merged (#45) |
| [#14](https://github.com/izyuumi/everything/issues/14) | Implement Tool Broker execution journal | #3, #5, #11 | Implemented; native loop integration remains #23 |
| [#15](https://github.com/izyuumi/everything/issues/15) | Implement shell and filesystem tools | #14 | Reviewed local integration; manual broker walkthrough passed; Actions billing blocks merge |
| [#16](https://github.com/izyuumi/everything/issues/16) | Implement context builder and compaction | #2, #3, #4, #6 | Merged (#46) |
| [#17](https://github.com/izyuumi/everything/issues/17) | Implement offline permission-aware tool discovery | #5, #11, #16 | Merged (#61); offline discovery |
| [#18](https://github.com/izyuumi/everything/issues/18) | Implement native macOS background host service | #1, #2, #3 | Shared lease and owner inbox/settings reviewed locally; actual schema 6→7 replay/restart checks passed; background dispatch/CLI in progress |
| [#19](https://github.com/izyuumi/everything/issues/19) | Implement MCP client tool adapter | #5, #14, #17 | Reviewed local integration; native server walkthrough passed; Actions billing blocks merge |
| [#20](https://github.com/izyuumi/everything/issues/20) | Implement configurable System One classification | #5, #11, #16, #17 | Reviewed local integration; offline/native-unavailable/refused-loopback checks passed; live service and native persistence unverified; Actions billing blocks merge |
| [#21](https://github.com/izyuumi/everything/issues/21) | Implement sqlite-vec hybrid retrieval and index switching | #6, #12, #31, #26 | In progress against frozen reviewed local prerequisites |
| [#22](https://github.com/izyuumi/everything/issues/22) | Implement device pairing and client sync | #2, #3, #7, #13 | Reviewed local integration; real relay/restart/revocation passed; Actions billing blocks merge |
| [#23](https://github.com/izyuumi/everything/issues/23) | Implement multi-tool agent loop | #8, #9, #10, #14 | Reviewed local integration; actual Rust 1.88 checks, strict Clippy, all-feature release and CLI recovery observations passed; Actions billing blocks merge |
| [#24](https://github.com/izyuumi/everything/issues/24) | Implement task scheduler and concurrent subagents | #3, #14, #23 | In progress against the frozen local native kernel; bounded actor and task tools |
| [#25](https://github.com/izyuumi/everything/issues/25) | Implement steering and autonomous continuation | #23, #24 | Backlog |
| [#26](https://github.com/izyuumi/everything/issues/26) | Implement wiki consolidation and lint jobs | #6, #16 | Reviewed local integration; required GitHub checks blocked by account billing |
| [#27](https://github.com/izyuumi/everything/issues/27) | Build optional SwiftUI menu bar client | #18, #25 | Backlog |
| [#28](https://github.com/izyuumi/everything/issues/28) | Build native iOS E2EE chat client | #13, #22, #25 | Backlog |
| [#29](https://github.com/izyuumi/everything/issues/29) | Implement optional scoped cloud worker delegation | #24, #25 | Architecture preparation complete; implementation waits for stable scheduler/steering |
| [#30](https://github.com/izyuumi/everything/issues/30) | META: Dependency roadmap for parallel agent implementation | None | Backlog |
| [#31](https://github.com/izyuumi/everything/issues/31) | Define generic embedding contract and index fingerprints | #1, #6 | Merged (#42) |
| [#50](https://github.com/izyuumi/everything/issues/50) | Persist scheduled jobs and immutable run history | #2, #3, #10; migration order after #22/private checkpoints | Reviewed local integration; actual schema 3→6, claims/races/restart and full static/release validation passed; Actions billing blocks merge |
| [#51](https://github.com/izyuumi/everything/issues/51) | Calculate timezone-aware occurrences and dispatch due work | #24, #50 | DST/clock/admission preparation complete; implementation waits for stable #24 |
| [#52](https://github.com/izyuumi/everything/issues/52) | Enforce isolated scheduled execution and run policies | #24, #25, #50, #51 | Backlog |
| [#53](https://github.com/izyuumi/everything/issues/53) | Add schedule CLI and native tool controls | #5, #11, #50, #51, #52 | Backlog |
| [#54](https://github.com/izyuumi/everything/issues/54) | Deliver schedule status and encrypted client notifications | #22, #27, #28, #52, #53 | Backlog |
| [#55](https://github.com/izyuumi/everything/issues/55) | Document schedule behavior, contracts and decisions | #50–#54 | Incremental documentation; final reconciliation pending |
| [#68](https://github.com/izyuumi/everything/issues/68) | Compose memory, MCP and optional selection in the headless host | #15, #18–#21, #23–#26 | Backlog; actual component interfaces precede composition |

The roadmap gained scheduling issues #50–#55 during implementation; all six and updated #30 have been read. Issue #68 records the remaining application-composition gap, so library completion cannot be mistaken for a fully wired host. It follows the actual background host, native scheduler/steering, memory/hybrid retrieval, configured MCP and optional selector contracts. The latest #17 covers offline discovery only and gates #20, a configurable generic System One classifier with TypeSafe preset and approved custom endpoints. Their implementations and PRs are separate. #31 owns the embedding contract. The roadmap's waves are scheduling guidance; explicit prerequisites take precedence (for example #19 requires #14).

The critical path is #1/#2/#3/#4/#5 → #8/#9/#10/#11 → #14 → #23 → #24 → #25. #6 and #7 can proceed concurrently. #31 follows #6; #12 then gates #21. #13 follows #7 and gates #22 and #28. Optional clients and workers wait for integrated runtime contracts. Scheduling persistence proceeds alongside the runtime; #24/#50 gate dispatch #51, then #25/#51 gate scheduled execution #52, controls #53 and client delivery #54.

The coordinator owns root manifests, library registration, CLI integration, shared contracts documentation, and this status file. Module owners work in isolated worktrees. Domain events own src/protocol.rs; cryptography owns src/crypto.rs and separate security documentation. No parallel agent edits those shared paths. Current native-host ownership is #18 in service/transport/local/host/IPC and the CLI; #24 owns the scheduler, #25 follows its freeze, and #21 owns memory retrieval adapters. The coordinator integrates #68 after those contracts stabilize. Provider adapters, storage, wiki, and tool foundations have disjoint source ownership.

## Deferred validation and blockers

Automated test creation is deliberately deferred under the mission's explicit no-tests instruction. [Follow-up #32](https://github.com/izyuumi/everything/issues/32) tracks issue-specific missing assertions. No deferred acceptance criterion is considered verified. Swift builds/interoperability, live provider requests and security review still require their supported environments or credentials. Earlier integrated Rust macOS CI passed; current PR heads await restored Actions access; device/relay roundtrips and local embedding inference have disposable manual observations. The macOS native embedding benchmark completed and its published metrics were inspected. Each component document records the exact validation scope.

## External integration blocker

GitHub Actions refused new jobs before startup because account payments failed or the spending limit needs attention. The owner was notified with the exact check annotation. Main remains at `570c36f`; local integration is a separate reviewed branch and does not close implementation issues. Required repository checks and merge protection remain in force.
