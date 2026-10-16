use thiserror::Error;

#[derive(Clone, Copy, Debug, Error)]
pub enum WorkerError {
    #[error("embedding worker is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("embedding worker descriptor boundary rejected")]
    DescriptorBoundary,
    #[error("invalid embedding worker request")]
    InvalidRequest,
    #[error("embedding worker initialization failed")]
    Initialization,
    #[error("embedding worker inference failed")]
    Inference,
    #[error("embedding worker response output failed")]
    Output,
}

pub fn run(probe: bool) -> Result<(), WorkerError> {
    #[cfg(target_os = "linux")]
    {
        linux::run(probe)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = probe;
        Err(WorkerError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::os::fd::AsFd;
    use std::time::Duration;

    use rustix::fs::{FileType, Mode, OFlags};
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;

    use super::super::Embedder;
    use super::super::embeddinggemma2::{
        EmbeddingGemma2, EmbeddingGemma2Options, EmbeddingRuntime, SUPPORTED_DIMENSIONS,
    };
    use super::super::wire::{
        MAX_ERROR_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
        WorkerConfiguration, WorkerFailure, WorkerFailureKind, WorkerRequest, WorkerResponse,
    };
    use super::WorkerError;

    const RUNTIME_PATH: &str = "/runtime/libonnxruntime.so";
    const MODEL_PATH: &str = "/model";
    const MAX_RUNTIME_BYTES: u64 = 256 * 1024 * 1024;

    pub(super) fn run(probe: bool) -> Result<(), WorkerError> {
        verify_descriptors()?;
        let mut input = io::stdin().lock();
        let mut output = io::stdout().lock();
        let result = write_response(
            &mut output,
            &WorkerResponse::Ready {
                version: PROTOCOL_VERSION,
            },
        )
        .and_then(|()| {
            if probe {
                Ok(())
            } else {
                serve(&mut input, &mut output)
            }
        });
        if let Err(error) = result {
            let kind = match error {
                WorkerError::InvalidRequest => WorkerFailureKind::InvalidRequest,
                WorkerError::Inference => WorkerFailureKind::Inference,
                WorkerError::Output => return Err(error),
                _ => WorkerFailureKind::Initialization,
            };
            write_response(
                &mut output,
                &WorkerResponse::Failed {
                    failure: WorkerFailure {
                        kind,
                        message: bounded_message(&error.to_string()),
                    },
                },
            )?;
            return Err(error);
        }
        Ok(())
    }

    fn verify_descriptors() -> Result<(), WorkerError> {
        let names = fs::read_dir("/proc/self/fd")
            .map_err(|_| WorkerError::DescriptorBoundary)?
            .take(64)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| WorkerError::DescriptorBoundary)?;
        if names.len() == 64 {
            return Err(WorkerError::DescriptorBoundary);
        }
        for name in names {
            let descriptor = name
                .to_str()
                .and_then(|name| name.parse::<u64>().ok())
                .ok_or(WorkerError::DescriptorBoundary)?;
            if descriptor > 2 {
                match fs::symlink_metadata(std::path::Path::new("/proc/self/fd").join(name)) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => return Err(WorkerError::DescriptorBoundary),
                }
            }
        }
        verify_stdio(&io::stdin(), false)?;
        verify_stdio(&io::stdout(), true)?;
        verify_stdio(&io::stderr(), true)
    }

    fn verify_stdio(descriptor: &impl AsFd, writable: bool) -> Result<(), WorkerError> {
        let metadata =
            rustix::fs::fstat(descriptor).map_err(|_| WorkerError::DescriptorBoundary)?;
        if !matches!(
            FileType::from_raw_mode(metadata.st_mode),
            FileType::RegularFile | FileType::Fifo
        ) {
            return Err(WorkerError::DescriptorBoundary);
        }
        let flags =
            rustix::fs::fcntl_getfl(descriptor).map_err(|_| WorkerError::DescriptorBoundary)?;
        let access = flags & OFlags::ACCMODE;
        let expected = if writable {
            OFlags::WRONLY
        } else {
            OFlags::RDONLY
        };
        if access != expected && access != OFlags::RDWR {
            return Err(WorkerError::DescriptorBoundary);
        }
        Ok(())
    }

    fn serve(input: &mut impl Read, output: &mut impl Write) -> Result<(), WorkerError> {
        let Some(request) = read_request(input)? else {
            return Ok(());
        };
        let WorkerRequest::Load { configuration } = request else {
            return Err(WorkerError::InvalidRequest);
        };
        validate_configuration(&configuration)?;
        let model = load_model(&configuration)?;
        write_response(
            output,
            &WorkerResponse::Loaded {
                profile: model.profile().clone(),
            },
        )?;
        let mut previous_id = 0_u64;
        while let Some(request) = read_request(input)? {
            let WorkerRequest::Embed { id, input } = request else {
                return Err(WorkerError::InvalidRequest);
            };
            if previous_id.checked_add(1) != Some(id) {
                return Err(WorkerError::InvalidRequest);
            }
            let vector = model
                .embed(&input, &CancellationToken::new())
                .map_err(|_| WorkerError::Inference)?;
            model
                .profile()
                .ensure_compatible(vector.profile_fingerprint())
                .and_then(|()| model.profile().validate_vector(vector.values()))
                .map_err(|_| WorkerError::Inference)?;
            write_response(
                output,
                &WorkerResponse::Vector {
                    id,
                    values: vector.into_values(),
                },
            )?;
            previous_id = id;
        }
        Ok(())
    }

    fn validate_configuration(configuration: &WorkerConfiguration) -> Result<(), WorkerError> {
        if configuration.runtime_sha256.len() != 64
            || !configuration
                .runtime_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !SUPPORTED_DIMENSIONS.contains(&configuration.dimensions)
            || !(1..=16).contains(&configuration.threads)
            || !(8..=8192).contains(&configuration.max_tokens)
            || !(1..=300_000).contains(&configuration.inference_timeout_ms)
            || !(1..=300_000).contains(&configuration.load_timeout_ms)
        {
            return Err(WorkerError::InvalidRequest);
        }
        Ok(())
    }

    fn load_model(configuration: &WorkerConfiguration) -> Result<EmbeddingGemma2, WorkerError> {
        EmbeddingRuntime::require_telemetry_suppression()
            .map_err(|_| WorkerError::Initialization)?;
        let descriptor = rustix::fs::open(
            RUNTIME_PATH,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| WorkerError::Initialization)?;
        let mut file = File::from(descriptor);
        let metadata = file.metadata().map_err(|_| WorkerError::Initialization)?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RUNTIME_BYTES {
            return Err(WorkerError::Initialization);
        }
        let mut digest = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| WorkerError::Initialization)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > metadata.len() {
                return Err(WorkerError::Initialization);
            }
            digest.update(&buffer[..count]);
        }
        if bytes != metadata.len()
            || !format!("{:x}", digest.finalize())
                .eq_ignore_ascii_case(&configuration.runtime_sha256)
        {
            return Err(WorkerError::Initialization);
        }
        drop(file);
        let runtime =
            EmbeddingRuntime::initialize(RUNTIME_PATH).map_err(|_| WorkerError::Initialization)?;
        if !runtime
            .library_sha256()
            .eq_ignore_ascii_case(&configuration.runtime_sha256)
        {
            return Err(WorkerError::Initialization);
        }
        EmbeddingGemma2::load(
            runtime,
            MODEL_PATH,
            EmbeddingGemma2Options {
                threads: configuration.threads,
                max_tokens: configuration.max_tokens,
                inference_timeout: Duration::from_millis(configuration.inference_timeout_ms),
                load_timeout: Duration::from_millis(configuration.load_timeout_ms),
            },
            &CancellationToken::new(),
        )
        .and_then(|model| model.with_dimensions(configuration.dimensions))
        .map_err(|_| WorkerError::Initialization)
    }

    fn read_request(input: &mut impl Read) -> Result<Option<WorkerRequest>, WorkerError> {
        let mut prefix = [0_u8; 4];
        loop {
            match input.read(&mut prefix[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(WorkerError::InvalidRequest),
            }
        }
        input
            .read_exact(&mut prefix[1..])
            .map_err(|_| WorkerError::InvalidRequest)?;
        let length = u32::from_be_bytes(prefix) as usize;
        if length == 0 || length > MAX_REQUEST_BYTES {
            return Err(WorkerError::InvalidRequest);
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(length)
            .map_err(|_| WorkerError::InvalidRequest)?;
        payload.resize(length, 0);
        input
            .read_exact(&mut payload)
            .map_err(|_| WorkerError::InvalidRequest)?;
        serde_json::from_slice(&payload)
            .map(Some)
            .map_err(|_| WorkerError::InvalidRequest)
    }

    fn write_response(
        output: &mut impl Write,
        response: &WorkerResponse,
    ) -> Result<(), WorkerError> {
        let payload = serde_json::to_vec(response).map_err(|_| WorkerError::Initialization)?;
        if payload.is_empty() || payload.len() > MAX_RESPONSE_BYTES {
            return Err(WorkerError::Initialization);
        }
        let length = u32::try_from(payload.len()).map_err(|_| WorkerError::Initialization)?;
        output
            .write_all(&length.to_be_bytes())
            .and_then(|()| output.write_all(&payload))
            .and_then(|()| output.flush())
            .map_err(|_| WorkerError::Output)
    }

    fn bounded_message(message: &str) -> String {
        let mut end = message.len().min(MAX_ERROR_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message[..end].to_owned()
    }
}
