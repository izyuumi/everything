use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::fs::{FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::context::{ContextBudget, ContextBuilder, MemoryExcerpt};
use crate::memory::consolidate::{ConsolidationConfig, ConsolidationError, Consolidator};
use crate::memory::search::{FallbackReason, RetrievalMode, RetrievalSummary};
use crate::memory::{Wiki, WikiError};
use crate::scheduler::TaskProviderFactory;
use crate::service::ServiceError;
use crate::service::state::StateDirectory;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("invalid host memory settings: {0}")]
    Invalid(&'static str),
    #[error("host memory requires an agent with a retained state lease")]
    UnleasedAgent,
    #[error("host memory is shutting down")]
    Draining,
    #[error("host memory operation cancelled")]
    Cancelled,
    #[error("host memory worker failed")]
    Worker,
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Wiki(#[from] WikiError),
    #[error(transparent)]
    Consolidation(ConsolidationError),
    #[error(transparent)]
    Filesystem(#[from] std::io::Error),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}

impl From<ConsolidationError> for MemoryError {
    fn from(error: ConsolidationError) -> Self {
        match error {
            ConsolidationError::Cancelled => Self::Cancelled,
            error => Self::Consolidation(error),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    pub retrieval_enabled: bool,
    pub max_results: usize,
    pub max_excerpt_bytes: usize,
    pub max_total_bytes: usize,
    pub maintenance: Option<MaintenanceModelSettings>,
    pub maintenance_context_tokens: u64,
    pub maintenance_timeout_ms: u64,
    pub embeddings: EmbeddingSettings,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            retrieval_enabled: true,
            max_results: 8,
            max_excerpt_bytes: 4096,
            max_total_bytes: 32 * 1024,
            maintenance: None,
            maintenance_context_tokens: 32_000,
            maintenance_timeout_ms: 120_000,
            embeddings: EmbeddingSettings::Disabled,
        }
    }
}

impl MemorySettings {
    pub fn validate(&self) -> Result<(), MemoryError> {
        if !(1..=16).contains(&self.max_results)
            || !(64..=16_384).contains(&self.max_excerpt_bytes)
            || !(1024..=256 * 1024).contains(&self.max_total_bytes)
            || !(1..=2_000_000).contains(&self.maintenance_context_tokens)
            || !(1..=300_000).contains(&self.maintenance_timeout_ms)
        {
            return Err(MemoryError::Invalid("retrieval or maintenance budget"));
        }
        if let Some(model) = &self.maintenance {
            let configuration = model.conversation_configuration();
            configuration
                .validate()
                .map_err(|_| MemoryError::Invalid("maintenance model configuration"))?;
            ContextBuilder::new(self.maintenance_budget(&configuration))
                .map_err(|_| MemoryError::Invalid("maintenance context budget"))?;
        }
        if let EmbeddingSettings::EmbeddingGemma2(configuration) = &self.embeddings {
            configuration.validate()?;
        }
        Ok(())
    }

    fn maintenance_budget(&self, configuration: &ConversationConfig) -> ContextBudget {
        ContextBudget {
            max_context_tokens: self.maintenance_context_tokens,
            reserved_output_tokens: configuration.max_output_tokens,
            safety_margin_tokens: 1024,
            max_request_bytes: 32 * 1024 * 1024,
            max_compaction_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceModelSettings {
    pub provider: ProviderKind,
    pub endpoint: ProviderEndpoint,
    pub model: String,
    pub max_output_tokens: u32,
}

impl MaintenanceModelSettings {
    fn conversation_configuration(&self) -> ConversationConfig {
        ConversationConfig {
            provider: self.provider,
            model: self.model.clone(),
            instructions:
                "Maintain the local Markdown wiki using the native cited consolidation protocol."
                    .into(),
            endpoint: self.endpoint.clone(),
            max_output_tokens: self.max_output_tokens,
            temperature: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingSettings {
    #[default]
    Disabled,
    EmbeddingGemma2(EmbeddingGemma2Settings),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingGemma2Settings {
    pub runtime_library: PathBuf,
    pub runtime_sha256: String,
    pub model_bundle: PathBuf,
    pub dimensions: usize,
    #[serde(default)]
    pub execution: EmbeddingExecutionSettings,
}

impl EmbeddingGemma2Settings {
    fn validate(&self) -> Result<(), MemoryError> {
        for path in [&self.runtime_library, &self.model_bundle] {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|value| value.len() > 4096 || value.chars().any(char::is_control))
            {
                return Err(MemoryError::Invalid("embedding artifact path"));
            }
        }
        if self.runtime_sha256.len() != 64
            || !self
                .runtime_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || ![128, 256, 512, 768].contains(&self.dimensions)
        {
            return Err(MemoryError::Invalid(
                "embedding runtime digest or dimension",
            ));
        }
        self.execution.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingExecutionSettings {
    pub threads: usize,
    pub max_tokens: usize,
    pub inference_timeout_ms: u64,
    pub load_timeout_ms: u64,
}

impl Default for EmbeddingExecutionSettings {
    fn default() -> Self {
        Self {
            threads: 2,
            max_tokens: 2048,
            inference_timeout_ms: 30_000,
            load_timeout_ms: 120_000,
        }
    }
}

impl EmbeddingExecutionSettings {
    fn validate(&self) -> Result<(), MemoryError> {
        if !(1..=16).contains(&self.threads)
            || !(8..=8192).contains(&self.max_tokens)
            || !(1..=300_000).contains(&self.inference_timeout_ms)
            || !(1..=300_000).contains(&self.load_timeout_ms)
        {
            return Err(MemoryError::Invalid("embedding execution budget"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDiagnostic {
    MaintenanceProviderUnavailable,
    EmbeddingFeatureUnavailable,
    EmbeddingTelemetrySuppressionRequired,
    EmbeddingInitializationFailed,
    MaintenanceWorkerFailed,
    QuiescenceFailed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MaintenanceStatus {
    Disabled,
    Enabled,
    Unavailable { diagnostic: MemoryDiagnostic },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EmbeddingStatus {
    Disabled,
    Enabled { dimensions: usize },
    Unavailable { diagnostic: MemoryDiagnostic },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemoryWorkerStatus {
    Running,
    Stopped {
        diagnostic: Option<MemoryDiagnostic>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub retrieval_enabled: bool,
    pub maintenance: MaintenanceStatus,
    pub embeddings: EmbeddingStatus,
    pub worker: MemoryWorkerStatus,
}

#[derive(Clone, Debug)]
pub struct RetrievedMemory {
    pub excerpts: Vec<MemoryExcerpt>,
    pub retrieval: RetrievalSummary,
    pub omitted_hits: usize,
}

#[derive(Clone)]
pub struct MemoryService {
    owner: Arc<MemoryOwner>,
}

struct MemoryOwner {
    core: Arc<MemoryCore>,
}

struct MemoryCore {
    settings: MemorySettings,
    consolidator: Consolidator,
    maintenance: MaintenanceStatus,
    embeddings: EmbeddingStatus,
    worker: watch::Receiver<MemoryWorkerStatus>,
    stop: CancellationToken,
    closing: AtomicBool,
    _agent: Agent,
}

impl Drop for MemoryOwner {
    fn drop(&mut self) {
        self.core.closing.store(true, Ordering::Release);
        self.core.stop.cancel();
    }
}

impl MemoryService {
    pub async fn open(
        agent: Agent,
        settings: MemorySettings,
        providers: Option<Arc<dyn TaskProviderFactory>>,
        cancellation: CancellationToken,
    ) -> Result<Self, MemoryError> {
        let initialization = cancellation.child_token();
        let guard = initialization.clone().drop_guard();
        let result = tokio::spawn(Self::open_owned(agent, settings, providers, initialization))
            .await
            .map_err(|_| MemoryError::Worker)?;
        if result.is_ok() {
            guard.disarm();
        }
        result
    }

    async fn open_owned(
        agent: Agent,
        settings: MemorySettings,
        providers: Option<Arc<dyn TaskProviderFactory>>,
        cancellation: CancellationToken,
    ) -> Result<Self, MemoryError> {
        settings.validate()?;
        if cancellation.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let prepared_agent = agent.clone();
        let paths = tokio::task::spawn_blocking(move || prepare_memory(&prepared_agent))
            .await
            .map_err(|_| MemoryError::Worker)??;
        if cancellation.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let (consolidator, maintenance) = match &settings.maintenance {
            Some(model) => {
                let configuration = model.conversation_configuration();
                let provider = match providers {
                    Some(providers) => {
                        let factory_cancellation = cancellation.child_token();
                        let _cancel_factory_on_drop = factory_cancellation.clone().drop_guard();
                        tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => return Err(MemoryError::Cancelled),
                            result = tokio::time::timeout(
                                Duration::from_secs(30),
                                providers.create(&configuration, factory_cancellation.clone()),
                            ) => result.ok().and_then(Result::ok),
                        }
                    }
                    None => None,
                };
                let budget = settings.maintenance_budget(&configuration);
                let provider = provider.filter(|provider| {
                    let capabilities = provider.capabilities();
                    capabilities
                        .max_context_tokens
                        .is_none_or(|limit| budget.max_context_tokens <= limit)
                        && capabilities
                            .max_output_tokens
                            .is_none_or(|limit| budget.reserved_output_tokens <= limit)
                });
                match provider {
                    Some(provider) => (
                        Consolidator::open(
                            paths.wiki,
                            &paths.journal,
                            Some(provider),
                            ConsolidationConfig {
                                model: configuration.model.clone(),
                                budget,
                                model_timeout: Duration::from_millis(
                                    settings.maintenance_timeout_ms,
                                ),
                            },
                        )
                        .await?,
                        MaintenanceStatus::Enabled,
                    ),
                    None => (
                        Consolidator::open_offline(paths.wiki, &paths.journal).await?,
                        MaintenanceStatus::Unavailable {
                            diagnostic: MemoryDiagnostic::MaintenanceProviderUnavailable,
                        },
                    ),
                }
            }
            None => (
                Consolidator::open_offline(paths.wiki, &paths.journal).await?,
                MaintenanceStatus::Disabled,
            ),
        };
        let embeddings = configure_embeddings(
            &agent,
            &consolidator,
            &settings.embeddings,
            paths.directory,
            cancellation.clone(),
        )
        .await?;
        if cancellation.is_cancelled() {
            consolidator.wait_idle().await?;
            return Err(MemoryError::Cancelled);
        }
        let (updates, worker) = watch::channel(MemoryWorkerStatus::Running);
        let core = Arc::new(MemoryCore {
            settings,
            consolidator,
            maintenance,
            embeddings,
            worker,
            stop: cancellation.child_token(),
            closing: AtomicBool::new(false),
            _agent: agent,
        });
        let supervisor = Arc::clone(&core);
        tokio::spawn(async move {
            let result = supervisor.consolidator.run(supervisor.stop.clone()).await;
            supervisor.closing.store(true, Ordering::Release);
            let diagnostic = if supervisor.consolidator.wait_idle().await.is_err() {
                Some(MemoryDiagnostic::QuiescenceFailed)
            } else if result.is_err() {
                Some(MemoryDiagnostic::MaintenanceWorkerFailed)
            } else {
                None
            };
            drop(supervisor);
            updates.send_replace(MemoryWorkerStatus::Stopped { diagnostic });
        });
        Ok(Self {
            owner: Arc::new(MemoryOwner { core }),
        })
    }

    pub fn settings(&self) -> &MemorySettings {
        &self.owner.core.settings
    }

    pub fn consolidator(&self) -> &Consolidator {
        &self.owner.core.consolidator
    }

    pub fn status(&self) -> MemoryStatus {
        let core = &self.owner.core;
        MemoryStatus {
            retrieval_enabled: core.settings.retrieval_enabled,
            maintenance: core.maintenance.clone(),
            embeddings: core.embeddings.clone(),
            worker: core.worker.borrow().clone(),
        }
    }

    pub async fn retrieve(
        &self,
        question: String,
        cancellation: CancellationToken,
    ) -> Result<RetrievedMemory, MemoryError> {
        self.ensure_accepting()?;
        if cancellation.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let core = &self.owner.core;
        if !core.settings.retrieval_enabled {
            return Ok(RetrievedMemory {
                excerpts: Vec::new(),
                retrieval: RetrievalSummary {
                    mode: RetrievalMode::Lexical,
                    fallback: Some(FallbackReason::Disabled),
                    diagnostic: None,
                },
                omitted_hits: 0,
            });
        }
        let response = core
            .consolidator
            .retrieve(question, core.settings.max_results, cancellation)
            .await?;
        let mut retrieval = response.summary;
        if matches!(core.embeddings, EmbeddingStatus::Unavailable { .. }) {
            retrieval.fallback = Some(FallbackReason::SemanticFailure);
            retrieval
                .diagnostic
                .get_or_insert_with(|| "configured local semantic retrieval is unavailable".into());
        }
        let mut excerpts = Vec::with_capacity(response.hits.len());
        let mut bytes = 2_usize;
        let mut omitted_hits = 0;
        for hit in response.hits {
            let mut excerpt = MemoryExcerpt {
                chunk_id: hit.chunk_id,
                title: hit.title,
                heading: hit.heading,
                text: hit.excerpt,
                provenance: hit.provenance,
            };
            truncate(&mut excerpt.text, core.settings.max_excerpt_bytes);
            let remaining = core.settings.max_total_bytes.saturating_sub(bytes);
            let raw_bytes = excerpt
                .chunk_id
                .len()
                .saturating_add(excerpt.title.len())
                .saturating_add(excerpt.heading.len())
                .saturating_add(excerpt.text.len())
                .saturating_add(excerpt.provenance.page_path.len());
            if raw_bytes > remaining {
                omitted_hits += 1;
                continue;
            }
            let encoded = serde_json::to_vec(&ExcerptBoundary::from(&excerpt))?;
            let encoded_bytes = encoded.len() + usize::from(!excerpts.is_empty());
            if encoded_bytes > remaining {
                omitted_hits += 1;
                continue;
            }
            bytes += encoded_bytes;
            excerpts.push(excerpt);
        }
        Ok(RetrievedMemory {
            excerpts,
            retrieval,
            omitted_hits,
        })
    }

    #[cfg(feature = "local-embeddings")]
    pub async fn semantic_status(
        &self,
    ) -> Result<Option<crate::memory::vector::VectorStatus>, MemoryError> {
        self.ensure_accepting()?;
        Ok(self.consolidator().semantic_status().await?)
    }

    #[cfg(feature = "local-embeddings")]
    pub async fn rebuild_semantic(
        &self,
        budget: crate::memory::search::BuildBudget,
        fresh: bool,
        cancellation: CancellationToken,
    ) -> Result<crate::memory::search::BuildReport, MemoryError> {
        self.ensure_accepting()?;
        Ok(self
            .consolidator()
            .rebuild_semantic(budget, fresh, cancellation)
            .await?)
    }

    pub async fn shutdown(&self) -> Result<(), MemoryError> {
        let core = &self.owner.core;
        core.closing.store(true, Ordering::Release);
        core.stop.cancel();
        let mut worker = core.worker.clone();
        loop {
            match worker.borrow().clone() {
                MemoryWorkerStatus::Running => {}
                MemoryWorkerStatus::Stopped { diagnostic: None } => return Ok(()),
                MemoryWorkerStatus::Stopped { .. } => return Err(MemoryError::Worker),
            }
            worker.changed().await.map_err(|_| MemoryError::Worker)?;
        }
    }

    fn ensure_accepting(&self) -> Result<(), MemoryError> {
        if self.owner.core.closing.load(Ordering::Acquire) {
            return Err(MemoryError::Draining);
        }
        Ok(())
    }
}

struct PreparedMemory {
    wiki: Wiki,
    journal: PathBuf,
    directory: PathBuf,
}

fn prepare_memory(agent: &Agent) -> Result<PreparedMemory, MemoryError> {
    let state = agent.state_directory().ok_or(MemoryError::UnleasedAgent)?;
    let directory = StateDirectory::open(&state.join("memory"), true)?;
    let wiki = StateDirectory::open(&directory.path.join("wiki"), true)?;
    prepare_database(&directory, "lexical.sqlite3")?;
    prepare_database(&directory, "maintenance.sqlite3")?;
    let journal = directory.path.join("maintenance.sqlite3");
    let wiki = Wiki::open(&wiki.path, directory.path.join("lexical.sqlite3"))?;
    Ok(PreparedMemory {
        wiki,
        journal,
        directory: directory.path,
    })
}

fn prepare_database(directory: &StateDirectory, name: &str) -> Result<(), MemoryError> {
    let descriptor = rustix::fs::openat(
        &directory.file,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?;
    let metadata = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != directory.uid
        || metadata.st_mode & 0o7777 != 0o600
        || metadata.st_nlink != 1
    {
        return Err(MemoryError::Invalid(
            "memory database file ownership or permissions",
        ));
    }
    Ok(())
}

async fn configure_embeddings(
    agent: &Agent,
    consolidator: &Consolidator,
    settings: &EmbeddingSettings,
    directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<EmbeddingStatus, MemoryError> {
    let EmbeddingSettings::EmbeddingGemma2(configuration) = settings else {
        return Ok(EmbeddingStatus::Disabled);
    };
    #[cfg(not(feature = "local-embeddings"))]
    {
        let _ = (agent, consolidator, configuration, directory, cancellation);
        Ok(EmbeddingStatus::Unavailable {
            diagnostic: MemoryDiagnostic::EmbeddingFeatureUnavailable,
        })
    }
    #[cfg(feature = "local-embeddings")]
    {
        if let Err(error) = crate::memory::embedding::embeddinggemma2::EmbeddingRuntime::require_telemetry_suppression() {
            consolidator.set_hybrid_search_result(Err(error.into())).await?;
            return Ok(EmbeddingStatus::Unavailable {
                diagnostic: MemoryDiagnostic::EmbeddingTelemetrySuppressionRequired,
            });
        }
        let configuration = configuration.clone();
        let dimensions = configuration.dimensions;
        let retained_agent = agent.clone();
        let construction = tokio::task::spawn_blocking(move || {
            let _retained_agent = retained_agent;
            build_embeddings(configuration, directory, cancellation)
        })
        .await
        .map_err(|_| MemoryError::Worker)?;
        let available = construction.is_ok();
        consolidator.set_hybrid_search_result(construction).await?;
        Ok(if available {
            EmbeddingStatus::Enabled { dimensions }
        } else {
            EmbeddingStatus::Unavailable {
                diagnostic: MemoryDiagnostic::EmbeddingInitializationFailed,
            }
        })
    }
}

#[cfg(feature = "local-embeddings")]
fn build_embeddings(
    configuration: EmbeddingGemma2Settings,
    directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<crate::memory::search::HybridSearch, crate::memory::vector::VectorError> {
    use std::io::Read;

    use sha2::{Digest, Sha256};

    use crate::memory::embedding::EmbeddingError;
    use crate::memory::embedding::embeddinggemma2::{
        EmbeddingGemma2, EmbeddingGemma2Options, EmbeddingRuntime,
    };
    use crate::memory::search::HybridSearch;
    use crate::memory::vector::{VectorError, VectorIndex, VectorLimits};

    if cancellation.is_cancelled() {
        return Err(VectorError::Cancelled);
    }
    EmbeddingRuntime::require_telemetry_suppression()?;
    let descriptor = rustix::fs::open(
        &configuration.runtime_library,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| VectorError::Invalid("verified embedding runtime is unavailable"))?;
    let library = std::fs::File::from(descriptor);
    let metadata = library
        .metadata()
        .map_err(|_| VectorError::Invalid("verified embedding runtime metadata is unavailable"))?;
    if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
        return Err(VectorError::Invalid(
            "verified embedding runtime size or file type",
        ));
    }
    let mut reader = library.take(256 * 1024 * 1024 + 1);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_cancelled() {
            return Err(VectorError::Cancelled);
        }
        let bytes = reader
            .read(&mut buffer)
            .map_err(|_| VectorError::Invalid("verified embedding runtime could not be read"))?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
    }
    if !format!("{:x}", digest.finalize()).eq_ignore_ascii_case(&configuration.runtime_sha256) {
        return Err(VectorError::Invalid(
            "verified embedding runtime digest does not match",
        ));
    }
    let runtime = EmbeddingRuntime::initialize(&configuration.runtime_library)?;
    if !runtime
        .library_sha256()
        .eq_ignore_ascii_case(&configuration.runtime_sha256)
    {
        return Err(VectorError::Invalid(
            "verified embedding runtime changed during initialization",
        ));
    }
    let model = EmbeddingGemma2::load(
        runtime,
        &configuration.model_bundle,
        EmbeddingGemma2Options {
            threads: configuration.execution.threads,
            max_tokens: configuration.execution.max_tokens,
            inference_timeout: Duration::from_millis(configuration.execution.inference_timeout_ms),
            load_timeout: Duration::from_millis(configuration.execution.load_timeout_ms),
        },
        &cancellation,
    )?
    .with_dimensions(configuration.dimensions)?;
    if cancellation.is_cancelled() {
        return Err(VectorError::Embedding(EmbeddingError::Cancelled));
    }
    let directory = StateDirectory::open(&directory, false)
        .map_err(|_| VectorError::Invalid("private semantic index directory is unavailable"))?;
    prepare_database(&directory, "vectors.sqlite3")
        .map_err(|_| VectorError::Invalid("private semantic index file is unavailable"))?;
    let index = VectorIndex::open(
        directory.path.join("vectors.sqlite3"),
        VectorLimits::default(),
    )?;
    HybridSearch::new(index, Arc::new(model))
}

fn truncate(value: &mut String, maximum: usize) {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[derive(Serialize)]
struct ExcerptBoundary<'a> {
    chunk_id: &'a str,
    title: &'a str,
    heading: &'a str,
    text: &'a str,
    page_path: &'a str,
    start_line: usize,
    end_line: usize,
}

impl<'a> From<&'a MemoryExcerpt> for ExcerptBoundary<'a> {
    fn from(excerpt: &'a MemoryExcerpt) -> Self {
        Self {
            chunk_id: &excerpt.chunk_id,
            title: &excerpt.title,
            heading: &excerpt.heading,
            text: &excerpt.text,
            page_path: &excerpt.provenance.page_path,
            start_line: excerpt.provenance.start_line,
            end_line: excerpt.provenance.end_line,
        }
    }
}
