# Embedding contracts

Issue [#31](https://github.com/izyuumi/everything/issues/31) defines `memory::embedding`. It provides validated model metadata, text preparation, vector validation, compatibility fingerprints, configuration, and the synchronous `Embedder` interface. This issue supplies no inference backend, mock provider, vector database, or model download. Initial backend implementation is reserved for EmbeddingGemma 2 in [#12](https://github.com/izyuumi/everything/issues/12). Automated contract coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

## Inputs and preprocessing

`EmbeddingInput::query(text)` creates a query. `EmbeddingInput::document(text, title)` creates a document with an optional title. The corresponding `EmbeddingPurpose` remains explicit. Blank text is rejected; source text is limited to 1 MiB and optional titles to 4 KiB. Queries cannot carry document titles, including through deserialization. Source strings retain their original bytes and Unicode normalization.

`EmbeddingPreprocessing::new(revision, query_template, document_template, missing_title)` validates backend-defined text preparation. Both templates contain exactly one `{text}`. A document template may also contain one `{title}`; queries cannot use it. Other braces and placeholders are rejected. Templates and missing-title text are limited to 4 KiB each. Missing or blank titles use the explicit missing-title value.

`format(input)` substitutes into the template without reinterpreting placeholder-looking text inside user input. It does not truncate or tokenize. A backend must enforce its actual token limit and return an error if it cannot process the prepared text. Any tokenizer, pooling, projection, truncation, or other preprocessing behavior that affects output must be identified by the model revision or preprocessing revision. The initial model's documented formatting is query `task: search result | query: {text}` and document `title: {title} | text: {text}`, with missing title `none`; this contract does not claim inference verification.

## Specifications, selected profiles, and configuration

`EmbeddingSpec::new(model, revision, supported_dimensions, preprocessing, normalization, metric)` identifies one immutable embedding implementation and artifact. Model and revision strings must be nonempty, at most 256 UTF-8 bytes, and contain no surrounding whitespace or control characters. Model revision must identify the actual weights, tokenizer, and quantization artifact; a repository revision alone cannot distinguish two differently quantized files. Backend construction is responsible for checking that the declared identity matches what it loads.

Supported dimensions form a validated, sorted set with 1–64 distinct entries; each entry is between 1 and 65,536. Keeping the generic shape independent of a model allows future user-provided implementations without a plugin framework. Only EmbeddingGemma 2 is planned as an initial backend, with selectable dimensions 128, 256, 512, and 768. This issue does not add another model implementation.

`EmbeddingProfile::new(spec, dimensions)` selects one advertised output shape and rejects an unsupported dimension. `EmbeddingNormalization` is `none` or `unit_l2`. `EmbeddingMetric` is `cosine`, `dot_product`, or `squared_euclidean`. They identify retrieval semantics, rather than silently changing vectors. Configuration uses a tagged `EmbeddingConfig` enum, defaults to `Disabled`, and serializes as `{"mode":"disabled"}` or `{"mode":"enabled","profile":...}`. Disabled configuration contains no hidden model profile and performs no inference. Runtime code must avoid constructing a backend when disabled; configuration itself does not create one.

Constructors and deserialization enforce the same invariants. Metadata structures reject unknown fields. Fields remain private so callers cannot mutate a valid profile into an invalid one. Callers must bound serialized input bytes before parsing JSON; value validation does not make an unbounded JSON parser safe.

## Fingerprints and compatibility

`EmbeddingFingerprint` is a validated SHA-256 hexadecimal digest, serialized as a 64-character lowercase string. Parsing accepts either hexadecimal case and canonicalizes it. Fingerprints contain no source text or document title. They describe settings and are not a substitute for authenticating model downloads.

The exact encoding is deterministic and independent of JSON field ordering, machine word size, and endianness. Strings are encoded as an unsigned 64-bit little-endian UTF-8 byte count followed by their original UTF-8 bytes. All digests use SHA-256 and lowercase hexadecimal output.

| Fingerprint | Encoding after the namespace bytes |
| --- | --- |
| Spec | Model string; model revision string; supported-dimension count as u64 LE; each sorted dimension as u32 LE; preprocessing revision, query template, document template, and missing-title strings; normalization key string; metric key string |
| Profile | Spec fingerprint as a length-prefixed lowercase hexadecimal string; selected dimension as u32 LE |
| Index | Profile fingerprint as a length-prefixed lowercase hexadecimal string; document-schema string |

Namespace bytes are respectively `everything.embedding.spec.v1` followed by one NUL byte, `everything.embedding.profile.v1` followed by NUL, and `everything.embedding.index.v1` followed by NUL. Normalization keys are `none` and `unit_l2`; metric keys are `cosine`, `dot_product`, and `squared_euclidean`.

A spec's advertised dimension set is part of its identity, so adding a dimension conservatively invalidates old profiles even if output at an existing dimension happens to remain unchanged. Model revision, selected dimension, formatting, normalization, and metric changes also invalidate compatibility.

`EmbeddingIndexSpec::new(profile, document_schema)` adds the source document/chunk schema to an index fingerprint. The caller must identify title selection, chunking rules and settings, and other source transformations in that schema revision. Its identifier follows the same 256-byte rule. `profile.ensure_compatible(stored_fingerprint)` and `index_spec.ensure_compatible(stored_fingerprint)` require exact matches and return typed incompatibility errors. Namespaces keep profile fingerprints distinct from index fingerprints. A vector store must validate metadata before reading or adding vectors and must rebuild into a separate generation on incompatibility; no index switching is implemented here.

## Vectors and execution

`EmbeddingVector::new(profile, values)` checks the selected dimension, rejects NaN and infinities, and validates declared normalization. Cosine vectors must have nonzero norm. Unit L2 vectors must have norm within an absolute tolerance of 0.0001 of 1. Norm accumulation uses f64. Zero vectors remain valid for unnormalized dot-product or squared-Euclidean profiles. `normalize_l2(values)` validates finite values and a nonzero norm before normalizing, with a maximum of 65,536 dimensions. Output values are exposed immutably, and vectors carry their profile fingerprint. Loading stored vector data must reconstruct a vector with a validated profile; vectors have no unchecked deserialization path.

`Embedder: Send + Sync` exposes `profile()` and `embed(input, cancellation) -> Result<EmbeddingVector, EmbeddingError>`. An instance serves one immutable profile. Implementations must return a vector compatible with that profile, honor cancellation where their backend permits it, and report backend errors. There is no external gateway or network operation in this contract.

The API is synchronous because local CPU inference is blocking work. Async callers must run backend loading, inference, and the batch adapter on a blocking worker, not a Tokio executor thread. The implementation may serialize access to a native model/context internally. Cancellation is a `tokio_util::sync::CancellationToken`; it requires no running async executor.

The optional EmbeddingGemma 2 backend requires `ORT_DISABLE_TELEMETRY=1` at process start. `EmbeddingRuntime::require_telemetry_suppression()` returns a typed `TelemetrySuppressionRequired` backend error for every other or missing value. `initialize` invokes it before canonicalization, hashing or native initialization, and retains API telemetry suppression afterwards. A composing host can preflight before its own artifact verification and retain genuine FTS with explicit unavailable status. No environment mutation occurs. This control addresses the documented initialization-before-API-disable behavior of the supported trusted runtime; it is not OS network confinement or attestation of arbitrary native code. See [the actual runtime boundary](../architecture/embeddinggemma2.md) and [#72](https://github.com/izyuumi/everything/issues/72).

`embed_batch(embedder, inputs, cancellation)` processes inputs sequentially in original order, limits batches to 64 inputs and 16 MiB of aggregate source text and titles, checks cancellation before and after each inference, and revalidates output shape, norm, and fingerprint. It returns no partial batch on failure. This helper favors a bounded initial implementation over a batching framework; a 65,536-dimensional profile has at most 16 MiB of f32 vector values in a 64-input result. Real model resource limits remain the backend's responsibility.

## Validation

Formatting, workspace compilation, strict Clippy, and release builds passed for this implementation. Disposable manual runtime checks exercised template substitution without reinterpreting user placeholders, serialized metadata roundtrips, incompatible model/revision/dimension rejection, invalid deserialization, finite and normalized vector rules, and disabled configuration without creating a mock embedder or calling a model. An independent Python encoding reproduced the Rust spec, profile, and index fingerprints exactly. These checks do not establish the deferred automated coverage or actual model inference correctness.
