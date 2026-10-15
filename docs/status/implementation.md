# Implementation status

Assessment date: 2026-10-15. The initial main branch contained only LICENSE and .gitignore. No pre-existing implementation or automated tests were present. All 31 open issues and comments were read before implementation. Issue #30 remains the authoritative roadmap.

## Current integration

Bootstrap #1, domain #2, storage #3, provider contracts #4, tool contracts #5, wiki #6, crypto/sync #7 and native Anthropic adapter #8 are merged through PRs #33–#40 after Linux/macOS CI. Native OpenAI #9 is under PR #41. Permissions #11, context #16 and generic embedding profiles #31 have passed local validation and independent review and await focused integration. The #10 prerequisite adds atomic private conversation checkpoints so messages and provider continuation cannot diverge on restart. Background service #18 and blind relay #13 are being implemented against merged contracts.

The CLI still exposes help only; native inference is not yet wired to it. No live model requests have been made. Disposable checks covered storage rollback/dedup/restart/revocation, wiki CRUD/reindex/confinement, Rust crypto/replay/sync, approval nonescalation/expiry/revocation, context compaction/grouping/budget/continuation, and embedding contract validation/fingerprints. These observations are not automated coverage or a security audit.

Rust 1.99.0 is installed. Apple SDKs and Mac hardware are unavailable locally; GitHub macOS CI supplies Rust build checks. Swift interoperability, live model behavior, and native Mac embedding benchmarks remain unverified. Normal GitHub merge enforcement and passing CI are required.

## Dependency graph and ownership

| Issue | Objective | Prerequisites | State |
| --- | --- | --- | --- |
| [#1](https://github.com/izyuumi/everything/issues/1) | Bootstrap Rust workspace and CI | None | Merged (#33) |
| [#2](https://github.com/izyuumi/everything/issues/2) | Define versioned domain and event contracts | None | Merged (#34) |
| [#3](https://github.com/izyuumi/everything/issues/3) | Implement SQLite storage and migrations | None | Read-page guard corrected; follow-up validation |
| [#4](https://github.com/izyuumi/everything/issues/4) | Implement model-neutral provider types | None | Merged (#35) |
| [#5](https://github.com/izyuumi/everything/issues/5) | Define Tool trait, registry, and argument validation | None | Merged (#36) |
| [#6](https://github.com/izyuumi/everything/issues/6) | Implement Markdown wiki parser and FTS5 index | None | Merged (#38) |
| [#7](https://github.com/izyuumi/everything/issues/7) | Define relay protocol and crypto test vectors | None | Merged (#39) |
| [#8](https://github.com/izyuumi/everything/issues/8) | Implement Anthropic native Messages adapter | #4 | Merged (#40); live API unverified |
| [#9](https://github.com/izyuumi/everything/issues/9) | Implement OpenAI native Responses adapter | #4 | Implemented, PR #41 validation |
| [#10](https://github.com/izyuumi/everything/issues/10) | Build persistent CLI conversation loop | #2, #3, #4, #8 | Atomic checkpoint and coherent snapshots implemented; integration pending |
| [#11](https://github.com/izyuumi/everything/issues/11) | Implement permission policy and persistent grants | #2, #3, #5 | Implemented, reviewed; integration pending |
| [#12](https://github.com/izyuumi/everything/issues/12) | Implement local EmbeddingGemma 2 inference spike | #31 | Blocked on #31 integration |
| [#13](https://github.com/izyuumi/everything/issues/13) | Implement E2EE relay transport server | #7 | In progress |
| [#14](https://github.com/izyuumi/everything/issues/14) | Implement Tool Broker execution journal | #3, #5, #11 | Backlog |
| [#15](https://github.com/izyuumi/everything/issues/15) | Implement shell and filesystem tools | #14 | Backlog |
| [#16](https://github.com/izyuumi/everything/issues/16) | Implement context builder and compaction | #2, #3, #4, #6 | Implemented, reviewed; integration pending |
| [#17](https://github.com/izyuumi/everything/issues/17) | Implement local tool discovery and optional Jev selector | #5, #11, #16 | Backlog |
| [#18](https://github.com/izyuumi/everything/issues/18) | Implement native macOS background host service | #1, #2, #3 | In progress |
| [#19](https://github.com/izyuumi/everything/issues/19) | Implement MCP client tool adapter | #5, #14 | Backlog |
| [#20](https://github.com/izyuumi/everything/issues/20) | Implement local tool discovery and optional Jev selector | #5, #11, #16 | Backlog |
| [#21](https://github.com/izyuumi/everything/issues/21) | Implement sqlite-vec hybrid retrieval and index switching | #6, #12, #31 | Backlog |
| [#22](https://github.com/izyuumi/everything/issues/22) | Implement device pairing and client sync | #2, #3, #7, #13 | Backlog |
| [#23](https://github.com/izyuumi/everything/issues/23) | Implement multi-tool agent loop | #8, #9, #10, #14 | Backlog |
| [#24](https://github.com/izyuumi/everything/issues/24) | Implement task scheduler and concurrent subagents | #3, #14, #23 | Backlog |
| [#25](https://github.com/izyuumi/everything/issues/25) | Implement steering and autonomous continuation | #23, #24 | Backlog |
| [#26](https://github.com/izyuumi/everything/issues/26) | Implement wiki consolidation and lint jobs | #6, #16 | Backlog |
| [#27](https://github.com/izyuumi/everything/issues/27) | Build optional SwiftUI menu bar client | #18, #25 | Backlog |
| [#28](https://github.com/izyuumi/everything/issues/28) | Build native iOS E2EE chat client | #13, #22, #25 | Backlog |
| [#29](https://github.com/izyuumi/everything/issues/29) | Implement optional scoped cloud worker delegation | #24, #25 | Backlog |
| [#30](https://github.com/izyuumi/everything/issues/30) | META: Dependency roadmap for parallel agent implementation | None | Backlog |
| [#31](https://github.com/izyuumi/everything/issues/31) | Define generic embedding contract and index fingerprints | #1, #6 | Implemented, reviewed; integration pending |

#17 and #20 specify the same selector scope and will share one focused implementation, with both issues explicitly reconciled. #31 owns the embedding contract. The roadmap's waves are scheduling guidance; explicit prerequisites take precedence (for example #19 requires #14).

The critical path is #1/#2/#3/#4/#5 → #8/#9/#10/#11 → #14 → #23 → #24 → #25. #6 and #7 can proceed concurrently. #31 follows #6; #12 then gates #21. #13 follows #7 and gates #22 and #28. Optional clients and workers wait for integrated runtime contracts.

The coordinator owns root manifests, library registration, CLI integration, shared contracts documentation, and this status file. Module owners work in isolated worktrees. Domain events own src/protocol.rs; cryptography owns src/crypto.rs and separate security documentation. No parallel agent edits those shared paths. Provider adapters, storage, wiki, and tool foundations have disjoint source ownership.

## Deferred validation and blockers

Automated test creation is deliberately deferred under the mission's explicit no-tests instruction. [Follow-up #32](https://github.com/izyuumi/everything/issues/32) tracks issue-specific missing assertions. No deferred acceptance criterion is considered verified. Apple builds, device roundtrips, native Mac benchmarks, live provider requests, and security review require their respective supported environments or credentials. None have been verified.
