# Local EmbeddingGemma 2 benchmark

Issue [#12](https://github.com/izyuumi/everything/issues/12) includes the standalone `vorra-embedding-bench` binary. It performs real local inference through the [EmbeddingGemma 2 ONNX backend](../architecture/embeddinggemma2.md), sharing one loaded model across the 128-, 256-, 512-, and 768-dimensional profiles. It does not download models, start Python, call a model service, create a vector index, or read user documents.

The benchmark intentionally uses the direct native development API. It does not launch the production embedding helper or establish confinement. Its existing macOS workflow remains a genuine performance measurement; ordinary macOS Host embeddings are unavailable and retain FTS until a platform boundary is implemented and verified. See [the production boundary](../architecture/embeddinggemma2.md#production-embedding-boundary) for the separate Linux Host behavior.

## Build and run

Prepare the pinned model bundle and a trusted compatible ONNX Runtime shared library according to the backend documentation. Selecting a native library explicitly authorizes loading its code; do not use an untrusted library. Model files remain outside the repository. The `local-embeddings` Cargo feature is required; default builds skip the backend and benchmark binary.

Launch the benchmark with `ORT_DISABLE_TELEMETRY=1` set before process start. The backend rejects missing or other values before runtime access and retains API telemetry suppression. The earlier numerical/performance observations below did not establish network silence; [#72](https://github.com/izyuumi/everything/issues/72) tracks runtime telemetry and verification of the specific supported build. This environment control is not an OS sandbox for an arbitrary native library.

```sh
cargo build --release --features local-embeddings --bin vorra-embedding-bench
ORT_DISABLE_TELEMETRY=1 ./target/release/vorra-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.dylib \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --threads 2 \
  --repetitions 3 \
  --warmup-iterations 1 \
  --deadline-seconds 300 > embedding-report.json
```

Use the corresponding `.so` library on Linux. Run `vorra-embedding-bench --help` for bounds and options. Threads are limited to 1–16, tokens to 8–8192 including backend special tokens, repetitions to 1–10, and warmup iterations to 0–3. The fixed corpus has eight short documents and four queries; each profile performs at most 120 measured inferences and six warmup inferences. All four profiles are always measured. There are no user-supplied text inputs or unbounded batches.

Defaults are two inference threads, 2048 tokens, three repetitions, one query/document warmup pair per profile, a 30-second per-inference deadline, a 120-second load deadline, and a 180-second overall deadline. The overall limit can be selected from 1–900 seconds; backend load and inference limits can each be selected from 1–300 seconds. Inputs over the configured token limit fail rather than being silently truncated.

Ctrl-C or the overall deadline cancels the backend token, waits for worker cleanup, and returns a nonzero status. The benchmark does CPU work on a blocking worker. Native session initialization can finish after a cancellation request because the backend cannot interrupt that native loading call; the overall deadline is cooperative and does not sandbox or forcibly kill native code. There is no detached inference worker. Successful stdout contains one complete JSON report; failure diagnostics are written to stderr and never masquerade as a successful partial report.

## Report semantics

The versioned JSON report records the public input corpus, runtime-library digest and information, the declared upstream source reference and pinned ONNX export revision, artifact and dimension-specific profile fingerprints, configuration, operating system and architecture, initialization and model-load times, and elapsed benchmark time. The operating-system file cache is uncontrolled. A new process creates a fresh model session but does not establish a cold disk-cache measurement.

Each dimension records every measured query/document latency, finiteness observation, vector dimension, and f64-accumulated L2 norm. Profile compatibility, shape, finite values, and declared normalization are validated through the existing embedding contract. Query and document latency summaries include minimum, mean, median, nearest-rank p95, and maximum. Warmup inference is timed separately and excluded from measured samples. With the defaults, each dimension has 12 query and 24 document samples; these are small observational samples rather than a statistically powered performance study.

The model always computes its native 768-component embedding before dimension selection and normalization. Smaller dimensions reduce stored vector size, rather than guaranteeing proportionally faster transformer inference. Dimension order is fixed; compare elapsed values with that order and shared-session warming in mind.

Every query ranks all eight documents by f64 cosine similarity, with deterministic document-ID ordering for equal scores. Two related documents are declared relevant to each query. The report includes all scores and per-query precision@1, precision@3, recall@3, and reciprocal rank, plus arithmetic means. Since there are two relevant documents, the maximum precision@3 is 2/3. The ranking probe reports actual results without asserting a required quality threshold.

The corpus contains authored summaries of public facts about Mars, photosynthesis, Rust ownership/borrowing, and macOS LaunchAgents. Reference material includes [NASA Mars facts](https://science.nasa.gov/mars/facts/), [OpenStax photosynthesis](https://openstax.org/books/biology-2e/pages/8-introduction), [the Rust ownership chapter](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html), and [Apple's launchd guide](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html). These URLs are documentation references; execution performs no fetches.

This tiny topical corpus is an illustrative retrieval probe. It is not a standard retrieval dataset, a broad semantic-quality estimate, a held-out evaluation, or a comparison with a BF16/quantized reference. The backend currently runs its pinned float32 artifact; no numerical parity with another implementation is claimed.

## Process memory on Linux and macOS

Linux reports `VmRSS` and lifetime peak `VmHWM` from the bounded `/proc/self/status` file in bytes. These process-wide values include native model loading and runtime allocations. If the fields cannot be read, the JSON explicitly marks memory measurement unavailable.

Capture an independent Linux process measurement using the built executable, so compilation is excluded:

```sh
/usr/bin/time -v -o embedding-process.txt env ORT_DISABLE_TELEMETRY=1 \
  ./target/release/vorra-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.so \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --deadline-seconds 300 > embedding-report.json
```

On macOS, collect native process memory with the platform command:

```sh
/usr/bin/time -l env ORT_DISABLE_TELEMETRY=1 \
  ./target/release/vorra-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.dylib \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --threads 2 --repetitions 3 --warmup-iterations 1 \
  --deadline-seconds 300 > embedding-report.json 2> embedding-process.txt
```

The macOS `maximum resident set size` measurement is in bytes; GNU time's Linux maximum RSS is in KiB. Retain the raw platform output alongside the JSON and record the Mac model, memory capacity, architecture, runtime library, thread count, power mode, and competing workload. JSON on macOS marks its internal RSS observation unavailable and points to this external measurement. A measurement includes one process and one shared model session, rather than four simultaneous model copies.

The Linux development environment cannot measure native macOS performance. The separately observed GitHub macOS run below provides supported-platform measurements.

## Observed Linux run

On 2026-10-15, a release build completed the full default public workload on Linux x86_64 with an AMD EPYC 9V74 processor. The environment exposed five logical processors with a four-CPU cgroup quota. Inference used two threads, 2048 tokens, three repetitions, one query/document warmup pair per dimension, and a 300-second overall deadline. Other repository work shared the machine; CPU contention and the operating-system file cache were uncontrolled. These measurements describe this run and do not establish dedicated-machine performance.

The process performed 144 measured inferences and eight warmup inferences. ONNX Runtime initialization took 258.193 ms, model loading took 10,690.221 ms, and the JSON report covered 121,384.318 ms. An independent Python standard-library `subprocess`/`resource.getrusage(RUSAGE_CHILDREN)` measurement of the single executable observed 122.689 seconds of wall time, 50.906 seconds of user CPU time, and 0.881 seconds of system CPU time. Python was only the external measurement wrapper; the executable performed all tokenization and inference in Rust and ONNX Runtime.

| Dimensions | Query median / p95 (ms) | Document median / p95 (ms) | Maximum L2 norm error |
| --- | --- | --- | --- |
| 128 | 374.167 / 755.996 | 707.764 / 1187.810 | 1.890e-8 |
| 256 | 408.333 / 674.947 | 718.896 / 1058.230 | 1.140e-8 |
| 512 | 516.014 / 967.975 | 1026.841 / 1400.151 | 9.326e-9 |
| 768 | 484.571 / 721.974 | 919.962 / 1130.379 | 2.420e-8 |

All measured vectors were finite. Across every dimension and repetition, precision@1, recall@3, and reciprocal rank were 1.0; precision@3 was 2/3. This result applies only to the eight-document authored corpus above. Each dimension had 12 measured queries and 24 measured documents.

The internal Linux peak was 709,427,200 bytes, approximately 676.56 MiB. The independent child-process peak was 692,412 KiB, approximately 676.18 MiB. Linux accounting interfaces can differ slightly; neither figure is an Apple memory observation. `/usr/bin/time` was unavailable in this environment, so the independent observation used `getrusage` instead.

The artifact fingerprint was `b77a8268c326f5b88a9d6fd6d0bcc29317cd27001ca1bf27864405421c816449`, the ONNX export revision was `daa72c51243991dfcaf9f9137d2c573d8f7790c0`, and the ONNX Runtime 1.31.0 library SHA-256 was `21901b1b02233484b80888c16f4f5473742f796bf5528d5d0c4073dbe9802001`. The upstream source reference is metadata rather than certification that the third-party ONNX conversion used that exact source commit.

The release run exited successfully with empty stderr. A separate invocation with a one-second overall deadline exited unsuccessfully after 1.074 seconds, reported the deadline on stderr, and emitted no partial stdout. This focused observation does not establish cancellation behavior at every stage of native session loading or inference.

## Observed macOS run

On 2026-10-15, [GitHub Actions run 38769609299](https://github.com/izyuumi/everything/actions/runs/38769609299) completed real inference at commit `4e212b1dfc7e7507a47e200090e1f2663aed4c97` on a macOS aarch64 hosted runner. Hardware reported `VirtualMac2,1`, 15,032,385,536 bytes of memory and five CPUs. This is a virtual hosted Mac measurement; physical hardware, power mode and competing workloads were not controlled. Configuration was two threads, 2048 tokens, three repetitions, one warmup pair per dimension and a 600-second overall deadline.

The process completed 144 measured inferences and eight warmup inferences. Runtime initialization took 127.112 ms, model loading took 5,039.464 ms and elapsed benchmark time was 19,067.901 ms. Native `/usr/bin/time -l` reported maximum resident size 990,969,856 bytes (945.0625 MiB). Runtime-library SHA-256 was `ff52828ce7c6dd1aea99a5e9f5da9e2786e3d03e467fbf8b6893a4b624334920`; the model artifact fingerprint and ONNX revision matched the Linux run.

| Dimensions | Query median / p95 (ms) | Document median / p95 (ms) | Maximum L2 norm error |
| --- | --- | --- | --- |
| 128 | 58.742 / 98.472 | 120.177 / 135.527 | 1.476e-08 |
| 256 | 53.512 / 71.791 | 119.543 / 145.838 | 1.007e-08 |
| 512 | 49.345 / 65.913 | 104.160 / 127.556 | 9.726e-09 |
| 768 | 50.559 / 61.182 | 98.972 / 117.695 | 2.968e-08 |

All vectors were finite. Each dimension had precision@1, recall@3 and reciprocal rank 1.0, and precision@3 2/3, on the same tiny authored corpus. These are limited observations rather than broad retrieval accuracy or controlled cross-platform comparisons. The workflow publishes its raw report/process output as an artifact and a summary through a GitHub check run. The development proxy could not download the Azure-backed artifact, so the coordinator inspected the GitHub check-run measurements directly.

## Current-core production boundary observations

The direct benchmark measurements above and the production Host boundary are separate evidence. On 2026-10-16, disposable Linux x86_64 observations used copied normal `0.1.0` core artifacts from `dbee29aec2d72795eac37c6abffbeb44b8ee6e04`, built with Rust 1.88. They did not run the later private Settings runtime. Retained external drivers changed only reviewed paths, package/version checks and source/report identity metadata; the two Rust inspection sources and helper-failure driver retained their original bytes. No new automated tests or production implementations were introduced by this observation.

| Exact core artifact | SHA-256 |
| --- | --- |
| Host/CLI `everything` | `79adadbd3323c977ddc3804193cb5d6654789eaa18350d8c566cb1e5e5147f1f` |
| Adjacent `everything-embedding-worker` | `3404db9c5109c16d738b7f8c7f75a78fc2627da2159961d1c99f88c48e2a97b8` |
| Copied `libeverything.rlib` used for external inspectors | `48ea256bf61ee98f88247fd47f47f8bf21d469de9ffd335a29a751db4d7bd0f9` |

The actual pinned float32 ONNX export and Linux ONNX Runtime 1.31.0 digest match the earlier Linux artifact identities above. Lifecycle inspectors linked with the actual Cargo-recorded extern/native paths rather than rebuilding a modified package. Native archives were hashed after linking and before/after runtime; those hashes do not establish a pre-link archive snapshot. Coordinator-retained receipts include `summary.json`, `post-run-audit.json`, `linking.json`, `runtime-after.json`, `sidecar-addendum.json` and the independent retained-state/trace review. Temporary states, traces and model files are not shipped in this repository.

### Host inference and fallback

Four separate disposable profile states exercised real owner-IPC rebuilds at 128/256/512/768 dimensions and returned nonempty vectors and genuine hybrid excerpts with Wiki provenance. All seven walkthrough jobs included default-off Host phases with zero traced model/runtime/vector open or metadata access. Actual missing-helper installation and a disposable outer profile disabling further user namespaces each produced isolation-unavailable status and genuine FTS before configured runtime/model/vector sentinel access. The namespace negative changed no global kernel setting and supplied no unrestricted fallback.

The production helper had its own network namespace, required private mounts, only HOME/LANG/ORT_DISABLE_TELEMETRY/PWD environment keys and no inherited sockets or exposed workspace/home roots. Ordinary positive Hosts retained their original network namespace; that topology observation does not establish a successful provider or relay request. The walkthrough traces contained no AF_INET/AF_INET6 family line across 4,420,404 bytes. One post-load Host maps sample excluded ORT. The observer's `parent_never_mapped_ORT` flag is that sample, not continuous instrumentation.

A real authoritative Wiki update through an explicitly supplied maintenance plan caused stale-source FTS. An incremental rebuild restored hybrid retrieval. pidfd SIGKILL targeted only the exact owned helper; repeated retrieval then reported embedding_process_unavailable with genuine FTS and no respawn. Fourteen ready/stopped Hosts recorded successful Host exits; the deliberately killed helper is a separate outcome. The plans were observer-provided, not model-generated maintenance.

### Cancellation and retained cleanup

The separately linked inspectors observed the following actual core behavior. Times describe those observations and include cleanup where stated; they are not performance targets.

| Case | Observation | Qualification |
| --- | --- | --- |
| Adapter API cancellation | Cancelled after 32 ms with actual worker cleanup | Worker CPU progress was observed; no Embed frame delivery or native function entry was asserted. |
| Framed cancellation | A 3,759-byte Embed write matched the worker read; the same-starttime worker advanced CPU ticks 290→291; cancellation returned in 60 ms and joined worker/monitor | Payload delivery and CPU activity do not instrument native function entry. The adapter became unavailable rather than automatically respawning. |
| Load deadline configured 1 ms | Deadline returned after 136 ms including actual cleanup | No hard 1 ms completion or native-load interruption is established. |
| Inference deadline configured 1 ms | A successfully loaded adapter returned Deadline after 65 ms including cleanup | Request delivery and native inference start were not asserted in this deadline case. |
| Last adapter Arc drop | Drop completed in 20 ms with tracked processes absent | No explicit shutdown call was used. |
| Last MemoryService owner drop | Drop returned in 3 µs; StateLease reacquisition succeeded after 303 ms with tracked processes absent | Fast owner drop is distinct from completion of retained cleanup. |
| Initialization future drop | A non-probe serving helper was observed before dropping the caller future; lease reacquisition succeeded after 207 ms | Caller future completion before drop was not observed; no exact native initialization phase is inferred. |

The lifecycle/cancellation traces contained no Internet-family line across 5,993,957 bytes; all 40 recorded trace process IDs and 12 separately tracked worker/monitor identities were absent after cleanup. The inspector library, linked artifacts and model/runtime hashes were retained. These observations do not establish a hard process-death, RAM or descendant-PID bound.

### Separate same-database restart and profile sequence

A subsequent approved manual sequence reused one new disposable state for five Host starts, still using the exact copied `dbee29a` CLI/helper identities above. It did not rebuild/relink or use later Settings code. One explicit public orchard maintenance plan populated the Wiki; the Wiki hash stayed unchanged through all five stages.

| Start | Actual observation |
| --- | --- |
| Default off | Real lexical/disabled retrieval, then owner CAS enabled 128 with the actual restart-required boundary. |
| Enabled 128 | First query was lexical/missing_generation; a real rebuild produced ready generation 1 with two 128-dimensional vectors and genuine hybrid excerpts/provenance. |
| Enabled 128 restart | The first query was hybrid before any rebuild. |
| Persisted 256 plus restart | The first query was lexical/incompatible_profile; real rebuilding produced distinct ready generation 2 with two 256-dimensional vectors, zero reused vectors and subsequent hybrid retrieval. |
| Persisted Disabled plus restart | First query was lexical/disabled, no helper child remained, and traced configured model/runtime/vector open or metadata access was absent. |

The vector database retained separate fixed float[128]/float[256] tables and active generation 2. The metadata audit loaded no extension and queried no virtual vector values. This establishes the observed 128→256 persisted restart/rebuild boundary, not every dimension pair, interrupted-build recovery or a new live-control API. Explicit Disabled here is a saved configuration plus restart; it does not verify immediate off while preserving operator configuration.

All five real Host drains exited 0. Three point-in-time helper observations retained the Host's ordinary network namespace and a distinct strict helper namespace, approved environment and no inherited sockets/Host roots. All 729 trace PIDs were absent; no AF_INET/AF_INET6 family line appeared across 2,169,400 trace bytes. This is not continuous maps monitoring, native function-entry instrumentation, general network-path absence or a security audit.

The complete four-database DB/WAL/SHM inventory was captured after all joins and before immutable/query_only reads, refusing nonempty WAL. Exact path sets and hashes matched afterward, with no missing/new/changed path or sidecar exception. Integrity/FK/mode checks passed at schema 12/2/1/1 and 0600. Host task/conversation/tool/event counts were zero. The separate independent review rechecked recorded metadata, trace hashes and process absence rather than reexecuting producer action ordering. This run never opened the prior 33 databases and does not erase their producer-inventory qualification below.

### Audit scope and remaining validation

For the original four-profile and lifecycle runs, independent retained-state/trace review covered the combined 33 stopped databases and 1,681 trace files. Integrity and foreign-key checks passed; application/lexical/maintenance/vector schema versions were 12/2/1/1. All recorded trace process IDs were absent and no Internet-family line appeared. This reviewed retained data and trace identities rather than reexecuting every producer command assertion.

The lifecycle producer enumerated related files before opening each read-only/query_only database and rehashed only that initial inventory. Fourteen advisory WAL/SHM paths were newly listed afterward. Previously recorded database hashes were unchanged; unchanged hashes did not prove an unchanged full sidecar set. Sidecar creation during audit is a plausible explanation, not an instrumented attribution. The later independent immutable/query_only audit covered the complete current DB/WAL/SHM set with unchanged hashes and zero-byte WALs. No sidecar was deleted or checkpointed to make this qualification pass.

The original four-profile/lifecycle run did not verify a same-database switch or enabled restart; the separate sequence above supplies only its qualified 128→256 persisted/restart observations. Neither run verifies native task context capture or later Settings live-apply hooks. Main `e7785bd` now contains PR #129 changes to four memory sources, so the earlier `dbee29a`→`50caf6c` whole-memory blob parity is historical. Current isolated adapter/wire/worker/helper/vector-index/contract blobs remain equal where independently compared, while memory orchestration and the native backend's named default have changed. PR #129 passed its actual Rust/benchmark CI, but those checks do not supply runtime observations of the new controls; see [the current composition boundary](../contracts/host-composition.md#current-main-integration). Successful conversational/classifier/provider inference, Apple/UI/Keychain/device/launchd behavior, broad performance/quality, arbitrary native-code security and production readiness remain outside it. Production Apple embeddings still report unavailable and retain FTS until an enforced supported boundary is implemented. Automated acceptance remains deferred to [#32](https://github.com/izyuumi/everything/issues/32); issue [#72](https://github.com/izyuumi/everything/issues/72) requires coordinator reconciliation rather than closure based on this document alone.

## Validation

Validate feature wiring and the benchmark with formatting, feature-enabled compilation, strict Clippy, and a release build. These checks do not require a model load:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --features local-embeddings --locked
cargo clippy --workspace --all-targets --features local-embeddings --locked -- -D warnings
cargo build --workspace --release --features local-embeddings --locked
```

Formatting, feature-enabled checking, strict Clippy, and the release build passed locally for the implementation used in the observed run. Linux/macOS optional-feature builds and the native macOS benchmark also passed in GitHub Actions. Production Apple client behavior and broad performance remain outside these observations.

Automated inference, numerical-parity, cancellation, percentile-calculation, and ranking assertions remain deferred in [#32](https://github.com/izyuumi/everything/issues/32). No tests, mock embedders, fixtures, or benchmark framework are introduced. The binary and its fixed public measurement workload are the runnable benchmark itself.
