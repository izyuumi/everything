#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde::Serialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use vorra::memory::embedding::embeddinggemma2::{
    EmbeddingGemma2, EmbeddingGemma2Options, EmbeddingRuntime, ONNX_MODEL_REVISION,
    SOURCE_MODEL_REVISION, SUPPORTED_DIMENSIONS,
};
use vorra::memory::embedding::{
    Embedder, EmbeddingError, EmbeddingInput, EmbeddingPurpose, EmbeddingVector,
};

const BENCHMARK_ID: &str = "embeddinggemma2-public-retrieval-v1";

#[derive(Parser)]
#[command(
    version,
    about = "Measure real local EmbeddingGemma 2 inference on a bounded public corpus"
)]
struct Arguments {
    #[arg(
        long,
        help = "Trusted ONNX Runtime shared library; it is loaded as native code"
    )]
    runtime_library: PathBuf,
    #[arg(
        long,
        help = "Directory containing the pinned model bundle; no files are downloaded"
    )]
    model_dir: PathBuf,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    threads: u16,
    #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u16).range(8..=8192))]
    max_tokens: u16,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=10))]
    repetitions: u8,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(0..=3))]
    warmup_iterations: u8,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..=300))]
    inference_timeout_seconds: u16,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u16).range(1..=300))]
    load_timeout_seconds: u16,
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u16).range(1..=900))]
    deadline_seconds: u16,
}

