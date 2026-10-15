# Local EmbeddingGemma 2 benchmark

Issue [#12](https://github.com/izyuumi/everything/issues/12) includes the standalone `everything-embedding-bench` binary. It performs real local inference through the [EmbeddingGemma 2 ONNX backend](../architecture/embeddinggemma2.md), sharing one loaded model across the 128-, 256-, 512-, and 768-dimensional profiles. It does not download models, start Python, call a model service, create a vector index, or read user documents.

## Build and run

Prepare the pinned model bundle and a trusted compatible ONNX Runtime shared library according to the backend documentation. Selecting a native library explicitly authorizes loading its code; do not use an untrusted library. Model files remain outside the repository. The `local-embeddings` Cargo feature is required; default builds skip the backend and benchmark binary.

```sh
cargo build --release --features local-embeddings --bin everything-embedding-bench
./target/release/everything-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.dylib \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --threads 2 \
  --repetitions 3 \
  --warmup-iterations 1 \
  --deadline-seconds 300 > embedding-report.json
```

Use the corresponding `.so` library on Linux. Run `everything-embedding-bench --help` for bounds and options. Threads are limited to 1–16, tokens to 8–8192 including backend special tokens, repetitions to 1–10, and warmup iterations to 0–3. The fixed corpus has eight short documents and four queries; each profile performs at most 120 measured inferences and six warmup inferences. All four profiles are always measured. There are no user-supplied text inputs or unbounded batches.

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
/usr/bin/time -v -o embedding-process.txt \
  ./target/release/everything-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.so \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --deadline-seconds 300 > embedding-report.json
```

On macOS, collect native process memory with the platform command:

```sh
/usr/bin/time -l \
  ./target/release/everything-embedding-bench \
  --runtime-library /absolute/path/to/libonnxruntime.dylib \
  --model-dir /absolute/path/to/embeddinggemma-2-onnx \
  --threads 2 --repetitions 3 --warmup-iterations 1 \
  --deadline-seconds 300 > embedding-report.json 2> embedding-process.txt
```

The macOS `maximum resident set size` measurement is in bytes; GNU time's Linux maximum RSS is in KiB. Retain the raw platform output alongside the JSON and record the Mac model, memory capacity, architecture, runtime library, thread count, power mode, and competing workload. JSON on macOS marks its internal RSS observation unavailable and points to this external measurement. A measurement includes one process and one shared model session, rather than four simultaneous model copies.

No Apple hardware or Apple runtime measurements are available from the Linux development environment. A Linux result must not be presented as a macOS RAM or latency result.

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

## Validation

Validate feature wiring and the benchmark with formatting, feature-enabled compilation, strict Clippy, and a release build. These checks do not require a model load:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --features local-embeddings --locked
cargo clippy --workspace --all-targets --features local-embeddings --locked -- -D warnings
cargo build --workspace --release --features local-embeddings --locked
```

Formatting, feature-enabled checking, strict Clippy, and the release build passed locally for the implementation used in the observed run. macOS compilation, runtime-library compatibility, native RSS, and latency require their own supported-platform validation.

Automated inference, numerical-parity, cancellation, percentile-calculation, and ranking assertions remain deferred in [#32](https://github.com/izyuumi/everything/issues/32). No tests, mock embedders, fixtures, or benchmark framework are introduced. The binary and its fixed public measurement workload are the runnable benchmark itself.
