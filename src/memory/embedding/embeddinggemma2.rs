use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::time::{Duration, Instant};

use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::run_options::RunOptions;
use ort::value::Tensor;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;

use super::{
    Embedder, EmbeddingError, EmbeddingInput, EmbeddingMetric, EmbeddingNormalization,
    EmbeddingPreprocessing, EmbeddingProfile, EmbeddingSpec, EmbeddingVector, normalize_l2,
};

pub const SOURCE_MODEL_REVISION: &str = "914f7f89142e33e77833254d9c9b90c3cef7303b";
pub const ONNX_MODEL_REVISION: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";
pub const SUPPORTED_DIMENSIONS: [usize; 4] = [128, 256, 512, 768];

const ARTIFACTS: [Artifact; 4] = [
    Artifact {
        path: "config.json",
        bytes: 5_031,
        sha256: "8d011bfe08b5e345bbe0b81e5c6fd02c381920b345b986047bc2a33ce7b90d1d",
    },
    Artifact {
        path: "tokenizer.json",
        bytes: 32_170_510,
        sha256: "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
    },
    Artifact {
        path: "onnx/model.onnx",
        bytes: 421_421,
        sha256: "bc47de15f81208a5c99e5ab10f746d5e33b51ea228b7dc0bef9c133a94f1c1c3",
    },
    Artifact {
        path: "onnx/model.onnx_data",
        bytes: 1_084_170_240,
        sha256: "9fd452bfc6916e5a92f080f8dada35598ca4512b21c65897d4c5974d5f9630a2",
    },
];