#[derive(Debug, Error)]
enum BenchmarkError {
    #[error(transparent)]
    Embedding(#[from] EmbeddingError),
    #[error("benchmark I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("benchmark JSON output failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("benchmark worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("benchmark interrupted")]
    Interrupted,
    #[error("benchmark exceeded its {0}-second deadline")]
    Deadline(u16),
    #[error("invalid benchmark measurement: {0}")]
    Measurement(&'static str),
}

#[derive(Clone, Copy, Serialize)]
struct PublicDocument {
    id: &'static str,
    title: &'static str,
    text: &'static str,
}

#[derive(Clone, Copy, Serialize)]
struct PublicQuery {
    id: &'static str,
    text: &'static str,
    relevant_document_ids: &'static [&'static str],
}

const DOCUMENTS: [PublicDocument; 8] = [
    PublicDocument {
        id: "mars-surface",
        title: "The surface of Mars",
        text: "Mars is the fourth planet from the Sun. It is a rocky terrestrial world with a thin atmosphere. Iron minerals in its soil oxidize and give its surface a reddish appearance.",
    },
    PublicDocument {
        id: "mars-exploration",
        title: "Exploring Mars",
        text: "Space agencies explore Mars with orbiters, landers, and robotic rovers. NASA's Curiosity and Perseverance rovers study Martian rocks and the history of environments that might once have supported microbial life.",
    },
    PublicDocument {
        id: "photosynthesis",
        title: "Photosynthesis converts light into stored energy",
        text: "During photosynthesis, plants use energy from sunlight to convert carbon dioxide and water into sugars. Oxygen is released as a byproduct, and chemical energy is stored in the sugars.",
    },
    PublicDocument {
        id: "chlorophyll",
        title: "Chlorophyll and plant cells",
        text: "Chlorophyll pigments in chloroplasts absorb light. Photosynthetic reactions use that light energy to support sugar production. Splitting water during these reactions releases oxygen.",
    },
    PublicDocument {
        id: "rust-ownership",
        title: "Rust ownership manages memory",
        text: "Rust associates each value with an owner. Values are dropped when their owner leaves scope, and moving a value transfers ownership. The compiler checks these rules to support memory safety without a garbage collector.",
    },
    PublicDocument {
        id: "rust-borrowing",
        title: "Rust borrowing and references",
        text: "Rust lets code borrow values through references. At a given time, a value can have multiple shared references or one exclusive mutable reference. Lifetime checking prevents references from outliving the data they borrow.",
    },
    PublicDocument {
        id: "macos-launchd",
        title: "Background processes managed by launchd",
        text: "launchd is the macOS service manager. It can start and supervise background processes independently of graphical menu bar applications. The launchctl command manages jobs in a user's launchd domain.",
    },
    PublicDocument {
        id: "macos-launchagents",
        title: "User LaunchAgents at login",
        text: "A macOS user LaunchAgent is described by a property list in the user's Library/LaunchAgents folder. RunAtLoad starts a job when the agent is loaded, and KeepAlive can restart it after it exits.",
    },
];

const QUERIES: [PublicQuery; 4] = [
    PublicQuery {
        id: "mars",
        text: "What do we know about Mars and how is it explored?",
        relevant_document_ids: &["mars-surface", "mars-exploration"],
    },
    PublicQuery {
        id: "plants",
        text: "How do plants convert sunlight into chemical energy and oxygen?",
        relevant_document_ids: &["photosynthesis", "chlorophyll"],
    },
    PublicQuery {
        id: "rust",
        text: "How does Rust use ownership and borrowing to manage memory safely?",
        relevant_document_ids: &["rust-ownership", "rust-borrowing"],
    },
    PublicQuery {
        id: "macos",
        text: "How can a macOS background service start at login without a menu bar application?",
        relevant_document_ids: &["macos-launchd", "macos-launchagents"],
    },
];

#[derive(Serialize)]
struct BenchmarkReport {
    schema_version: u16,
    status: &'static str,
    benchmark: &'static str,
    unix_timestamp_seconds: u64,
    operating_system: &'static str,
    architecture: &'static str,
    application_version: &'static str,
    cache_state: &'static str,
    interpretation: &'static str,
    configuration: BenchmarkConfiguration,
    runtime: RuntimeReport,
    model: ModelReport,
    documents: &'static [PublicDocument],
    queries: &'static [PublicQuery],
    dimensions: Vec<DimensionReport>,
    elapsed_ms: f64,
    process_memory: MemoryReport,
}

#[derive(Serialize)]
struct BenchmarkConfiguration {
    threads: u16,
    max_tokens: u16,
    repetitions: u8,
    warmup_iterations: u8,
    inference_timeout_seconds: u16,
    load_timeout_seconds: u16,
    deadline_seconds: u16,
}

#[derive(Serialize)]
struct RuntimeReport {
    library_sha256: String,
    information: String,
    initialization_ms: f64,
}

#[derive(Serialize)]
struct ModelReport {
    model: String,
    revision: String,
    source_reference_revision: &'static str,
    onnx_revision: &'static str,
    artifact_fingerprint: String,
    load_ms: f64,
}

#[derive(Serialize)]
struct DimensionReport {
    dimensions: usize,
    profile_fingerprint: String,
    warmup_ms: f64,
    query_latency: LatencyReport,
    document_latency: LatencyReport,
    all_finite: bool,
    minimum_l2_norm: f64,
    maximum_l2_norm: f64,
    maximum_l2_norm_error: f64,
    ranking_metrics: RankingMetrics,
    samples: Vec<InferenceSample>,
    rankings: Vec<QueryRanking>,
}

#[derive(Serialize)]
struct InferenceSample {
    repetition: u8,
    input_id: &'static str,
    purpose: EmbeddingPurpose,
    latency_ms: f64,
    dimensions: usize,
    finite: bool,
    l2_norm: f64,
}

#[derive(Serialize)]
struct LatencyReport {
    samples: usize,
    minimum_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    maximum_ms: f64,
}

#[derive(Serialize)]
struct QueryRanking {
    repetition: u8,
    query_id: &'static str,
    metrics: RankingMetrics,
    documents: Vec<RankedDocument>,
}

#[derive(Serialize)]
struct RankedDocument {
    document_id: &'static str,
    cosine_similarity: f64,
    relevant: bool,
}

#[derive(Serialize)]
struct RankingMetrics {
    precision_at_1: f64,
    precision_at_3: f64,
    recall_at_3: f64,
    reciprocal_rank: f64,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum MemoryReport {
    #[cfg(target_os = "linux")]
    Available {
        source: &'static str,
        resident_bytes: u64,
        peak_resident_bytes: u64,
    },
    Unavailable {
        reason: &'static str,
    },
}

#[derive(Serialize)]
struct FailureReport {
    schema_version: u16,
    status: &'static str,
    benchmark: &'static str,
    error: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments).await {
        Ok(report) => match write_json(&mut io::stdout().lock(), &report) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            let failure = FailureReport {
                schema_version: 1,
                status: "failed",
                benchmark: BENCHMARK_ID,
                error: error.to_string(),
            };
            if let Err(error) = write_json(&mut io::stderr().lock(), &failure) {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(arguments: Arguments) -> Result<BenchmarkReport, BenchmarkError> {
    let limit = arguments.deadline_seconds;
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let mut worker = tokio::task::spawn_blocking(move || measure(arguments, &worker_cancellation));
    tokio::select! {
        biased;
        signal = tokio::signal::ctrl_c() => {
            cancellation.cancel();
            finish_cancelled(worker).await?;
            signal?;
            Err(BenchmarkError::Interrupted)
        }
        _ = tokio::time::sleep(Duration::from_secs(u64::from(limit))) => {
            cancellation.cancel();
            finish_cancelled(worker).await?;
            Err(BenchmarkError::Deadline(limit))
        }
        report = &mut worker => report?,
    }
}

async fn finish_cancelled(
    worker: tokio::task::JoinHandle<Result<BenchmarkReport, BenchmarkError>>,
) -> Result<(), BenchmarkError> {
    match worker.await? {
        Ok(_) | Err(BenchmarkError::Embedding(EmbeddingError::Cancelled)) => Ok(()),
        Err(error) => Err(error),
    }
}

fn measure(
    arguments: Arguments,
    cancellation: &CancellationToken,
) -> Result<BenchmarkReport, BenchmarkError> {
    let started = Instant::now();
    let initialization_started = Instant::now();
    let runtime = EmbeddingRuntime::initialize(&arguments.runtime_library)?;
    let runtime_report = RuntimeReport {
        library_sha256: runtime.library_sha256().to_owned(),
        information: runtime.information().to_owned(),
        initialization_ms: milliseconds(initialization_started.elapsed()),
    };
    let load_started = Instant::now();
    let model = EmbeddingGemma2::load(
        runtime,
        &arguments.model_dir,
        EmbeddingGemma2Options {
            threads: usize::from(arguments.threads),
            max_tokens: usize::from(arguments.max_tokens),
            inference_timeout: Duration::from_secs(u64::from(arguments.inference_timeout_seconds)),
            load_timeout: Duration::from_secs(u64::from(arguments.load_timeout_seconds)),
        },
        cancellation,
    )?;
    let model_report = ModelReport {
        model: model.profile().spec().model().to_owned(),
        revision: model.profile().spec().revision().to_owned(),
        source_reference_revision: SOURCE_MODEL_REVISION,
        onnx_revision: ONNX_MODEL_REVISION,
        artifact_fingerprint: model.artifact_fingerprint().to_owned(),
        load_ms: milliseconds(load_started.elapsed()),
    };
    let mut dimensions = Vec::with_capacity(SUPPORTED_DIMENSIONS.len());
    for dimension in SUPPORTED_DIMENSIONS {
        checkpoint(cancellation)?;
        dimensions.push(measure_dimension(
            &model.with_dimensions(dimension)?,
            &arguments,
            cancellation,
        )?);
    }
    checkpoint(cancellation)?;
    Ok(BenchmarkReport {
        schema_version: 1,
        status: "completed",
        benchmark: BENCHMARK_ID,
        unix_timestamp_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BenchmarkError::Measurement("system clock predates the Unix epoch"))?
            .as_secs(),
        operating_system: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        application_version: env!("CARGO_PKG_VERSION"),
        cache_state: "uncontrolled operating-system file cache; one fresh model session",
        interpretation: "Illustrative public-corpus retrieval probe, not a general accuracy or Apple-hardware benchmark",
        configuration: BenchmarkConfiguration {
            threads: arguments.threads,
            max_tokens: arguments.max_tokens,
            repetitions: arguments.repetitions,
            warmup_iterations: arguments.warmup_iterations,
            inference_timeout_seconds: arguments.inference_timeout_seconds,
            load_timeout_seconds: arguments.load_timeout_seconds,
            deadline_seconds: arguments.deadline_seconds,
        },
        runtime: runtime_report,
        model: model_report,
        documents: &DOCUMENTS,
        queries: &QUERIES,
        dimensions,
        elapsed_ms: milliseconds(started.elapsed()),
        process_memory: process_memory(),
    })
}

fn measure_dimension(
    model: &EmbeddingGemma2,
    arguments: &Arguments,
    cancellation: &CancellationToken,
) -> Result<DimensionReport, BenchmarkError> {
    let warmup_started = Instant::now();
    for _ in 0..arguments.warmup_iterations {
        model.embed(&EmbeddingInput::query(QUERIES[0].text)?, cancellation)?;
        model.embed(
            &EmbeddingInput::document(DOCUMENTS[0].text, Some(DOCUMENTS[0].title.to_owned()))?,
            cancellation,
        )?;
    }
    let warmup_ms = milliseconds(warmup_started.elapsed());
    let mut samples = Vec::new();
    let mut rankings = Vec::new();
    for repetition in 1..=arguments.repetitions {
        let mut documents = Vec::with_capacity(DOCUMENTS.len());
        for document in DOCUMENTS {
            checkpoint(cancellation)?;
            let input = EmbeddingInput::document(document.text, Some(document.title.to_owned()))?;
            let (vector, sample) =
                measure_input(model, &input, document.id, repetition, cancellation)?;
            samples.push(sample);
            documents.push((document, vector));
        }
        for query in QUERIES {
            checkpoint(cancellation)?;
            let input = EmbeddingInput::query(query.text)?;
            let (vector, sample) =
                measure_input(model, &input, query.id, repetition, cancellation)?;
            samples.push(sample);
            rankings.push(rank_query(query, &vector, &documents, repetition)?);
        }
    }
    let query_samples: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.purpose == EmbeddingPurpose::Query)
        .map(|sample| sample.latency_ms)
        .collect();
    let document_samples: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.purpose == EmbeddingPurpose::Document)
        .map(|sample| sample.latency_ms)
        .collect();
    let minimum_l2_norm = samples
        .iter()
        .map(|sample| sample.l2_norm)
        .reduce(f64::min)
        .ok_or(BenchmarkError::Measurement("no vector measurements"))?;
    let maximum_l2_norm = samples
        .iter()
        .map(|sample| sample.l2_norm)
        .reduce(f64::max)
        .ok_or(BenchmarkError::Measurement("no vector measurements"))?;
    let maximum_l2_norm_error = samples
        .iter()
        .map(|sample| (sample.l2_norm - 1.0).abs())
        .fold(0.0_f64, f64::max);
    Ok(DimensionReport {
        dimensions: model.profile().dimensions(),
        profile_fingerprint: model.profile().fingerprint().as_str().to_owned(),
        warmup_ms,
        query_latency: latency_report(query_samples)?,
        document_latency: latency_report(document_samples)?,
        all_finite: samples.iter().all(|sample| sample.finite),
        minimum_l2_norm,
        maximum_l2_norm,
        maximum_l2_norm_error,
        ranking_metrics: mean_ranking_metrics(&rankings)?,
        samples,
        rankings,
    })
}

fn measure_input(
    model: &dyn Embedder,
    input: &EmbeddingInput,
    input_id: &'static str,
    repetition: u8,
    cancellation: &CancellationToken,
) -> Result<(EmbeddingVector, InferenceSample), BenchmarkError> {
    let started = Instant::now();
    let vector = model.embed(input, cancellation)?;
    let latency_ms = milliseconds(started.elapsed());
    model
        .profile()
        .ensure_compatible(vector.profile_fingerprint())?;
    model.profile().validate_vector(vector.values())?;
    let sample = InferenceSample {
        repetition,
        input_id,
        purpose: input.purpose(),
        latency_ms,
        dimensions: vector.values().len(),
        finite: vector.values().iter().all(|value| value.is_finite()),
        l2_norm: norm(vector.values()),
    };
    Ok((vector, sample))
}

fn rank_query(
    query: PublicQuery,
    vector: &EmbeddingVector,
    documents: &[(PublicDocument, EmbeddingVector)],
    repetition: u8,
) -> Result<QueryRanking, BenchmarkError> {
    let mut ranking = Vec::with_capacity(documents.len());
    for (document, document_vector) in documents {
        if vector.profile_fingerprint() != document_vector.profile_fingerprint()
            || vector.values().len() != document_vector.values().len()
        {
            return Err(BenchmarkError::Measurement(
                "ranking vector profiles differ",
            ));
        }
        let dot: f64 = vector
            .values()
            .iter()
            .zip(document_vector.values())
            .map(|(query, document)| f64::from(*query) * f64::from(*document))
            .sum();
        let cosine_similarity = dot / (norm(vector.values()) * norm(document_vector.values()));
        if !cosine_similarity.is_finite() {
            return Err(BenchmarkError::Measurement(
                "ranking similarity is not finite",
            ));
        }
        ranking.push(RankedDocument {
            document_id: document.id,
            cosine_similarity,
            relevant: query.relevant_document_ids.contains(&document.id),
        });
    }
    ranking.sort_by(|left, right| {
        right
            .cosine_similarity
            .total_cmp(&left.cosine_similarity)
            .then_with(|| left.document_id.cmp(right.document_id))
    });
    let top = ranking
        .first()
        .ok_or(BenchmarkError::Measurement("ranking has no documents"))?;
    let top_three = ranking.len().min(3);
    let relevant_in_three = ranking
        .iter()
        .take(top_three)
        .filter(|document| document.relevant)
        .count();
    let reciprocal_rank = ranking
        .iter()
        .position(|document| document.relevant)
        .map_or(0.0, |index| 1.0 / (index + 1) as f64);
    if query.relevant_document_ids.is_empty() {
        return Err(BenchmarkError::Measurement("query has no relevance labels"));
    }
    let metrics = RankingMetrics {
        precision_at_1: if top.relevant { 1.0 } else { 0.0 },
        precision_at_3: relevant_in_three as f64 / top_three as f64,
        recall_at_3: relevant_in_three as f64 / query.relevant_document_ids.len() as f64,
        reciprocal_rank,
    };
    Ok(QueryRanking {
        repetition,
        query_id: query.id,
        metrics,
        documents: ranking,
    })
}

fn mean_ranking_metrics(rankings: &[QueryRanking]) -> Result<RankingMetrics, BenchmarkError> {
    if rankings.is_empty() {
        return Err(BenchmarkError::Measurement("no query rankings"));
    }
    let count = rankings.len() as f64;
    Ok(RankingMetrics {
        precision_at_1: rankings
            .iter()
            .map(|ranking| ranking.metrics.precision_at_1)
            .sum::<f64>()
            / count,
        precision_at_3: rankings
            .iter()
            .map(|ranking| ranking.metrics.precision_at_3)
            .sum::<f64>()
            / count,
        recall_at_3: rankings
            .iter()
            .map(|ranking| ranking.metrics.recall_at_3)
            .sum::<f64>()
            / count,
        reciprocal_rank: rankings
            .iter()
            .map(|ranking| ranking.metrics.reciprocal_rank)
            .sum::<f64>()
            / count,
    })
}

fn latency_report(mut samples: Vec<f64>) -> Result<LatencyReport, BenchmarkError> {
    if samples.is_empty()
        || samples
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(BenchmarkError::Measurement(
            "latency samples must be finite and nonempty",
        ));
    }
    samples.sort_by(f64::total_cmp);
    let count = samples.len();
    let p50_ms = if count.is_multiple_of(2) {
        (samples[count / 2 - 1] + samples[count / 2]) / 2.0
    } else {
        samples[count / 2]
    };
    Ok(LatencyReport {
        samples: count,
        minimum_ms: samples[0],
        mean_ms: samples.iter().sum::<f64>() / count as f64,
        p50_ms,
        p95_ms: samples[(count * 95).div_ceil(100) - 1],
        maximum_ms: samples[count - 1],
    })
}

