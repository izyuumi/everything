# Local EmbeddingGemma 2 inference

Issue [#12](https://github.com/izyuumi/everything/issues/12) adds an optional native Rust adapter for text embeddings from `google/embeddinggemma-2`. The `local-embeddings` Cargo feature is disabled by default. The adapter uses the established `ort` binding to ONNX Runtime and Hugging Face `tokenizers`; it neither runs Python nor starts a daemon. Model downloads, native runtime installation, and model loading are explicit. No user input is sent to a remote service.

## Native backend selection

The supplied official GGUF checkpoint identifies its architecture as `gemma-embedding2`. The bundled llama.cpp in crates.io `llama-cpp-2` / `llama-cpp-sys-2` 0.1.159 registers only `gemma-embedding`, which is the earlier model. The official Rust wrapper main revision `e40decdb45d589925a370679d29f30454805c271` uses llama.cpp `d81235049384534c167caea52b85a694f6103d14` and also lacks the new architecture. Native upstream llama.cpp added the model in `4fbc76dec51d0add466f0210855c0596589b60d4`, after those revisions. The adapter does not relabel the GGUF, substitute the earlier model, implement a projection, or vendor a modified inference engine.

The supported fallback is the published ONNX text graph from [onnx-community/embeddinggemma-2-ONNX](https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/tree/daa72c51243991dfcaf9f9137d2c573d8f7790c0), revision `daa72c51243991dfcaf9f9137d2c573d8f7790c0`. The conversion declares [google/embeddinggemma-2](https://huggingface.co/google/embeddinggemma-2/tree/914f7f89142e33e77833254d9c9b90c3cef7303b) as its base model. The upstream model card was inspected at revision `914f7f89142e33e77833254d9c9b90c3cef7303b`; the converted repository does not record the exact upstream weights revision. The `source_ref` metadata records that reference, while compatibility relies on actual converted-artifact digests. The ONNX graph contains the learned 512-to-768 projection, attention-mask-aware mean pooling, and L2 normalization. It returns `sentence_embedding` with shape `[batch, 768]`. The adapter supplies integer token IDs, an all-one text attention mask, and empty `[0, 512]` image, video, and audio feature inputs. No modality encoder is loaded.

Only the pinned float32 graph is accepted. The model card warns against float16 activations. Additional quantizations, multimodal inference, and other embedding models are outside this implementation.

## Explicit local artifacts and trust

The caller supplies a local bundle root containing exactly these required artifact paths. Additional unrelated files are ignored.

| Path | Bytes | SHA-256 |
| --- | ---: | --- |
| `config.json` | 5,031 | `8d011bfe08b5e345bbe0b81e5c6fd02c381920b345b986047bc2a33ce7b90d1d` |
| `tokenizer.json` | 32,170,510 | `4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4` |
| `onnx/model.onnx` | 421,421 | `bc47de15f81208a5c99e5ab10f746d5e33b51ea228b7dc0bef9c133a94f1c1c3` |
| `onnx/model.onnx_data` | 1,084,170,240 | `9fd452bfc6916e5a92f080f8dada35598ca4512b21c65897d4c5974d5f9630a2` |

Loading checks regular-file sizes, bounds hashing and JSON/tokenizer reads, rejects paths resolving outside the bundle root, verifies all digests, and confirms the expected model identity and architecture dimensions from configuration. The source files must remain immutable while loading; the native runtime opens the graph and external weights by path after verification. These checks do not make a concurrently writable artifact directory a hostile-filesystem sandbox.

`EmbeddingRuntime::initialize(native_library_path)` takes an explicit local ONNX Runtime shared library, hashes it, disables telemetry, and initializes a CPU-only runtime. The library must be a trusted regular file no larger than 256 MiB. Loading a native library grants it the host process's authority. Runtime provisioning is an application trust decision; the adapter does not download libraries or add execution providers. `ort` uses process-wide initialization, so initialize once, retain the returned `Arc<EmbeddingRuntime>`, and share it. A failed native initialization is reported; a subsequent attempt may require restarting the process because the underlying binding retains its initialization path.

Installing a prebuilt ONNX Runtime distribution is sufficient; a Python installation is not required by the application. A development environment may obtain the same native shared library from an `onnxruntime` wheel, but the Rust process opens the shared library directly and never invokes Python. Versioned `ort = 2.0.0-rc.10` uses the ONNX Runtime 1.22 API; an installed runtime must provide that API. Actual validation must identify its native build and platform.

## Profiles and dimensions

`EmbeddingGemma2::load(runtime, bundle_root, options, cancellation)` loads one native session and initially exposes a 768-dimensional profile through the generic [`Embedder` contract](../contracts/embeddings.md). `with_dimensions(128 | 256 | 512 | 768)` creates a profile sharing the existing session and model allocation. Other dimensions are rejected. The graph produces the full learned embedding; the adapter retains the leading selected dimensions, rejects nonfinite output, renormalizes after truncation, and constructs a validated unit vector.

Query formatting is exactly `task: search result | query: {text}`. Documents use `title: {title} | text: {text}`, with `none` for absent or blank titles. Placeholder-looking user text remains literal. The pinned tokenizer inserts BOS and EOS. It applies no truncation or padding; inputs exceeding the configured token ceiling, including the task prefix and special tokens, fail with a typed limit error.

The spec fingerprint includes the source-model-card reference, converted graph revision, a deterministic digest of all four artifact names and digests, native runtime library digest, exact binding/tokenizer versions, float32 execution, graph-owned pooling/projection, special-token policy, long-input rejection, query/document templates, selected dimension, metric, and normalization. Changing model artifacts or native runtime therefore requires a compatible new index generation, rather than silently mixing vector profiles.

## Resource ownership and cancellation

The runtime permits one loaded model allocation. Dimension variants share it through `Arc`. Dropping the final variant releases the native session and tokenizer before allowing another model load. A fail-fast admission lock permits one load or inference call at a time; contending callers receive a typed busy error. The adapter does not accumulate queued blocking jobs.

Defaults use two CPU threads, one inter-op thread, a 2,048-token input ceiling, a 30-second inference deadline, and a 120-second load deadline. Configuration bounds threads to 1–16, input tokens to 8–8,192, and each deadline to a positive duration at most 300 seconds. Sequential graph execution, disabled CPU arenas, and disabled memory-pattern retention limit persistent resource growth. The pinned float32 weights require roughly 1.1 GB of artifact storage, plus tokenizer and native working memory. There is no operating-system memory quota; workload and native-runtime memory must be measured on the intended host.

Hashing and load phases check cancellation and deadlines. Native session construction is a blocking, non-preemptible phase in the safe wrapper; cancellation or an elapsed deadline is reported after it returns and cleanup completes. Inference uses the safe `RunOptions::terminate` API from one scoped monitor thread, checked every 10 ms. Termination interrupts graph execution at native runtime checkpoints, with an active kernel potentially finishing first. Every return waits for the scoped monitor and native call to finish; no timed-out inference worker is detached. These are cooperative cancellation and deadline semantics, not an immediate hard real-time guarantee.

Async runtime integration must use a bounded blocking-worker admission mechanism, call this synchronous API off Tokio worker threads, and retain ownership until cleanup finishes. Disabled embedding configuration must skip runtime initialization and model loading. After an already enabled service is disabled, its model handles should be dropped; a retained lightweight runtime handle allows a later enable without repeating process-wide native initialization.

## Validation and remaining work

Actual native Rust inference on the pinned float32 graph produced finite unit vectors at 128, 256, 512, and 768 dimensions on Linux x86_64 with ONNX Runtime 1.31.0 (build commit `402979a`). A Mars query ranked a Mars document above an unrelated bread document at each dimension. These three-input observations establish a working path and directional sanity check, rather than a quantitative accuracy evaluation. Active cancellation and a 250 ms inference deadline were manually exercised; the deadline returned after 346 ms including native cleanup, and the session remained usable after both interruptions. Concurrent admission, already loaded models, pre-cancelled requests, and token-budget rejection returned typed errors. Release benchmark measurements are recorded in the [benchmark guide](../development/embedding-benchmark.md) when available. This document does not claim macOS validation, quantitative model accuracy, or production readiness. Automated numerical, cancellation, resource, and index compatibility coverage remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).
