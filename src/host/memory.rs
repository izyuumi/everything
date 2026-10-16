use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::fs::{FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::context::{ContextBudget, ContextBuilder, MemoryExcerpt};
pub use crate::memory::consolidate::SemanticGeneration;
use crate::memory::consolidate::{ConsolidationConfig, ConsolidationError, Consolidator};
use crate::memory::embedding::EmbeddingProfile;
use crate::memory::search::{FallbackReason, RetrievalMode, RetrievalSummary};
use crate::memory::{Wiki, WikiError};
use crate::protocol::CommandId;
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
    #[error("embedding application binding changed")]
    BindingChanged,
    #[error("host memory operation cancelled")]
    Cancelled,
    #[error("host memory worker failed")]
    Worker,
    #[error("semantic backend detachment and shutdown both failed")]
    BackendCleanup {
        detachment: ConsolidationError,
        #[source]
        shutdown: Box<MemoryError>,
    },
    #[error("semantic operation and owned cleanup both failed")]
    EmbeddingCleanup {
        operation: Box<MemoryError>,
        #[source]
        cleanup: Box<MemoryError>,
    },
    #[cfg(feature = "local-embeddings")]
    #[error("isolated embedding helper shutdown failed")]
    EmbeddingShutdown(#[source] Box<crate::memory::embedding::EmbeddingError>),
    #[cfg(feature = "local-embeddings")]
    #[error("isolated embedding helper cleanup worker failed")]
    EmbeddingShutdownWorker(#[source] Box<tokio::task::JoinError>),
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingSettings {
    #[default]
    Disabled,
    EmbeddingGemma2(EmbeddingGemma2Settings),
}

impl EmbeddingSettings {
    pub fn backend_default_dimensions(&self) -> Option<usize> {
        match self {
            Self::Disabled => None,
            Self::EmbeddingGemma2(_) => {
                #[cfg(feature = "local-embeddings")]
                {
                    Some(crate::memory::embedding::embeddinggemma2::DEFAULT_DIMENSIONS)
                }
                #[cfg(not(feature = "local-embeddings"))]
                {
                    None
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    EmbeddingIsolationUnavailable,
    EmbeddingProcessUnavailable,
    EmbeddingInitializationFailed,
    MaintenanceWorkerFailed,
    QuiescenceFailed,
    EmbeddingApplyFailed,
    EmbeddingConfigurationUnavailable,
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

#[derive(Clone, Copy, Debug, Serialize)]
pub enum EmbeddingDimensionChoice {
    Inherit,
    BackendDefault,
    Selected { dimensions: u32 },
}

#[derive(Clone, Debug, Serialize)]
pub struct EmbeddingPreferences {
    pub configured: EmbeddingSettings,
    pub enabled: bool,
    pub dimensions: EmbeddingDimensionChoice,
}

impl EmbeddingPreferences {
    pub fn inherited(configured: EmbeddingSettings) -> Self {
        let enabled = !matches!(configured, EmbeddingSettings::Disabled);
        Self {
            configured,
            enabled,
            dimensions: EmbeddingDimensionChoice::Inherit,
        }
    }

    fn resolved(&self) -> Result<EmbeddingSettings, MemoryError> {
        if let EmbeddingDimensionChoice::Selected { dimensions } = self.dimensions
            && !(1..=65_536).contains(&dimensions)
        {
            return Err(MemoryError::Invalid("embedding dimensions"));
        }
        let mut settings = self.configured.clone();
        if let EmbeddingSettings::EmbeddingGemma2(configuration) = &mut settings {
            configuration.validate()?;
            match self.dimensions {
                EmbeddingDimensionChoice::Inherit => {}
                EmbeddingDimensionChoice::BackendDefault => {
                    if let Some(dimensions) = self.configured.backend_default_dimensions() {
                        configuration.dimensions = dimensions;
                    }
                }
                EmbeddingDimensionChoice::Selected { dimensions } => {
                    configuration.dimensions = dimensions as usize;
                }
            }
            configuration.validate()?;
        }
        if !self.enabled {
            return Ok(EmbeddingSettings::Disabled);
        }
        Ok(settings)
    }
}

#[derive(Clone, Debug)]
pub struct EmbeddingApplyRequest {
    pub operation_id: CommandId,
    pub settings_revision: u64,
    pub request_sha256: String,
    pub preferences: EmbeddingPreferences,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingApplyStage {
    Capacity,
    Detaching,
    Loading,
    Rebuilding,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingBuildStop {
    ChunkBudget,
    TimeBudget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingBuildProgress {
    pub vector_generation: i64,
    pub vectors: usize,
    pub embedded: usize,
    pub reused: usize,
    pub skipped_blank: usize,
    pub stop: EmbeddingBuildStop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EmbeddingApplyPhase {
    Pending {
        stage: EmbeddingApplyStage,
    },
    Disabled,
    Ready {
        dimensions: usize,
        vector_generation: i64,
        reused: bool,
    },
    Unavailable {
        diagnostic: MemoryDiagnostic,
    },
    Failed {
        diagnostic: MemoryDiagnostic,
    },
    Superseded,
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingApplySnapshot {
    pub operation_id: CommandId,
    pub settings_revision: u64,
    pub generation: u64,
    pub phase: EmbeddingApplyPhase,
    pub progress: Option<EmbeddingBuildProgress>,
}

#[derive(Clone, Debug)]
pub struct EmbeddingMetadata {
    pub generation: u64,
    pub accepting_semantics: bool,
    pub application_active: bool,
    pub status: EmbeddingStatus,
    pub profile: Option<EmbeddingProfile>,
    pub application: Option<EmbeddingApplySnapshot>,
}

#[derive(Clone, Debug)]
pub enum EmbeddingApplyAdmission {
    Started(EmbeddingApplySnapshot),
    Observed(EmbeddingApplySnapshot),
    Deferred(EmbeddingApplySnapshot),
}

struct EmbeddingApplication {
    binding: Vec<u8>,
    snapshot: EmbeddingApplySnapshot,
    installed: bool,
}

struct EmbeddingControl {
    status: EmbeddingStatus,
    profile: Option<EmbeddingProfile>,
    application: Option<EmbeddingApplication>,
    active: Option<u64>,
    #[cfg(feature = "local-embeddings")]
    process: Option<Arc<crate::memory::embedding::isolated::IsolatedEmbeddingGemma2>>,
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
    embeddings: Mutex<EmbeddingControl>,
    embedding_changes: Arc<Notify>,
    #[cfg(feature = "local-embeddings")]
    directory: PathBuf,
    quiescence_failed: AtomicBool,
    worker: watch::Receiver<MemoryWorkerStatus>,
    stop: CancellationToken,
    closing: AtomicBool,
    _agent: Agent,
}

impl Drop for MemoryOwner {
    fn drop(&mut self) {
        self.core.closing.store(true, Ordering::Release);
        if self.core.consolidator.close_semantics().is_err() {
            self.core.quiescence_failed.store(true, Ordering::Release);
        }
        self.core.stop.cancel();
        self.core.embedding_changes.notify_waiters();
    }
}

impl MemoryService {
    pub async fn open(
        agent: Agent,
        settings: MemorySettings,
        providers: Option<Arc<dyn TaskProviderFactory>>,
        cancellation: CancellationToken,
    ) -> Result<Self, MemoryError> {
        let preferences = EmbeddingPreferences::inherited(settings.embeddings.clone());
        Self::open_with_embedding_preferences(agent, settings, providers, cancellation, preferences)
            .await
    }

    pub async fn open_with_embedding_preferences(
        agent: Agent,
        settings: MemorySettings,
        providers: Option<Arc<dyn TaskProviderFactory>>,
        cancellation: CancellationToken,
        preferences: EmbeddingPreferences,
    ) -> Result<Self, MemoryError> {
        if serde_json::to_vec(&settings.embeddings)? != serde_json::to_vec(&preferences.configured)?
        {
            return Err(MemoryError::BindingChanged);
        }
        let effective = preferences.resolved()?;
        let initialization = cancellation.child_token();
        let guard = initialization.clone().drop_guard();
        let result = tokio::spawn(Self::open_owned(
            agent,
            settings,
            effective,
            providers,
            initialization,
        ))
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
        effective: EmbeddingSettings,
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
            &effective,
            paths.directory.clone(),
            cancellation.clone(),
        )
        .await?;
        if cancellation.is_cancelled() {
            let idle = consolidator.wait_idle().await;
            #[cfg(feature = "local-embeddings")]
            stop_embeddings(embeddings.process, agent.clone()).await?;
            idle?;
            return Err(MemoryError::Cancelled);
        }
        let (updates, worker) = watch::channel(MemoryWorkerStatus::Running);
        let core = Arc::new(MemoryCore {
            settings,
            consolidator,
            maintenance,
            embeddings: Mutex::new(EmbeddingControl {
                status: embeddings.status,
                profile: embeddings.profile,
                application: None,
                active: None,
                #[cfg(feature = "local-embeddings")]
                process: embeddings.process,
            }),
            embedding_changes: Arc::new(Notify::new()),
            #[cfg(feature = "local-embeddings")]
            directory: paths.directory,
            quiescence_failed: AtomicBool::new(false),
            worker,
            stop: cancellation.child_token(),
            closing: AtomicBool::new(false),
            _agent: agent,
        });
        let supervisor = Arc::clone(&core);
        tokio::spawn(async move {
            let result = supervisor.consolidator.run(supervisor.stop.clone()).await;
            supervisor.closing.store(true, Ordering::Release);
            let gate_failed = supervisor.consolidator.close_semantics().is_err();
            supervisor.stop.cancel();
            loop {
                let changed = supervisor.embedding_changes.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let active = match supervisor.embeddings.lock() {
                    Ok(state) => state.active.is_some(),
                    Err(poisoned) => {
                        supervisor.quiescence_failed.store(true, Ordering::Release);
                        poisoned.into_inner().active.is_some()
                    }
                };
                if !active {
                    break;
                }
                changed.await;
            }
            let idle_failed = supervisor.consolidator.wait_idle().await.is_err();
            #[cfg(feature = "local-embeddings")]
            let isolation_failed = shutdown_backend(&supervisor).await.is_err();
            #[cfg(not(feature = "local-embeddings"))]
            let isolation_failed = false;
            let isolation_failed = isolation_failed
                || gate_failed
                || supervisor.quiescence_failed.load(Ordering::Acquire);
            if let Ok(mut state) = supervisor.embeddings.lock() {
                state.status = EmbeddingStatus::Disabled;
                if let Some(application) = &mut state.application
                    && matches!(
                        application.snapshot.phase,
                        EmbeddingApplyPhase::Pending { .. }
                    )
                {
                    application.snapshot.phase = EmbeddingApplyPhase::Stopped;
                }
            }
            supervisor.embedding_changes.notify_waiters();
            let diagnostic = if idle_failed || isolation_failed {
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
        let embeddings = self
            .embedding_metadata()
            .map(|metadata| metadata.status)
            .unwrap_or(EmbeddingStatus::Unavailable {
                diagnostic: MemoryDiagnostic::QuiescenceFailed,
            });
        MemoryStatus {
            retrieval_enabled: core.settings.retrieval_enabled,
            maintenance: core.maintenance.clone(),
            embeddings,
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
        if matches!(
            self.status().embeddings,
            EmbeddingStatus::Unavailable { .. }
        ) && retrieval.mode == RetrievalMode::Lexical
        {
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

    pub fn suspend_semantics(
        &self,
        operation_id: CommandId,
    ) -> Result<SemanticGeneration, MemoryError> {
        self.ensure_accepting()?;
        let generation = self.consolidator().suspend_semantics(operation_id)?;
        self.owner.core.embedding_changes.notify_waiters();
        Ok(generation)
    }

    pub fn restore_semantics(&self, generation: &SemanticGeneration) -> Result<(), MemoryError> {
        self.ensure_accepting()?;
        let core = &self.owner.core;
        let control = core.embeddings.lock().map_err(|_| MemoryError::Worker)?;
        if control
            .application
            .as_ref()
            .is_some_and(|application| application.snapshot.generation == generation.generation())
            || control.active == Some(generation.generation())
        {
            return Err(MemoryError::BindingChanged);
        }
        if core.closing.load(Ordering::Acquire) {
            return Err(MemoryError::Draining);
        }
        core.consolidator.restore_semantics(generation)?;
        drop(control);
        core.embedding_changes.notify_waiters();
        Ok(())
    }

    pub fn embedding_metadata(&self) -> Result<EmbeddingMetadata, MemoryError> {
        let core = &self.owner.core;
        let control = core.embeddings.lock().map_err(|_| MemoryError::Worker)?;
        let (generation, accepting_semantics) = core.consolidator.semantic_admission()?;
        let status = control.status.clone();
        #[cfg(feature = "local-embeddings")]
        let status = if matches!(status, EmbeddingStatus::Enabled { .. })
            && control
                .process
                .as_ref()
                .is_some_and(|process| !process.is_available())
        {
            EmbeddingStatus::Unavailable {
                diagnostic: MemoryDiagnostic::EmbeddingProcessUnavailable,
            }
        } else {
            status
        };
        let application = control.application.as_ref().map(|application| {
            let mut snapshot = application.snapshot.clone();
            if snapshot.generation != generation {
                snapshot.phase = EmbeddingApplyPhase::Superseded;
            }
            snapshot
        });
        Ok(EmbeddingMetadata {
            generation,
            accepting_semantics,
            application_active: control.active.is_some(),
            status,
            profile: control.profile.clone(),
            application,
        })
    }

    pub fn embedding_changes(&self) -> Arc<Notify> {
        Arc::clone(&self.owner.core.embedding_changes)
    }

    pub async fn apply_embeddings(
        &self,
        generation: SemanticGeneration,
        request: EmbeddingApplyRequest,
    ) -> Result<EmbeddingApplyAdmission, MemoryError> {
        self.ensure_accepting()?;
        if request.settings_revision == 0
            || request.settings_revision > i64::MAX as u64
            || request.request_sha256.len() != 64
            || !request
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(MemoryError::Invalid("embedding application identity"));
        }
        request.preferences.resolved()?;
        let binding = serde_json::to_vec(&(
            request.operation_id,
            request.settings_revision,
            &request.request_sha256,
            &request.preferences,
        ))?;
        if binding.len() > 32 * 1024 {
            return Err(MemoryError::Invalid("embedding application capture"));
        }
        let core = Arc::clone(&self.owner.core);
        core.consolidator
            .validate_semantics(&generation, request.operation_id)?;
        let mut control = core.embeddings.lock().map_err(|_| MemoryError::Worker)?;
        core.consolidator
            .validate_semantics(&generation, request.operation_id)?;
        if core.closing.load(Ordering::Acquire) {
            return Err(MemoryError::Draining);
        }
        if let Some(application) = &control.application
            && application.snapshot.generation == generation.generation()
        {
            if application.binding != binding {
                return Err(MemoryError::BindingChanged);
            }
            if !matches!(
                application.snapshot.phase,
                EmbeddingApplyPhase::Pending { .. }
            ) || control.active == Some(generation.generation())
            {
                return Ok(EmbeddingApplyAdmission::Observed(
                    application.snapshot.clone(),
                ));
            }
        } else {
            control.application = Some(EmbeddingApplication {
                binding,
                snapshot: EmbeddingApplySnapshot {
                    operation_id: request.operation_id,
                    settings_revision: request.settings_revision,
                    generation: generation.generation(),
                    phase: EmbeddingApplyPhase::Pending {
                        stage: EmbeddingApplyStage::Capacity,
                    },
                    progress: None,
                },
                installed: false,
            });
        }
        if control.active.is_some() {
            let application = control.application.as_ref().ok_or(MemoryError::Worker)?;
            return Ok(EmbeddingApplyAdmission::Deferred(
                application.snapshot.clone(),
            ));
        }
        let application = control.application.as_mut().ok_or(MemoryError::Worker)?;
        application.snapshot.phase = EmbeddingApplyPhase::Pending {
            stage: if application.installed {
                EmbeddingApplyStage::Rebuilding
            } else {
                EmbeddingApplyStage::Detaching
            },
        };
        let snapshot = application.snapshot.clone();
        control.active = Some(generation.generation());
        drop(control);
        let worker = EmbeddingApplyOwner {
            core: Arc::clone(&core),
            generation: generation.generation(),
            settled: false,
        };
        tokio::spawn(async move {
            let mut worker = worker;
            let result = apply_embedding_step(&core, &generation, &request).await;
            if let Err(error) = result {
                let phase = if core.closing.load(Ordering::Acquire) {
                    EmbeddingApplyPhase::Stopped
                } else if core
                    .consolidator
                    .validate_semantics(&generation, request.operation_id)
                    .is_err()
                {
                    EmbeddingApplyPhase::Superseded
                } else {
                    EmbeddingApplyPhase::Failed {
                        diagnostic: MemoryDiagnostic::EmbeddingApplyFailed,
                    }
                };
                if set_application_phase(&core, &generation, phase, None).is_err() {
                    core.quiescence_failed.store(true, Ordering::Release);
                    core.closing.store(true, Ordering::Release);
                    core.stop.cancel();
                }
                drop(error);
            }
            worker.settled = true;
        });
        Ok(EmbeddingApplyAdmission::Started(snapshot))
    }

    pub async fn shutdown(&self) -> Result<(), MemoryError> {
        let core = &self.owner.core;
        core.closing.store(true, Ordering::Release);
        let gate = core.consolidator.close_semantics();
        core.stop.cancel();
        core.embedding_changes.notify_waiters();
        gate?;
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

struct EmbeddingApplyOwner {
    core: Arc<MemoryCore>,
    generation: u64,
    settled: bool,
}

impl Drop for EmbeddingApplyOwner {
    fn drop(&mut self) {
        match self.core.embeddings.lock() {
            Ok(mut state) => {
                if state.active == Some(self.generation) {
                    state.active = None;
                }
                if !self.settled
                    && let Some(application) = &mut state.application
                    && application.snapshot.generation == self.generation
                {
                    application.snapshot.phase = EmbeddingApplyPhase::Failed {
                        diagnostic: MemoryDiagnostic::EmbeddingApplyFailed,
                    };
                }
            }
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                if state.active == Some(self.generation) {
                    state.active = None;
                }
                self.core.quiescence_failed.store(true, Ordering::Release);
                self.core.closing.store(true, Ordering::Release);
                self.core.stop.cancel();
            }
        }
        self.core.embedding_changes.notify_waiters();
    }
}

fn set_application_phase(
    core: &MemoryCore,
    generation: &SemanticGeneration,
    phase: EmbeddingApplyPhase,
    progress: Option<EmbeddingBuildProgress>,
) -> Result<(), MemoryError> {
    let mut state = core.embeddings.lock().map_err(|_| MemoryError::Worker)?;
    if let Some(application) = &mut state.application
        && application.snapshot.generation == generation.generation()
    {
        application.snapshot.phase = phase;
        application.snapshot.progress = progress;
    }
    drop(state);
    core.embedding_changes.notify_waiters();
    Ok(())
}

fn ensure_application(
    core: &MemoryCore,
    generation: &SemanticGeneration,
    request: &EmbeddingApplyRequest,
) -> Result<(), MemoryError> {
    if core.closing.load(Ordering::Acquire) || core.stop.is_cancelled() {
        return Err(MemoryError::Draining);
    }
    core.consolidator
        .validate_semantics(generation, request.operation_id)
        .map_err(MemoryError::from)
}

async fn apply_embedding_step(
    core: &Arc<MemoryCore>,
    generation: &SemanticGeneration,
    request: &EmbeddingApplyRequest,
) -> Result<(), MemoryError> {
    ensure_application(core, generation, request)?;
    let installed = core
        .embeddings
        .lock()
        .map_err(|_| MemoryError::Worker)?
        .application
        .as_ref()
        .filter(|application| application.snapshot.generation == generation.generation())
        .is_some_and(|application| application.installed);
    if !installed {
        set_application_phase(
            core,
            generation,
            EmbeddingApplyPhase::Pending {
                stage: EmbeddingApplyStage::Detaching,
            },
            None,
        )?;
        #[cfg(feature = "local-embeddings")]
        {
            let hybrid = core.consolidator.detach_semantics(generation).await?;
            let process = core
                .embeddings
                .lock()
                .map_err(|_| MemoryError::Worker)?
                .process
                .take();
            if let Err(error) = stop_backend(hybrid, process, core._agent.clone()).await {
                latch_failed_cleanup(core);
                return Err(error);
            }
        }
        ensure_application(core, generation, request)?;
        let settings = request.preferences.resolved()?;
        if !request.preferences.enabled {
            core.embeddings
                .lock()
                .map_err(|_| MemoryError::Worker)?
                .status = EmbeddingStatus::Disabled;
            return set_application_phase(core, generation, EmbeddingApplyPhase::Disabled, None);
        }
        if matches!(settings, EmbeddingSettings::Disabled) {
            return unavailable_application(
                core,
                generation,
                MemoryDiagnostic::EmbeddingConfigurationUnavailable,
            );
        }
        #[cfg(not(feature = "local-embeddings"))]
        {
            return unavailable_application(
                core,
                generation,
                MemoryDiagnostic::EmbeddingFeatureUnavailable,
            );
        }
        #[cfg(feature = "local-embeddings")]
        {
            let EmbeddingSettings::EmbeddingGemma2(configuration) = settings else {
                return Err(MemoryError::Invalid("embedding application backend"));
            };
            set_application_phase(
                core,
                generation,
                EmbeddingApplyPhase::Pending {
                    stage: EmbeddingApplyStage::Loading,
                },
                None,
            )?;
            ensure_application(core, generation, request)?;
            let owned = (
                configuration,
                core.directory.clone(),
                generation.cancellation(),
                Arc::clone(core),
            );
            let construction = tokio::task::spawn_blocking(move || {
                let (configuration, directory, cancellation, core) = owned;
                build_embeddings(configuration, directory, cancellation, core._agent.clone())
            })
            .await
            .map_err(|_| MemoryError::Worker)?;
            let mut built = match construction {
                Ok(built) => built,
                Err(error) => {
                    ensure_application(core, generation, request)?;
                    let diagnostic = match &error {
                        crate::memory::vector::VectorError::Embedding(error)
                            if crate::memory::embedding::isolated::is_unavailable(error) =>
                        {
                            MemoryDiagnostic::EmbeddingIsolationUnavailable
                        }
                        _ => MemoryDiagnostic::EmbeddingInitializationFailed,
                    };
                    return unavailable_application(core, generation, diagnostic);
                }
            };
            if let Err(error) = ensure_application(core, generation, request) {
                return Err(dispose_after_error(core, built, error).await);
            }
            let Some(hybrid) = built.hybrid.take() else {
                return Err(dispose_after_error(core, built, MemoryError::Worker).await);
            };
            let profile = hybrid.specification().profile().clone();
            let installation = core
                .consolidator
                .install_semantics(generation, hybrid)
                .await;
            match installation {
                Ok(Ok(())) => {}
                Ok(Err(hybrid)) => {
                    built.hybrid = Some(hybrid);
                    return Err(
                        dispose_after_error(core, built, MemoryError::BindingChanged).await,
                    );
                }
                Err(error) => {
                    return Err(dispose_after_error(core, built, error.into()).await);
                }
            }
            {
                let mut state = core.embeddings.lock().map_err(|_| MemoryError::Worker)?;
                state.profile = Some(profile.clone());
                state.status = EmbeddingStatus::Enabled {
                    dimensions: profile.dimensions(),
                };
                state.process = Some(Arc::clone(&built.process));
                if let Some(application) = &mut state.application
                    && application.snapshot.generation == generation.generation()
                {
                    application.installed = true;
                }
            }
        }
    }
    #[cfg(feature = "local-embeddings")]
    {
        ensure_application(core, generation, request)?;
        set_application_phase(
            core,
            generation,
            EmbeddingApplyPhase::Pending {
                stage: EmbeddingApplyStage::Rebuilding,
            },
            None,
        )?;
        let previous = core.consolidator.semantic_status().await?;
        let report = core
            .consolidator
            .rebuild_semantics_generation(generation, crate::memory::search::BuildBudget::default())
            .await?;
        ensure_application(core, generation, request)?;
        match report.stop {
            crate::memory::search::BuildStop::Complete => {
                let status = core.consolidator.semantic_status().await?;
                let active = status
                    .and_then(|status| status.active)
                    .filter(|active| {
                        active.ready
                            && active.id == report.generation.id
                            && active.spec == report.generation.spec
                    })
                    .ok_or(MemoryError::Worker)?;
                core.consolidator.activate_semantics(generation)?;
                set_application_phase(
                    core,
                    generation,
                    EmbeddingApplyPhase::Ready {
                        dimensions: active.spec.profile().dimensions(),
                        vector_generation: active.id,
                        reused: previous
                            .and_then(|status| status.active)
                            .is_some_and(|previous| previous.ready && previous.id == active.id),
                    },
                    None,
                )
            }
            stop => {
                let stop = match stop {
                    crate::memory::search::BuildStop::ChunkBudget => {
                        EmbeddingBuildStop::ChunkBudget
                    }
                    crate::memory::search::BuildStop::TimeBudget => EmbeddingBuildStop::TimeBudget,
                    crate::memory::search::BuildStop::Complete => return Err(MemoryError::Worker),
                };
                set_application_phase(
                    core,
                    generation,
                    EmbeddingApplyPhase::Pending {
                        stage: EmbeddingApplyStage::Rebuilding,
                    },
                    Some(EmbeddingBuildProgress {
                        vector_generation: report.generation.id,
                        vectors: report.generation.vectors,
                        embedded: report.generation.embedded,
                        reused: report.generation.reused,
                        skipped_blank: report.generation.skipped_blank,
                        stop,
                    }),
                )
            }
        }
    }
    #[cfg(not(feature = "local-embeddings"))]
    {
        unavailable_application(
            core,
            generation,
            MemoryDiagnostic::EmbeddingFeatureUnavailable,
        )
    }
}

fn unavailable_application(
    core: &MemoryCore,
    generation: &SemanticGeneration,
    diagnostic: MemoryDiagnostic,
) -> Result<(), MemoryError> {
    core.embeddings
        .lock()
        .map_err(|_| MemoryError::Worker)?
        .status = EmbeddingStatus::Unavailable { diagnostic };
    set_application_phase(
        core,
        generation,
        EmbeddingApplyPhase::Unavailable { diagnostic },
        None,
    )
}

#[cfg(feature = "local-embeddings")]
async fn stop_backend(
    hybrid: Option<crate::memory::search::HybridSearch>,
    process: Option<Arc<crate::memory::embedding::isolated::IsolatedEmbeddingGemma2>>,
    agent: Agent,
) -> Result<(), MemoryError> {
    let owned = (hybrid, process, agent);
    tokio::task::spawn_blocking(move || {
        let owned = owned;
        let result = match &owned.1 {
            Some(process) => process
                .shutdown()
                .map_err(|error| MemoryError::EmbeddingShutdown(Box::new(error))),
            None => Ok(()),
        };
        drop(owned);
        result
    })
    .await
    .map_err(|error| MemoryError::EmbeddingShutdownWorker(Box::new(error)))?
}

#[cfg(feature = "local-embeddings")]
async fn dispose_built(built: BuiltEmbeddings) -> Result<(), MemoryError> {
    tokio::task::spawn_blocking(move || {
        let owned = built;
        let result = owned
            .process
            .shutdown()
            .map_err(|error| MemoryError::EmbeddingShutdown(Box::new(error)));
        drop(owned);
        result
    })
    .await
    .map_err(|error| MemoryError::EmbeddingShutdownWorker(Box::new(error)))?
}

#[cfg(feature = "local-embeddings")]
fn latch_failed_cleanup(core: &MemoryCore) {
    core.quiescence_failed.store(true, Ordering::Release);
    core.closing.store(true, Ordering::Release);
    core.stop.cancel();
    core.embedding_changes.notify_waiters();
}

#[cfg(feature = "local-embeddings")]
async fn dispose_after_error(
    core: &MemoryCore,
    built: BuiltEmbeddings,
    operation: MemoryError,
) -> MemoryError {
    match dispose_built(built).await {
        Ok(()) => operation,
        Err(cleanup) => {
            latch_failed_cleanup(core);
            MemoryError::EmbeddingCleanup {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup),
            }
        }
    }
}

#[cfg(feature = "local-embeddings")]
async fn shutdown_backend(core: &MemoryCore) -> Result<(), MemoryError> {
    let (hybrid, detachment) = match core.consolidator.detach_all_semantics().await {
        Ok(hybrid) => (hybrid, None),
        Err(error) => (None, Some(error)),
    };
    let process = match core.embeddings.lock() {
        Ok(mut state) => state.process.take(),
        Err(poisoned) => {
            core.quiescence_failed.store(true, Ordering::Release);
            poisoned.into_inner().process.take()
        }
    };
    let shutdown = stop_backend(hybrid, process, core._agent.clone()).await;
    if detachment.is_some() || shutdown.is_err() {
        core.quiescence_failed.store(true, Ordering::Release);
    }
    match (detachment, shutdown) {
        (None, Ok(())) => Ok(()),
        (Some(error), Ok(())) => Err(error.into()),
        (None, Err(error)) => Err(error),
        (Some(detachment), Err(shutdown)) => Err(MemoryError::BackendCleanup {
            detachment,
            shutdown: Box::new(shutdown),
        }),
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
) -> Result<ConfiguredEmbeddings, MemoryError> {
    let EmbeddingSettings::EmbeddingGemma2(configuration) = settings else {
        return Ok(ConfiguredEmbeddings {
            status: EmbeddingStatus::Disabled,
            profile: None,
            #[cfg(feature = "local-embeddings")]
            process: None,
        });
    };
    #[cfg(not(feature = "local-embeddings"))]
    {
        let _ = (agent, consolidator, configuration, directory, cancellation);
        Ok(ConfiguredEmbeddings {
            status: EmbeddingStatus::Unavailable {
                diagnostic: MemoryDiagnostic::EmbeddingFeatureUnavailable,
            },
            profile: None,
        })
    }
    #[cfg(feature = "local-embeddings")]
    {
        let configuration = configuration.clone();
        let dimensions = configuration.dimensions;
        let owned = (configuration, directory, cancellation, agent.clone());
        let construction = tokio::task::spawn_blocking(move || {
            let (configuration, directory, cancellation, agent) = owned;
            build_embeddings(configuration, directory, cancellation, agent)
        })
        .await
        .map_err(|_| MemoryError::Worker)?;
        let (hybrid, process, status, profile) = match construction {
            Ok(mut built) => {
                let hybrid = built.hybrid.take().ok_or(MemoryError::Worker)?;
                let profile = hybrid.specification().profile().clone();
                (
                    Ok(hybrid),
                    Some(Arc::clone(&built.process)),
                    EmbeddingStatus::Enabled { dimensions },
                    Some(profile),
                )
            }
            Err(error) => {
                let diagnostic = match &error {
                    crate::memory::vector::VectorError::Embedding(error)
                        if crate::memory::embedding::isolated::is_unavailable(error) =>
                    {
                        MemoryDiagnostic::EmbeddingIsolationUnavailable
                    }
                    _ => MemoryDiagnostic::EmbeddingInitializationFailed,
                };
                (
                    Err(error),
                    None,
                    EmbeddingStatus::Unavailable { diagnostic },
                    None,
                )
            }
        };
        if let Err(error) = consolidator.set_hybrid_search_result(hybrid).await {
            stop_embeddings(process, agent.clone()).await?;
            return Err(error.into());
        }
        Ok(ConfiguredEmbeddings {
            status,
            process,
            profile,
        })
    }
}

struct ConfiguredEmbeddings {
    status: EmbeddingStatus,
    profile: Option<EmbeddingProfile>,
    #[cfg(feature = "local-embeddings")]
    process: Option<Arc<crate::memory::embedding::isolated::IsolatedEmbeddingGemma2>>,
}

#[cfg(feature = "local-embeddings")]
struct BuiltEmbeddings {
    hybrid: Option<crate::memory::search::HybridSearch>,
    process: Arc<crate::memory::embedding::isolated::IsolatedEmbeddingGemma2>,
    _agent: Agent,
}

#[cfg(feature = "local-embeddings")]
async fn stop_embeddings(
    process: Option<Arc<crate::memory::embedding::isolated::IsolatedEmbeddingGemma2>>,
    agent: Agent,
) -> Result<(), MemoryError> {
    stop_backend(None, process, agent).await
}

#[cfg(feature = "local-embeddings")]
fn build_embeddings(
    configuration: EmbeddingGemma2Settings,
    directory: PathBuf,
    cancellation: CancellationToken,
    agent: Agent,
) -> Result<BuiltEmbeddings, crate::memory::vector::VectorError> {
    use crate::memory::embedding::EmbeddingError;
    use crate::memory::embedding::isolated::{IsolatedEmbeddingGemma2, IsolationConfiguration};
    use crate::memory::embedding::wire::WorkerConfiguration;
    use crate::memory::search::HybridSearch;
    use crate::memory::vector::{VectorError, VectorIndex, VectorLimits};

    let retained_agent = agent;
    if cancellation.is_cancelled() {
        return Err(VectorError::Cancelled);
    }
    let model = IsolatedEmbeddingGemma2::load(
        IsolationConfiguration {
            runtime_library: configuration.runtime_library,
            model_bundle: configuration.model_bundle,
            worker: WorkerConfiguration {
                runtime_sha256: configuration.runtime_sha256,
                dimensions: configuration.dimensions,
                threads: configuration.execution.threads,
                max_tokens: configuration.execution.max_tokens,
                inference_timeout_ms: configuration.execution.inference_timeout_ms,
                load_timeout_ms: configuration.execution.load_timeout_ms,
            },
        },
        &cancellation,
    )?;
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
    let hybrid = HybridSearch::new(
        index,
        Arc::clone(&model) as Arc<dyn crate::memory::embedding::Embedder>,
    )?;
    Ok(BuiltEmbeddings {
        hybrid: Some(hybrid),
        process: model,
        _agent: retained_agent,
    })
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
