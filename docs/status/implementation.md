# Implementation status

Assessment date: 2026-10-15. The initial main branch contained only LICENSE and .gitignore. No pre-existing implementation or automated tests were present. All 31 open issues and comments were read before implementation. Issue #30 remains the authoritative roadmap.

## Current integration

Foundation issues #1–#9, permissions #11, relay #13, context #16, and embedding contracts #31 are merged through PRs #33–#46 after Linux/macOS CI. Atomic private conversation checkpoints and coherent snapshots are integrated for #10. This branch adds immutable owner cleanup attestations for #14 so an unknown side effect cannot lose its recovery quarantine on restart. Background service #18 has a merged library (#48) awaiting CLI integration. Persistent chat #10, the Tool Broker #14, selector #17/#20, device synchronization #22, and wiki consolidation #26 are active isolated workstreams.

The CLI still exposes help only; native inference is not yet wired to it. No live remote model requests have been made. EmbeddingGemma 2 produced finite normalized vectors and relevant-document rankings locally at 128, 256, 512, and 768 dimensions; the backend is not yet merged. Disposable checks covered storage rollback/dedup/restart/revocation, wiki CRUD/reindex/confinement, Rust crypto/replay/sync, approval nonescalation/expiry/revocation, context compaction/grouping/budget/continuation, and embedding contract validation/fingerprints. These observations are not automated coverage or a security audit.

Rust 1.99.0 is installed. Apple SDKs and Mac hardware are unavailable locally; GitHub macOS CI supplies Rust build checks. Swift interoperability, live model behavior, and native Mac embedding measurements are recorded in the benchmark document; Swift interoperability remains unverified. Normal GitHub merge enforcement and passing CI are required.

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
| [#10](https://github.com/izyuumi/everything/issues/10) | Build persistent CLI conversation loop | #2, #3, #4, #8 | In progress; atomic storage prerequisite merged (#44) |
| [#11](https://github.com/izyuumi/everything/issues/11) | Implement permission policy and persistent grants | #2, #3, #5 | Merged (#43) |
| [#12](https://github.com/izyuumi/everything/issues/12) | Implement local EmbeddingGemma 2 inference spike | #31 | Actual native inference and release benchmarks verified on Linux/macOS; integration validation |
| [#13](https://github.com/izyuumi/everything/issues/13) | Implement E2EE relay transport server | #7 | Merged (#45) |
| [#14](https://github.com/izyuumi/everything/issues/14) | Implement Tool Broker execution journal | #3, #5, #11 | Implemented; native loop integration remains #23 |
| [#15](https://github.com/izyuumi/everything/issues/15) | Implement shell and filesystem tools | #14 | Merged (#62); scoped filesystem and explicitly trusted shell implementation; automated assertions deferred to #32 |
| [#16](https://github.com/izyuumi/everything/issues/16) | Implement context builder and compaction | #2, #3, #4, #6 | Merged (#46) |
| [#17](https://github.com/izyuumi/everything/issues/17) | Implement offline permission-aware discovery | #5, #11, #16 | Merged (#61); offline permission-aware discovery |
| [#18](https://github.com/izyuumi/everything/issues/18) | Implement native macOS background host service | #1, #2, #3 | Library merged (#48); CLI integration pending |
| [#19](https://github.com/izyuumi/everything/issues/19) | Implement MCP client tool adapter | #5, #14, #17 | Implemented in PR #63; native server walkthrough passed; current Linux/macOS and minimum-version validation is recorded on the PR; automated assertions remain deferred to #32 |
| [#20](https://github.com/izyuumi/everything/issues/20) | Implement optional configurable System One classification | #5, #11, #16, #17 | Separate implementation required by edited issue |
| [#21](https://github.com/izyuumi/everything/issues/21) | Implement sqlite-vec hybrid retrieval and index switching | #6, #12, #31 | Backlog |
| [#22](https://github.com/izyuumi/everything/issues/22) | Implement device pairing and client sync | #2, #3, #7, #13 | In progress |
| [#23](https://github.com/izyuumi/everything/issues/23) | Implement multi-tool agent loop | #8, #9, #10, #14 | Backlog |
| [#24](https://github.com/izyuumi/everything/issues/24) | Implement task scheduler and concurrent subagents | #3, #14, #23 | Backlog |
| [#25](https://github.com/izyuumi/everything/issues/25) | Implement steering and autonomous continuation | #23, #24 | Backlog |
| [#26](https://github.com/izyuumi/everything/issues/26) | Implement wiki consolidation and lint jobs | #6, #16 | In progress |
| [#27](https://github.com/izyuumi/everything/issues/27) | Build optional SwiftUI menu bar client | #18, #25 | Backlog |
| [#28](https://github.com/izyuumi/everything/issues/28) | Build native iOS E2EE chat client | #13, #22, #25 | Backlog |
| [#29](https://github.com/izyuumi/everything/issues/29) | Implement optional scoped cloud worker delegation | #24, #25 | Backlog |
| [#30](https://github.com/izyuumi/everything/issues/30) | META: Dependency roadmap for parallel agent implementation | None | Backlog |
| [#31](https://github.com/izyuumi/everything/issues/31) | Define generic embedding contract and index fingerprints | #1, #6 | Merged (#42) |

The latest #17 covers offline discovery only and gates #20, which requires a configurable generic System One client with a TypeSafe preset and approved custom endpoints. Earlier combined fixed-TypeSafe work has been split and will not close #20. #31 owns the embedding contract. The roadmap's waves are scheduling guidance; explicit prerequisites take precedence (for example #19 requires #14).

The critical path is #1/#2/#3/#4/#5 → #8/#9/#10/#11 → #14 → #23 → #24 → #25. #6 and #7 can proceed concurrently. #31 follows #6; #12 then gates #21. #13 follows #7 and gates #22 and #28. Optional clients and workers wait for integrated runtime contracts.

The coordinator owns root manifests, library registration, CLI integration, shared contracts documentation, and this status file. Module owners work in isolated worktrees. Domain events own src/protocol.rs; cryptography owns src/crypto.rs and separate security documentation. No parallel agent edits those shared paths. Provider adapters, storage, wiki, and tool foundations have disjoint source ownership.

## Deferred validation and blockers

Automated test creation is deliberately deferred under the mission's explicit no-tests instruction. [Follow-up #32](https://github.com/izyuumi/everything/issues/32) tracks issue-specific missing assertions. No deferred acceptance criterion is considered verified. Apple builds, device roundtrips, native Mac benchmarks, live provider requests, and security review require their respective supported environments or credentials. None have been verified.
