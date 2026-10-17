use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{Provenance, SearchHit, WikiError, WikiIndex};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("retrieval operation cancelled")]
    Cancelled,
    #[error("wiki retrieval failed: {0}")]
    Wiki(#[from] WikiError),
    #[cfg(feature = "local-embeddings")]
    #[error("semantic retrieval failed: {0}")]
    Vector(#[from] super::vector::VectorError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    Lexical,
    Hybrid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    Disabled,
    MissingGeneration,
    IncompatibleProfile,
    StaleSource,
    SemanticFailure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalSummary {
    pub mode: RetrievalMode,
    pub fallback: Option<FallbackReason>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RetrievalHit {
    pub chunk_id: String,
    pub title: String,
    pub heading: String,
    pub excerpt: String,
    pub provenance: Provenance,
    pub lexical_score: Option<f64>,
    pub vector_distance: Option<f64>,
    pub fusion_score: f64,
}

#[derive(Clone, Debug)]
pub struct SearchResponse {
    pub hits: Vec<RetrievalHit>,
    pub summary: RetrievalSummary,
}

pub fn lexical_search(
    index: &WikiIndex,
    query: &str,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<SearchResponse, SearchError> {
    if cancellation.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    let hits = index.search_any(query, limit)?;
    if cancellation.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    Ok(lexical_response(hits, FallbackReason::Disabled, None))
}

fn lexical_response(
    hits: Vec<SearchHit>,
    fallback: FallbackReason,
    diagnostic: Option<String>,
) -> SearchResponse {
    SearchResponse {
        hits: hits
            .into_iter()
            .enumerate()
            .map(|(rank, hit)| RetrievalHit {
                chunk_id: hit.chunk_id,
                title: hit.title,
                heading: hit.heading,
                excerpt: hit.excerpt,
                provenance: hit.provenance,
                lexical_score: Some(hit.score),
                vector_distance: None,
                fusion_score: 1.0 / (61.0 + rank as f64),
            })
            .collect(),
        summary: RetrievalSummary {
            mode: RetrievalMode::Lexical,
            fallback: Some(fallback),
            diagnostic,
        },
    }
}

#[cfg(feature = "local-embeddings")]
mod hybrid {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::memory::Wiki;
    use crate::memory::consolidate::SemanticCapture;
    use crate::memory::embedding::{Embedder, EmbeddingError, EmbeddingIndexSpec, EmbeddingInput};
    use crate::memory::index::IndexedChunk;
    use crate::memory::vector::{
        GenerationStatus, VectorError, VectorIndex, VectorStatus, checkpoint,
    };

    pub const DOCUMENT_SCHEMA: &str = "wiki-heading-chunks-v1;title=page-title;body=full-chunk;unicode=preserve;overflow=reject;blank=skip;source=profiled-manifest-v2";

    #[derive(Clone, Debug)]
    pub struct BuildBudget {
        pub max_chunks: usize,
        pub max_elapsed: Duration,
    }

    impl Default for BuildBudget {
        fn default() -> Self {
            Self {
                max_chunks: 64,
                max_elapsed: Duration::from_secs(30),
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum BuildStop {
        Complete,
        ChunkBudget,
        TimeBudget,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct BuildReport {
        pub generation: GenerationStatus,
        pub stop: BuildStop,
    }

    pub struct HybridSearch {
        index: VectorIndex,
        embedder: Arc<dyn Embedder>,
        spec: EmbeddingIndexSpec,
    }

    impl HybridSearch {
        pub fn new(index: VectorIndex, embedder: Arc<dyn Embedder>) -> Result<Self, VectorError> {
            crate::memory::vector::validate_profile(embedder.profile())?;
            let spec = EmbeddingIndexSpec::new(embedder.profile().clone(), DOCUMENT_SCHEMA)?;
            Ok(Self {
                index,
                embedder,
                spec,
            })
        }

        pub fn status(&self) -> Result<VectorStatus, VectorError> {
            self.index.status()
        }
        pub fn specification(&self) -> &EmbeddingIndexSpec {
            &self.spec
        }

        pub fn rebuild(
            &mut self,
            wiki: &mut Wiki,
            budget: BuildBudget,
            fresh: bool,
            cancellation: &CancellationToken,
        ) -> Result<BuildReport, VectorError> {
            if !(1..=1_024).contains(&budget.max_chunks)
                || budget.max_elapsed.is_zero()
                || budget.max_elapsed > Duration::from_secs(300)
            {
                return Err(VectorError::Invalid(
                    "build chunks require 1..1024 and elapsed requires positive..300 seconds",
                ));
            }
            self.rebuild_inner(wiki, budget, fresh, cancellation, None)
        }

        pub(crate) fn rebuild_guarded(
            &mut self,
            wiki: &mut Wiki,
            budget: BuildBudget,
            fresh: bool,
            cancellation: &CancellationToken,
            semantic: &SemanticCapture,
        ) -> Result<BuildReport, VectorError> {
            if !(1..=1_024).contains(&budget.max_chunks)
                || budget.max_elapsed.is_zero()
                || budget.max_elapsed > Duration::from_secs(300)
            {
                return Err(VectorError::Invalid("invalid semantic build budget"));
            }
            self.rebuild_inner(wiki, budget, fresh, cancellation, Some(semantic))
        }

        fn rebuild_inner(
            &mut self,
            wiki: &mut Wiki,
            budget: BuildBudget,
            fresh: bool,
            cancellation: &CancellationToken,
            semantic: Option<&SemanticCapture>,
        ) -> Result<BuildReport, VectorError> {
            let result = self.build(wiki, budget, fresh, cancellation, semantic);
            if let Err(error) = &result {
                self.index.failure(error)?;
            }
            result
        }

        fn build(
            &mut self,
            wiki: &mut Wiki,
            budget: BuildBudget,
            fresh: bool,
            cancellation: &CancellationToken,
            semantic: Option<&SemanticCapture>,
        ) -> Result<BuildReport, VectorError> {
            semantic_checkpoint(semantic)?;
            checkpoint(cancellation)?;
            wiki.synchronize()?;
            checkpoint(cancellation)?;
            let source = wiki.index().source_fingerprint()?;
            semantic_checkpoint(semantic)?;
            let generation = self.index.prepare(&self.spec, &source, fresh)?;
            semantic_checkpoint(semantic)?;
            if generation.ready {
                return Ok(BuildReport {
                    generation,
                    stop: BuildStop::Complete,
                });
            }
            let started = Instant::now();
            let mut processed = 0;
            let mut cursor = generation.after_chunk_id;
            loop {
                semantic_checkpoint(semantic)?;
                checkpoint(cancellation)?;
                if processed >= budget.max_chunks {
                    return self.progress(generation.id, BuildStop::ChunkBudget);
                }
                if started.elapsed() >= budget.max_elapsed {
                    return self.progress(generation.id, BuildStop::TimeBudget);
                }
                let page = wiki.index().chunks_after(
                    cursor.as_deref(),
                    (budget.max_chunks - processed).min(32),
                    16 * 1024 * 1024,
                )?;
                for chunk in page.chunks {
                    semantic_checkpoint(semantic)?;
                    checkpoint(cancellation)?;
                    if started.elapsed() >= budget.max_elapsed {
                        return self.progress(generation.id, BuildStop::TimeBudget);
                    }
                    if chunk.chunk.markdown.trim().is_empty() {
                        self.index
                            .record(generation.id, &chunk.chunk.id, None, None, false)?;
                    } else {
                        let input = EmbeddingInput::document(
                            chunk.chunk.markdown.clone(),
                            Some(chunk.title.clone()),
                        )?;
                        let hash = input_hash(&self.spec, &input)?;
                        let existing = self.index.reusable(&self.spec, &chunk.chunk.id, &hash)?;
                        let reused = existing.is_some();
                        let vector = match existing {
                            Some(vector) => vector,
                            None => {
                                semantic_checkpoint(semantic)?;
                                match self.embedder.embed(&input, cancellation) {
                                    Err(EmbeddingError::Cancelled) => {
                                        return Err(VectorError::Cancelled);
                                    }
                                    result => result?,
                                }
                            }
                        };
                        checkpoint(cancellation)?;
                        self.index.record(
                            generation.id,
                            &chunk.chunk.id,
                            Some(&hash),
                            Some(&vector),
                            reused,
                        )?;
                    }
                    processed += 1;
                    cursor = Some(chunk.chunk.id);
                }
                if page.next_after_id.is_none() {
                    checkpoint(cancellation)?;
                    wiki.synchronize()?;
                    checkpoint(cancellation)?;
                    let current = wiki.index().source_fingerprint()?;
                    if current != source {
                        return Err(VectorError::SourceChanged);
                    }
                    semantic_checkpoint(semantic)?;
                    let generation = self.index.activate(generation.id, &current)?;
                    semantic_checkpoint(semantic)?;
                    return Ok(BuildReport {
                        generation,
                        stop: BuildStop::Complete,
                    });
                }
            }
        }

        fn progress(&self, id: i64, stop: BuildStop) -> Result<BuildReport, VectorError> {
            let generation = self
                .index
                .status()?
                .building
                .filter(|value| value.id == id)
                .ok_or(VectorError::Incompatible)?;
            Ok(BuildReport { generation, stop })
        }

        pub fn search(
            &self,
            wiki: &mut Wiki,
            query: &str,
            limit: usize,
            cancellation: &CancellationToken,
        ) -> Result<SearchResponse, SearchError> {
            self.search_inner(wiki, query, limit, cancellation, None)
        }

        pub(crate) fn search_guarded(
            &self,
            wiki: &mut Wiki,
            query: &str,
            limit: usize,
            cancellation: &CancellationToken,
            semantic: &SemanticCapture,
        ) -> Result<SearchResponse, SearchError> {
            self.search_inner(wiki, query, limit, cancellation, Some(semantic))
        }

        fn search_inner(
            &self,
            wiki: &mut Wiki,
            query: &str,
            limit: usize,
            cancellation: &CancellationToken,
            semantic: Option<&SemanticCapture>,
        ) -> Result<SearchResponse, SearchError> {
            if cancellation.is_cancelled() {
                return Err(SearchError::Cancelled);
            }
            if limit > 100 {
                return Err(WikiError::Limit {
                    resource: "hybrid search results",
                    limit: 100,
                }
                .into());
            }
            wiki.synchronize()?;
            let lexical = wiki.index().search_any(query, 100)?;
            if cancellation.is_cancelled() {
                return Err(SearchError::Cancelled);
            }
            if limit == 0 || query.trim().is_empty() {
                return Ok(lexical_response(Vec::new(), FallbackReason::Disabled, None));
            }
            if semantic.is_some_and(|capture| !capture.admitted()) {
                return Ok(self.fallback(wiki, query, limit, FallbackReason::Disabled, None)?);
            }
            let response = match self.semantic(wiki, query, cancellation, semantic) {
                Ok(Semantic::Fallback(reason, diagnostic)) => {
                    self.fallback(wiki, query, limit, reason, diagnostic)?
                }
                Ok(Semantic::Ready(semantic)) => match fuse(wiki.index(), lexical, semantic, limit)
                {
                    Ok(response) => response,
                    Err(error @ WikiError::Limit { .. }) => self.fallback(
                        wiki,
                        query,
                        limit,
                        FallbackReason::SemanticFailure,
                        Some(error.to_string().chars().take(256).collect()),
                    )?,
                    Err(error) => return Err(error.into()),
                },
                Err(VectorError::Cancelled | VectorError::Embedding(EmbeddingError::Cancelled)) => {
                    return Err(SearchError::Cancelled);
                }
                Err(VectorError::Wiki(error @ WikiError::Limit { .. })) => self.fallback(
                    wiki,
                    query,
                    limit,
                    FallbackReason::SemanticFailure,
                    Some(error.to_string().chars().take(256).collect()),
                )?,
                Err(VectorError::Wiki(error)) => return Err(error.into()),
                Err(error) => self.fallback(
                    wiki,
                    query,
                    limit,
                    FallbackReason::SemanticFailure,
                    Some(error.to_string().chars().take(256).collect()),
                )?,
            };
            if cancellation.is_cancelled() {
                return Err(SearchError::Cancelled);
            }
            if semantic.is_some_and(|capture| !capture.admitted()) {
                return Ok(self.fallback(wiki, query, limit, FallbackReason::Disabled, None)?);
            }
            Ok(response)
        }

        fn fallback(
            &self,
            wiki: &mut Wiki,
            query: &str,
            limit: usize,
            reason: FallbackReason,
            diagnostic: Option<String>,
        ) -> Result<SearchResponse, WikiError> {
            wiki.synchronize()?;
            Ok(lexical_response(
                wiki.index().search_any(query, limit)?,
                reason,
                diagnostic,
            ))
        }

        fn semantic(
            &self,
            wiki: &mut Wiki,
            query: &str,
            cancellation: &CancellationToken,
            semantic: Option<&SemanticCapture>,
        ) -> Result<Semantic, VectorError> {
            checkpoint(cancellation)?;
            if semantic.is_some_and(|capture| !capture.admitted()) {
                return Ok(Semantic::Fallback(FallbackReason::Disabled, None));
            }
            let status = self.index.status()?;
            let Some(active) = status.active else {
                return Ok(Semantic::Fallback(
                    FallbackReason::MissingGeneration,
                    status.last_error,
                ));
            };
            if active.spec != self.spec {
                return Ok(Semantic::Fallback(
                    FallbackReason::IncompatibleProfile,
                    status.last_error,
                ));
            }
            if wiki.index().source_fingerprint()? != active.source_fingerprint {
                return Ok(Semantic::Fallback(
                    FallbackReason::StaleSource,
                    status.last_error,
                ));
            }
            let input = EmbeddingInput::query(query)?;
            if semantic.is_some_and(|capture| !capture.admitted()) {
                return Ok(Semantic::Fallback(FallbackReason::Disabled, None));
            }
            let vector = match self.embedder.embed(&input, cancellation) {
                Err(EmbeddingError::Cancelled) => return Err(VectorError::Cancelled),
                result => result?,
            };
            checkpoint(cancellation)?;
            let nearest = self.index.nearest(&active, &vector, 100)?;
            wiki.synchronize()?;
            checkpoint(cancellation)?;
            if wiki.index().source_fingerprint()? != active.source_fingerprint {
                return Ok(Semantic::Fallback(FallbackReason::StaleSource, None));
            }
            let ids: Vec<_> = nearest.iter().map(|(id, _, _)| id.clone()).collect();
            let chunks: BTreeMap<_, _> = wiki
                .index()
                .chunks_by_ids(&ids, 16 * 1024 * 1024)?
                .into_iter()
                .map(|chunk| (chunk.chunk.id.clone(), chunk))
                .collect();
            let mut results = Vec::new();
            let mut seen = BTreeSet::new();
            for (id, distance, hash) in nearest {
                if !seen.insert(id.clone()) {
                    return Err(VectorError::Incompatible);
                }
                let Some(chunk) = chunks.get(&id) else {
                    return Ok(Semantic::Fallback(FallbackReason::StaleSource, None));
                };
                let input = document_input(chunk)?;
                if input_hash(&self.spec, &input)? != hash {
                    return Ok(Semantic::Fallback(FallbackReason::StaleSource, None));
                }
                results.push((id, distance));
            }
            if semantic.is_some_and(|capture| !capture.admitted()) {
                return Ok(Semantic::Fallback(FallbackReason::Disabled, None));
            }
            Ok(Semantic::Ready(results))
        }
    }

    fn semantic_checkpoint(semantic: Option<&SemanticCapture>) -> Result<(), VectorError> {
        if semantic.is_some_and(|capture| !capture.admitted()) {
            return Err(VectorError::Cancelled);
        }
        Ok(())
    }

    type RankPair = (Option<(usize, f64)>, Option<(usize, f64)>);

    fn fuse(
        index: &WikiIndex,
        lexical: Vec<SearchHit>,
        semantic: Vec<(String, f64)>,
        limit: usize,
    ) -> Result<SearchResponse, WikiError> {
        let mut ranks = BTreeMap::<String, RankPair>::new();
        for (rank, hit) in lexical.iter().enumerate() {
            ranks.entry(hit.chunk_id.clone()).or_default().0 = Some((rank + 1, hit.score));
        }
        for (rank, (id, distance)) in semantic.iter().enumerate() {
            ranks.entry(id.clone()).or_default().1 = Some((rank + 1, *distance));
        }
        let ids: Vec<_> = ranks.keys().cloned().collect();
        let chunks = index.chunks_by_ids(&ids, 16 * 1024 * 1024)?;
        let mut hits = Vec::new();
        for chunk in chunks {
            let Some((lexical, semantic)) = ranks.get(&chunk.chunk.id) else {
                continue;
            };
            let score = lexical.map_or(0.0, |(rank, _)| 1.0 / (60.0 + rank as f64))
                + semantic.map_or(0.0, |(rank, _)| 1.0 / (60.0 + rank as f64));
            hits.push(RetrievalHit {
                chunk_id: chunk.chunk.id,
                title: chunk.title,
                heading: chunk.chunk.heading,
                excerpt: chunk.chunk.markdown.chars().take(512).collect(),
                provenance: chunk.chunk.provenance,
                lexical_score: lexical.map(|(_, score)| score),
                vector_distance: semantic.map(|(_, distance)| distance),
                fusion_score: score,
            });
        }
        hits.sort_by(|left, right| {
            right
                .fusion_score
                .total_cmp(&left.fusion_score)
                .then_with(|| left.chunk_id.cmp(&right.chunk_id))
        });
        hits.truncate(limit);
        Ok(SearchResponse {
            hits,
            summary: RetrievalSummary {
                mode: RetrievalMode::Hybrid,
                fallback: None,
                diagnostic: None,
            },
        })
    }

    enum Semantic {
        Ready(Vec<(String, f64)>),
        Fallback(FallbackReason, Option<String>),
    }

    fn document_input(chunk: &IndexedChunk) -> Result<EmbeddingInput, EmbeddingError> {
        EmbeddingInput::document(chunk.chunk.markdown.clone(), Some(chunk.title.clone()))
    }

    fn input_hash(
        spec: &EmbeddingIndexSpec,
        input: &EmbeddingInput,
    ) -> Result<String, VectorError> {
        let mut hash = Sha256::new();
        hash.update(b"everything.wiki-vector-input.v1\0");
        for value in [
            spec.fingerprint().as_str().as_bytes(),
            input.text().as_bytes(),
            input.title().unwrap_or("").as_bytes(),
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

#[cfg(feature = "local-embeddings")]
pub use hybrid::{BuildBudget, BuildReport, BuildStop, DOCUMENT_SCHEMA, HybridSearch};