#[derive(Debug, Error)]
pub enum EmbeddingGemma2Error {
    #[error("invalid EmbeddingGemma 2 option: {0}")]
    InvalidOption(&'static str),
    #[error("EmbeddingGemma 2 I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("EmbeddingGemma 2 artifact {0} differs from the pinned model revision")]
    ArtifactIntegrity(String),
    #[error("EmbeddingGemma 2 model configuration does not identify the supported model")]
    ModelIdentity,
    #[error(
        "ONNX Runtime initialization failed; verify the selected native library and restart the host"
    )]
    RuntimeInitialization,
    #[error("ONNX Runtime is already initialized; share the existing EmbeddingRuntime instance")]
    RuntimeAlreadyInitialized,
    #[error("EmbeddingGemma 2 native resources are busy")]
    Busy,
    #[error("EmbeddingGemma 2 runtime already owns a loaded model; share it with with_dimensions")]
    ModelAlreadyLoaded,
    #[error("EmbeddingGemma 2 operation {0} exceeded its deadline")]
    Deadline(&'static str),
    #[error("EmbeddingGemma 2 session lock is poisoned")]
    Poisoned,
    #[error("EmbeddingGemma 2 cancellation monitor failed")]
    MonitorFailed,
    #[error("EmbeddingGemma 2 graph inputs or outputs do not match the pinned model")]
    GraphContract,
    #[error("EmbeddingGemma 2 tokenizer failed: {0}")]
    Tokenizer(#[source] tokenizers::Error),
    #[error("EmbeddingGemma 2 configuration failed: {0}")]
    Configuration(#[from] serde_json::Error),
    #[error("EmbeddingGemma 2 ONNX execution failed: {0}")]
    Onnx(#[from] ort::Error),
}

pub struct EmbeddingRuntime {
    library_path: PathBuf,
    library_sha256: String,
    information: String,
    admission: Mutex<()>,
    model_loaded: AtomicBool,
}

impl EmbeddingRuntime {
    pub fn initialize(library_path: impl AsRef<Path>) -> Result<Arc<Self>, EmbeddingError> {
        let path = library_path
            .as_ref()
            .canonicalize()
            .map_err(|source| native(io_error(library_path.as_ref(), source)))?;
        let metadata =
            std::fs::metadata(&path).map_err(|source| native(io_error(&path, source)))?;
        if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
            return Err(native(EmbeddingGemma2Error::InvalidOption(
                "runtime library must be a regular file no larger than 256 MiB",
            )));
        }
        let library_sha256 = hash_file(&path, metadata.len(), &CancellationToken::new(), None)?;
        let path_text = path.to_str().ok_or_else(|| {
            native(EmbeddingGemma2Error::InvalidOption(
                "runtime path must be UTF-8",
            ))
        })?;
        let initialization = std::panic::catch_unwind(|| {
            let initialized = ort::init_from(path_text)
                .with_name("everything-local-embeddings")
                .with_telemetry(false)
                .commit()?;
            Ok::<_, ort::Error>((initialized, ort::info().to_owned()))
        })
        .map_err(|_| native(EmbeddingGemma2Error::RuntimeInitialization))?
        .map_err(|error| native(error.into()))?;
        if !initialization.0 {
            return Err(native(EmbeddingGemma2Error::RuntimeAlreadyInitialized));
        }
        Ok(Arc::new(Self {
            library_path: path,
            library_sha256,
            information: initialization.1,
            admission: Mutex::new(()),
            model_loaded: AtomicBool::new(false),
        }))
    }

    pub fn library_path(&self) -> &Path {
        &self.library_path
    }

    pub fn information(&self) -> &str {
        &self.information
    }

    pub fn library_sha256(&self) -> &str {
        &self.library_sha256
    }
}

#[derive(Clone, Debug)]
pub struct EmbeddingGemma2Options {
    pub threads: usize,
    pub max_tokens: usize,
    pub inference_timeout: Duration,
    pub load_timeout: Duration,
}

impl Default for EmbeddingGemma2Options {
    fn default() -> Self {
        Self {
            threads: 2,
            max_tokens: 2_048,
            inference_timeout: Duration::from_secs(30),
            load_timeout: Duration::from_secs(120),
        }
    }
}

impl EmbeddingGemma2Options {
    fn validate(&self) -> Result<(), EmbeddingError> {
        if !(1..=16).contains(&self.threads) {
            return Err(native(EmbeddingGemma2Error::InvalidOption(
                "threads must be between 1 and 16",
            )));
        }
        if !(8..=8_192).contains(&self.max_tokens) {
            return Err(native(EmbeddingGemma2Error::InvalidOption(
                "max_tokens must be between 8 and 8192",
            )));
        }
        if self.inference_timeout.is_zero() || self.inference_timeout > Duration::from_secs(300) {
            return Err(native(EmbeddingGemma2Error::InvalidOption(
                "inference timeout must be positive and at most 300 seconds",
            )));
        }
        if self.load_timeout.is_zero() || self.load_timeout > Duration::from_secs(300) {
            return Err(native(EmbeddingGemma2Error::InvalidOption(
                "load timeout must be positive and at most 300 seconds",
            )));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct EmbeddingGemma2 {
    profile: EmbeddingProfile,
    inner: Arc<ModelState>,
}

struct ModelState {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    artifact_fingerprint: String,
    options: EmbeddingGemma2Options,
    reservation: ModelReservation,
}

struct ModelReservation {
    runtime: Arc<EmbeddingRuntime>,
}

impl Drop for ModelReservation {
    fn drop(&mut self) {
        self.runtime.model_loaded.store(false, Ordering::Release);
    }
}

impl EmbeddingGemma2 {
    pub fn load(
        runtime: Arc<EmbeddingRuntime>,
        bundle_root: impl AsRef<Path>,
        options: EmbeddingGemma2Options,
        cancellation: &CancellationToken,
    ) -> Result<Self, EmbeddingError> {
        options.validate()?;
        let deadline = Instant::now() + options.load_timeout;
        checkpoint(cancellation, Some(deadline), "load")?;
        let _admission = runtime.admission.try_lock().map_err(lock_error)?;
        runtime
            .model_loaded
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| native(EmbeddingGemma2Error::ModelAlreadyLoaded))?;
        let reservation = ModelReservation {
            runtime: Arc::clone(&runtime),
        };
        let root = bundle_root
            .as_ref()
            .canonicalize()
            .map_err(|source| native(io_error(bundle_root.as_ref(), source)))?;
        let mut bundle_hash = Sha256::new();
        bundle_hash.update(b"everything.embeddinggemma2.onnx.bundle.v1\0");
        for artifact in &ARTIFACTS {
            let path = root.join(artifact.path);
            let canonical = path
                .canonicalize()
                .map_err(|source| native(io_error(&path, source)))?;
            let metadata =
                std::fs::metadata(&canonical).map_err(|source| native(io_error(&path, source)))?;
            if !canonical.starts_with(&root)
                || !metadata.is_file()
                || metadata.len() != artifact.bytes
            {
                return Err(native(EmbeddingGemma2Error::ArtifactIntegrity(
                    artifact.path.to_owned(),
                )));
            }
            let digest = hash_file(&canonical, artifact.bytes, cancellation, Some(deadline))?;
            if digest != artifact.sha256 {
                return Err(native(EmbeddingGemma2Error::ArtifactIntegrity(
                    artifact.path.to_owned(),
                )));
            }
            bundle_hash.update((artifact.path.len() as u64).to_le_bytes());
            bundle_hash.update(artifact.path.as_bytes());
            bundle_hash.update(digest.as_bytes());
        }
        let artifact_fingerprint = format!("{:x}", bundle_hash.finalize());
        let configuration_file = root.join("config.json");
        let configuration = read_bounded(&configuration_file, ARTIFACTS[0].bytes)?;
        let configuration: ModelConfiguration =
            serde_json::from_slice(&configuration).map_err(|error| native(error.into()))?;
        if configuration.model_type != "embedding_gemma2"
            || configuration.architectures != ["EmbeddingGemma2Model"]
            || configuration.text_config.embedding_dim != 768
            || configuration.text_config.hidden_size != 512
            || configuration.text_config.num_hidden_layers != 24
        {
            return Err(native(EmbeddingGemma2Error::ModelIdentity));
        }
        let tokenizer_bytes = read_bounded(&root.join("tokenizer.json"), ARTIFACTS[1].bytes)?;
        let mut tokenizer = Tokenizer::from_bytes(tokenizer_bytes)
            .map_err(|error| native(EmbeddingGemma2Error::Tokenizer(error)))?;
        tokenizer
            .with_truncation(None)
            .map_err(|error| native(EmbeddingGemma2Error::Tokenizer(error)))?;
        tokenizer.with_padding(None);
        checkpoint(cancellation, Some(deadline), "load")?;
        let session = Session::builder()
            .and_then(|builder| builder.with_no_environment_execution_providers())
            .and_then(|builder| {
                builder.with_execution_providers([CPUExecutionProvider::default()
                    .with_arena_allocator(false)
                    .build()])
            })
            .and_then(|builder| builder.with_intra_threads(options.threads))
            .and_then(|builder| builder.with_inter_threads(1))
            .and_then(|builder| builder.with_parallel_execution(false))
            .and_then(|builder| builder.with_memory_pattern(false))
            .and_then(|builder| builder.with_optimization_level(GraphOptimizationLevel::Level1))
            .and_then(|builder| builder.with_deterministic_compute(true))
            .and_then(|builder| builder.commit_from_file(root.join("onnx/model.onnx")))
            .map_err(|error| native(error.into()))?;
        checkpoint(cancellation, Some(deadline), "load")?;
        let input_names: Vec<&str> = session
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect();
        if input_names
            != [
                "input_ids",
                "attention_mask",
                "image_features",
                "video_features",
                "audio_features",
            ]
            || !session
                .outputs
                .iter()
                .any(|output| output.name == "sentence_embedding")
        {
            return Err(native(EmbeddingGemma2Error::GraphContract));
        }
        let revision = format!(
            "source={SOURCE_MODEL_REVISION};onnx={ONNX_MODEL_REVISION};bundle={artifact_fingerprint};runtime={}",
            runtime.library_sha256
        );
        let preprocessing = EmbeddingPreprocessing::new(
            "ort-2.0.0-rc.10:tokenizers-0.22.2:fp32:graph-masked-mean-project768:bos-eos:reject-long:truncate-l2-v1",
            "task: search result | query: {text}",
            "title: {title} | text: {text}",
            "none",
        )?;
        let spec = EmbeddingSpec::new(
            "google/embeddinggemma-2",
            revision,
            SUPPORTED_DIMENSIONS.to_vec(),
            preprocessing,
            EmbeddingNormalization::UnitL2,
            EmbeddingMetric::Cosine,
        )?;
        Ok(Self {
            profile: EmbeddingProfile::new(spec, 768)?,
            inner: Arc::new(ModelState {
                session: Mutex::new(session),
                tokenizer,
                artifact_fingerprint,
                options,
                reservation,
            }),
        })
    }

    pub fn with_dimensions(&self, dimensions: usize) -> Result<Self, EmbeddingError> {
        Ok(Self {
            profile: EmbeddingProfile::new(self.profile.spec().clone(), dimensions)?,
            inner: Arc::clone(&self.inner),
        })
    }

    pub fn artifact_fingerprint(&self) -> &str {
        &self.inner.artifact_fingerprint
    }

    pub fn options(&self) -> &EmbeddingGemma2Options {
        &self.inner.options
    }
}

impl Embedder for EmbeddingGemma2 {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }

    fn embed(
        &self,
        input: &EmbeddingInput,
        cancellation: &CancellationToken,
    ) -> Result<EmbeddingVector, EmbeddingError> {
        let deadline = Instant::now() + self.inner.options.inference_timeout;
        checkpoint(cancellation, Some(deadline), "inference")?;
        let _admission = self
            .inner
            .reservation
            .runtime
            .admission
            .try_lock()
            .map_err(lock_error)?;
        let formatted = self.profile.spec().preprocessing().format(input);
        let encoded = self
            .inner
            .tokenizer
            .encode(formatted, true)
            .map_err(|error| native(EmbeddingGemma2Error::Tokenizer(error)))?;
        let tokens = encoded.get_ids();
        if tokens.len() > self.inner.options.max_tokens {
            return Err(EmbeddingError::Limit {
                resource: "EmbeddingGemma 2 input tokens including BOS/EOS",
                limit: self.inner.options.max_tokens,
            });
        }
        checkpoint(cancellation, Some(deadline), "inference")?;
        let input_ids = Tensor::from_array((
            [1, tokens.len()],
            tokens
                .iter()
                .map(|token| i64::from(*token))
                .collect::<Vec<_>>(),
        ))
        .map_err(|error| native(error.into()))?;
        let attention_mask = Tensor::from_array(([1, tokens.len()], vec![1_i64; tokens.len()]))
            .map_err(|error| native(error.into()))?;
        let allocator = ort::memory::Allocator::default();
        let image_features =
            Tensor::<f32>::new(&allocator, [0_usize, 512]).map_err(|error| native(error.into()))?;
        let video_features =
            Tensor::<f32>::new(&allocator, [0_usize, 512]).map_err(|error| native(error.into()))?;
        let audio_features =
            Tensor::<f32>::new(&allocator, [0_usize, 512]).map_err(|error| native(error.into()))?;
        let options = RunOptions::new().map_err(|error| native(error.into()))?;
        let mut session = self
            .inner
            .session
            .lock()
            .map_err(|_| native(EmbeddingGemma2Error::Poisoned))?;
        let values = std::thread::scope(|scope| {
            let (finished, completion) = mpsc::channel::<()>();
            let cancellation_options = &options;
            let monitor = std::thread::Builder::new()
                .name("everything-embedding-cancellation".into())
                .spawn_scoped(scope, move || {
                    loop {
                        match completion.recv_timeout(Duration::from_millis(10)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        if let Err(reason) = checkpoint(cancellation, Some(deadline), "inference") {
                            cancellation_options
                                .terminate()
                                .map_err(|error| native(error.into()))?;
                            return Err(reason);
                        }
                    }
                })
                .map_err(|source| native(io_error("embedding cancellation monitor", source)))?;
            let result = session.run_with_options(
                ort::inputs![
                    "input_ids" => input_ids,
                    "attention_mask" => attention_mask,
                    "image_features" => image_features,
                    "video_features" => video_features,
                    "audio_features" => audio_features,
                ],
                &options,
            );
            drop(finished);
            monitor
                .join()
                .map_err(|_| native(EmbeddingGemma2Error::MonitorFailed))??;
            checkpoint(cancellation, Some(deadline), "inference")?;
            let result = result.map_err(|error| native(error.into()))?;
            let output = result
                .get("sentence_embedding")
                .ok_or_else(|| native(EmbeddingGemma2Error::GraphContract))?;
            let (shape, values) = output
                .try_extract_tensor::<f32>()
                .map_err(|error| native(error.into()))?;
            if shape.as_ref() != [1, 768] || values.len() != 768 {
                return Err(native(EmbeddingGemma2Error::GraphContract));
            }
            if let Some(index) = values.iter().position(|value| !value.is_finite()) {
                return Err(EmbeddingError::NonfiniteVector(index));
            }
            Ok(values[..self.profile.dimensions()].to_vec())
        })?;
        let mut values = values;
        normalize_l2(&mut values)?;
        EmbeddingVector::new(&self.profile, values)
    }
}

struct Artifact {
    path: &'static str,
    bytes: u64,
    sha256: &'static str,
}

#[derive(Deserialize)]
struct ModelConfiguration {
    model_type: String,
    architectures: Vec<String>,
    text_config: TextConfiguration,
}

#[derive(Deserialize)]
struct TextConfiguration {
    embedding_dim: usize,
    hidden_size: usize,
    num_hidden_layers: usize,
}

fn hash_file(
    path: &Path,
    maximum_bytes: u64,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<String, EmbeddingError> {
    let mut file = File::open(path)
        .map_err(|source| native(io_error(path, source)))?
        .take(maximum_bytes.saturating_add(1));
    let mut buffer = [0_u8; 128 * 1024];
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        checkpoint(cancellation, deadline, "artifact hashing")?;
        let count = file
            .read(&mut buffer)
            .map_err(|source| native(io_error(path, source)))?;
        bytes = bytes.saturating_add(count as u64);
        if bytes > maximum_bytes {
            return Err(native(EmbeddingGemma2Error::ArtifactIntegrity(
                path.display().to_string(),
            )));
        }
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..count]);
    }
}

fn read_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, EmbeddingError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|source| native(io_error(path, source)))?
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| native(io_error(path, source)))?;
    if bytes.len() as u64 != maximum_bytes {
        return Err(native(EmbeddingGemma2Error::ArtifactIntegrity(
            path.display().to_string(),
        )));
    }
    Ok(bytes)
}

fn checkpoint(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    operation: &'static str,
) -> Result<(), EmbeddingError> {
    if cancellation.is_cancelled() {
        return Err(EmbeddingError::Cancelled);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(native(EmbeddingGemma2Error::Deadline(operation)));
    }
    Ok(())
}

fn lock_error<T>(error: TryLockError<T>) -> EmbeddingError {
    native(match error {
        TryLockError::WouldBlock => EmbeddingGemma2Error::Busy,
        TryLockError::Poisoned(_) => EmbeddingGemma2Error::Poisoned,
    })
}

fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> EmbeddingGemma2Error {
    EmbeddingGemma2Error::Io {
        path: path.into(),
        source,
    }
}

fn native(error: EmbeddingGemma2Error) -> EmbeddingError {
    EmbeddingError::Backend(Box::new(error))
}
