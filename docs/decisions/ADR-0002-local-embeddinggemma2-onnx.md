# ADR-0002: Local EmbeddingGemma 2 through ONNX Runtime

## Status

Accepted for the initial optional text embedding backend. Native Linux inference has been verified; macOS measurements and numerical parity against an independent reference remain unverified.

## Context

Everything requires fully local EmbeddingGemma 2 inference, optional configuration, selectable dimensions, and no Python daemon. Markdown remains authoritative and FTS5 must work without inference. The published GGUF uses the new `gemma-embedding2` architecture, which neither the selected crates.io llama.cpp Rust binding nor its current official Git submodule supports. Native upstream llama.cpp added support after the binding's bundled revision.

The published, pinned ONNX conversion contains the trained projection, bidirectional attention, masked mean pooling, and normalized 768-dimensional sentence output. Established Rust `ort` and `tokenizers` libraries can execute this graph directly without a custom model implementation or local unsafe code.

## Decision

Use an opt-in, default-off `local-embeddings` feature with `ort = 2.0.0-rc.10` and `tokenizers = 0.22.2`. Load an explicit trusted local ONNX Runtime shared library and the pinned float32 EmbeddingGemma 2 text bundle. Disable telemetry and use CPU execution. Perform no model or native-library downloads in application code.

Verify bounded artifacts by size and SHA-256, include actual artifact and runtime identities in profile fingerprints, and keep the operator-owned files immutable throughout session lifetime. The runtime's path-based opens are not a defense against a concurrent malicious same-UID writer. Use the graph's complete sentence output, retain the selected leading 128/256/512/768 dimensions, and renormalize through the generic vector contract.

Share one loaded session across dimension profiles, admit one native operation at a time, bound input tokens and CPU threads, and use safe runtime termination with joined cleanup. Document native session construction and active kernels as cooperative cancellation boundaries. Run all blocking work outside async executor workers.

## Consequences

The initial implementation runs locally with an established backend and preserves the generic embedder boundary. Users provision a native runtime and roughly 1.1 GB of float32 model artifacts explicitly. The application requires no Python installation. Default builds and disabled embedding configuration do not load this model.

Only the pinned model conversion is accepted. Updating artifacts, tokenizer behavior, native libraries, or preprocessing changes compatibility identity and requires a new vector index generation. Numerical parity with another conversion, quantitative retrieval quality, Apple performance, and deferred automated coverage require separate evidence. A supported official llama.cpp binding can be reconsidered later without changing the generic embedding interface.