fn norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn checkpoint(cancellation: &CancellationToken) -> Result<(), BenchmarkError> {
    if cancellation.is_cancelled() {
        Err(EmbeddingError::Cancelled.into())
    } else {
        Ok(())
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(target_os = "linux")]
fn process_memory() -> MemoryReport {
    fn read() -> io::Result<Option<(u64, u64)>> {
        let mut text = String::new();
        File::open("/proc/self/status")?
            .take(65_537)
            .read_to_string(&mut text)?;
        if text.len() > 65_536 {
            return Err(io::Error::other("process status exceeds its byte limit"));
        }
        let value = |key| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .and_then(|line| line.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|value| value.checked_mul(1_024))
        };
        Ok(value("VmRSS:").zip(value("VmHWM:")))
    }
    match read() {
        Ok(Some((resident_bytes, peak_resident_bytes))) => MemoryReport::Available {
            source: "linux /proc/self/status VmRSS and VmHWM",
            resident_bytes,
            peak_resident_bytes,
        },
        Ok(None) => MemoryReport::Unavailable {
            reason: "Linux process status did not contain readable RSS fields",
        },
        Err(_) => MemoryReport::Unavailable {
            reason: "Linux process memory could not be read",
        },
    }
}

#[cfg(not(target_os = "linux"))]
fn process_memory() -> MemoryReport {
    MemoryReport::Unavailable {
        reason: "native process RSS is not collected here; use the documented external platform time command",
    }
}

fn write_json(writer: &mut impl Write, value: &impl Serialize) -> Result<(), BenchmarkError> {
    serde_json::to_writer_pretty(&mut *writer, value)?;
    writeln!(writer)?;
    Ok(())
}
