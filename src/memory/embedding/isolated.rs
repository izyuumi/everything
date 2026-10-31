use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use super::wire::WorkerConfiguration;
use super::{Embedder, EmbeddingError, EmbeddingInput, EmbeddingProfile, EmbeddingVector};

#[derive(Clone, Debug)]
pub struct IsolationConfiguration {
    pub runtime_library: PathBuf,
    pub model_bundle: PathBuf,
    pub worker: WorkerConfiguration,
}

#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("embedding process isolation is unavailable: {0}")]
    Unavailable(&'static str),
    #[error("embedding process initialization failed: {0}")]
    Initialization(&'static str),
    #[error("embedding process is busy")]
    Busy,
    #[error("embedding process has stopped")]
    Stopped,
    #[error("embedding process state is poisoned")]
    Poisoned,
    #[error("embedding process protocol failed: {0}")]
    Protocol(&'static str),
    #[error("embedding process deadline expired")]
    Deadline,
    #[error("embedding process communication failed")]
    Communication,
    #[error("embedding process cleanup failed")]
    Cleanup,
}

pub fn is_unavailable(error: &EmbeddingError) -> bool {
    matches!(
        error,
        EmbeddingError::Backend(source)
            if matches!(source.downcast_ref::<IsolationError>(), Some(IsolationError::Unavailable(_)))
    )
}

fn failure(error: IsolationError) -> EmbeddingError {
    EmbeddingError::Backend(Box::new(error))
}

pub struct IsolatedEmbeddingGemma2 {
    profile: EmbeddingProfile,
    available: AtomicBool,
    #[cfg(all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    session: std::sync::Mutex<platform::Session>,
    #[cfg(all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    inference_timeout: std::time::Duration,
}

impl IsolatedEmbeddingGemma2 {
    pub fn load(
        configuration: IsolationConfiguration,
        cancellation: &CancellationToken,
    ) -> Result<Arc<Self>, EmbeddingError> {
        #[cfg(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            platform::load(configuration, cancellation)
        }
        #[cfg(not(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (configuration, cancellation);
            Err(failure(IsolationError::Unavailable(
                "unsupported operating system or ELF profile",
            )))
        }
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    pub fn shutdown(&self) -> Result<(), EmbeddingError> {
        self.available.store(false, Ordering::Release);
        #[cfg(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            platform::shutdown(self)
        }
        #[cfg(not(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            Ok(())
        }
    }
}

impl Embedder for IsolatedEmbeddingGemma2 {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }

    fn embed(
        &self,
        input: &EmbeddingInput,
        cancellation: &CancellationToken,
    ) -> Result<EmbeddingVector, EmbeddingError> {
        #[cfg(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            platform::embed(self, input, cancellation)
        }
        #[cfg(not(all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (input, cancellation);
            Err(failure(IsolationError::Unavailable(
                "unsupported operating system or ELF profile",
            )))
        }
    }
}

#[cfg(all(
    target_os = "linux",
    target_env = "gnu",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    use std::io::{self, Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
    use std::sync::{Arc, Mutex, TryLockError};
    use std::time::{Duration, Instant};

    use command_fds::{CommandFdExt, FdMapping};
    use rustix::fs::{FileType, Mode, OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
    use tokio_util::sync::CancellationToken;

    use super::super::embeddinggemma2::{
        ONNX_MODEL_REVISION, SOURCE_MODEL_REVISION, SUPPORTED_DIMENSIONS,
    };
    use super::super::wire::{
        MAX_ERROR_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
        WorkerConfiguration, WorkerRequest, WorkerResponse,
    };
    use super::super::{EmbeddingMetric, EmbeddingNormalization};
    use super::{
        EmbeddingError, EmbeddingInput, EmbeddingProfile, EmbeddingVector, IsolatedEmbeddingGemma2,
        IsolationConfiguration, IsolationError, Ordering, failure,
    };

    const STDERR_CAPTURE_BYTES: usize = 8192;
    const STDERR_DRAIN_BYTES: usize = 64 * 1024;
    const POLL_INTERVAL: Duration = Duration::from_millis(5);
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    const HELPER_DESTINATION: &str = "/worker/vorra-embedding-worker";
    const MODEL_FILES: [&str; 4] = [
        "config.json",
        "tokenizer.json",
        "onnx/model.onnx",
        "onnx/model.onnx_data",
    ];

    pub(super) struct Session {
        process: Option<WorkerProcess>,
        next_id: u64,
    }

    struct Mount {
        source: PathBuf,
        destination: PathBuf,
    }

    struct Sandbox {
        helper: PathBuf,
        libraries: Vec<Mount>,
        library_directory: &'static str,
        loader_directory: &'static str,
    }

    struct PinnedArtifacts {
        mounts: Vec<(OwnedFd, PathBuf)>,
    }

    pub(super) fn load(
        mut configuration: IsolationConfiguration,
        cancellation: &CancellationToken,
    ) -> Result<Arc<IsolatedEmbeddingGemma2>, EmbeddingError> {
        configuration.worker.runtime_sha256.make_ascii_lowercase();
        validate_configuration(&configuration)?;
        checkpoint(cancellation, Instant::now() + PROBE_TIMEOUT)?;
        let sandbox = Sandbox::resolve()?;
        let probe_deadline = Instant::now() + PROBE_TIMEOUT;
        let mut probe = WorkerProcess::spawn(sandbox.command(None)?)?;
        let probe_result = (|| {
            probe.ready(cancellation, probe_deadline)?;
            probe.wait_for_probe_exit(cancellation, probe_deadline)
        })();
        let probe_cleanup = probe.stop();
        match probe_result {
            Err(EmbeddingError::Cancelled) => {
                report_cleanup(probe_cleanup);
                return Err(EmbeddingError::Cancelled);
            }
            Err(_) => {
                report_cleanup(probe_cleanup);
                return Err(failure(IsolationError::Unavailable(
                    "strict namespace probe failed",
                )));
            }
            Ok(()) => {
                let status = probe_cleanup.map_err(|_| {
                    failure(IsolationError::Unavailable(
                        "strict namespace probe cleanup failed",
                    ))
                })?;
                if !status.is_some_and(|status| status.success()) {
                    return Err(failure(IsolationError::Unavailable(
                        "strict namespace probe did not exit successfully",
                    )));
                }
            }
        }
        let timeout = Duration::from_millis(configuration.worker.load_timeout_ms);
        let deadline = Instant::now() + timeout;
        checkpoint(cancellation, deadline)?;
        let artifacts = PinnedArtifacts::open(&configuration)?;
        checkpoint(cancellation, deadline)?;
        let mut process = WorkerProcess::spawn(sandbox.command(Some(artifacts))?)?;
        let initialized = (|| {
            process.ready(cancellation, deadline)?;
            if process.inner.is_none() {
                return Err(failure(IsolationError::Initialization(
                    "worker process identity unavailable",
                )));
            }
            process.send(
                &WorkerRequest::Load {
                    configuration: configuration.worker.clone(),
                },
                cancellation,
                deadline,
            )?;
            match process.receive(cancellation, deadline)? {
                WorkerResponse::Loaded { profile } => {
                    validate_profile(&profile, &configuration.worker)?;
                    Ok(profile)
                }
                WorkerResponse::Failed { failure: response } => {
                    validate_failure(&response.message)?;
                    Err(failure(IsolationError::Initialization(
                        "isolated model initialization was rejected",
                    )))
                }
                _ => Err(failure(IsolationError::Protocol(
                    "expected loaded response",
                ))),
            }
        })();
        let profile = match initialized {
            Ok(profile) => profile,
            Err(error) => {
                report_cleanup(process.stop());
                return Err(error);
            }
        };
        if let Err(error) = checkpoint(cancellation, deadline) {
            report_cleanup(process.stop());
            return Err(error);
        }
        Ok(Arc::new(IsolatedEmbeddingGemma2 {
            profile,
            available: super::AtomicBool::new(true),
            session: Mutex::new(Session {
                process: Some(process),
                next_id: 1,
            }),
            inference_timeout: Duration::from_millis(configuration.worker.inference_timeout_ms),
        }))
    }

    pub(super) fn shutdown(adapter: &IsolatedEmbeddingGemma2) -> Result<(), EmbeddingError> {
        let (mut session, poisoned) = match adapter.session.lock() {
            Ok(session) => (session, false),
            Err(error) => (error.into_inner(), true),
        };
        let process = session.process.take();
        drop(session);
        let cleanup = match process {
            Some(mut process) => process.stop(),
            None => Ok(None),
        };
        if poisoned {
            report_cleanup(cleanup);
            return Err(failure(IsolationError::Poisoned));
        }
        cleanup.map(|_| ())
    }

    pub(super) fn embed(
        adapter: &IsolatedEmbeddingGemma2,
        input: &EmbeddingInput,
        cancellation: &CancellationToken,
    ) -> Result<EmbeddingVector, EmbeddingError> {
        let mut session = match adapter.session.try_lock() {
            Ok(session) => session,
            Err(TryLockError::WouldBlock) => return Err(failure(IsolationError::Busy)),
            Err(TryLockError::Poisoned(error)) => {
                adapter.available.store(false, Ordering::Release);
                let mut session = error.into_inner();
                let process = session.process.take();
                drop(session);
                if let Some(mut process) = process {
                    report_cleanup(process.stop());
                }
                return Err(failure(IsolationError::Poisoned));
            }
        };
        if !adapter.is_available() || session.process.is_none() {
            return Err(failure(IsolationError::Stopped));
        }
        let id = session.next_id;
        let next_id = id.checked_add(1);
        let deadline = Instant::now() + adapter.inference_timeout;
        let result = (|| {
            checkpoint(cancellation, deadline)?;
            let next_id = next_id.ok_or_else(|| {
                failure(IsolationError::Protocol("request identity space exhausted"))
            })?;
            session.next_id = next_id;
            let process = session
                .process
                .as_mut()
                .ok_or_else(|| failure(IsolationError::Stopped))?;
            process.send(
                &WorkerRequest::Embed {
                    id,
                    input: input.clone(),
                },
                cancellation,
                deadline,
            )?;
            match process.receive(cancellation, deadline)? {
                WorkerResponse::Vector {
                    id: response_id,
                    values,
                } if response_id == id => {
                    let vector = EmbeddingVector::new(&adapter.profile, values)?;
                    checkpoint(cancellation, deadline)?;
                    Ok(vector)
                }
                WorkerResponse::Failed { failure: response } => {
                    validate_failure(&response.message)?;
                    Err(failure(IsolationError::Protocol(
                        "isolated inference failed",
                    )))
                }
                _ => Err(failure(IsolationError::Protocol(
                    "unexpected inference response or identity",
                ))),
            }
        })();
        match result {
            Ok(vector) => Ok(vector),
            Err(error) => {
                adapter.available.store(false, Ordering::Release);
                let process = session.process.take();
                drop(session);
                if let Some(mut process) = process {
                    report_cleanup(process.stop());
                }
                Err(error)
            }
        }
    }

    fn validate_configuration(
        configuration: &IsolationConfiguration,
    ) -> Result<(), EmbeddingError> {
        let worker = &configuration.worker;
        if !configuration.runtime_library.is_absolute() || !configuration.model_bundle.is_absolute()
        {
            return Err(failure(IsolationError::Initialization(
                "artifact paths must be absolute",
            )));
        }
        if worker.runtime_sha256.len() != 64
            || !worker
                .runtime_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(failure(IsolationError::Initialization(
                "runtime digest must be lowercase SHA-256",
            )));
        }
        if !SUPPORTED_DIMENSIONS.contains(&worker.dimensions) {
            return Err(EmbeddingError::UnsupportedDimension(worker.dimensions));
        }
        if !(1..=16).contains(&worker.threads)
            || !(8..=8192).contains(&worker.max_tokens)
            || !(1..=300_000).contains(&worker.load_timeout_ms)
            || !(1..=300_000).contains(&worker.inference_timeout_ms)
        {
            return Err(failure(IsolationError::Initialization(
                "worker limits are outside supported bounds",
            )));
        }
        Ok(())
    }

    fn validate_profile(
        profile: &EmbeddingProfile,
        configuration: &WorkerConfiguration,
    ) -> Result<(), EmbeddingError> {
        let spec = profile.spec();
        let preprocessing = spec.preprocessing();
        let revision_prefix =
            format!("source_ref={SOURCE_MODEL_REVISION};onnx={ONNX_MODEL_REVISION};bundle=");
        let revision_suffix = format!(";runtime={}", configuration.runtime_sha256);
        let bundle_digest = spec
            .revision()
            .strip_prefix(&revision_prefix)
            .and_then(|revision| revision.strip_suffix(&revision_suffix));
        let valid_digest = bundle_digest.is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if profile.dimensions() != configuration.dimensions
            || spec.model() != "google/embeddinggemma-2"
            || spec.supported_dimensions() != SUPPORTED_DIMENSIONS
            || spec.normalization() != EmbeddingNormalization::UnitL2
            || spec.metric() != EmbeddingMetric::Cosine
            || preprocessing.revision()
                != "ort-2.0.0-rc.10:tokenizers-0.22.2:fp32:graph-masked-mean-project768:bos-eos:reject-long:truncate-l2-v1"
            || preprocessing.query_template() != "task: search result | query: {text}"
            || preprocessing.document_template() != "title: {title} | text: {text}"
            || preprocessing.missing_title() != "none"
            || !valid_digest
        {
            return Err(failure(IsolationError::Protocol("loaded profile mismatch")));
        }
        Ok(())
    }

    fn validate_failure(message: &str) -> Result<(), EmbeddingError> {
        if message.len() > MAX_ERROR_BYTES {
            return Err(failure(IsolationError::Protocol(
                "oversized failure response",
            )));
        }
        Ok(())
    }

    impl Sandbox {
        fn resolve() -> Result<Self, EmbeddingError> {
            #[cfg(target_arch = "x86_64")]
            let (library_directory, loader_directory, loader) = (
                "/lib/x86_64-linux-gnu",
                "/lib64",
                "/lib64/ld-linux-x86-64.so.2",
            );
            #[cfg(target_arch = "aarch64")]
            let (library_directory, loader_directory, loader) = (
                "/lib/aarch64-linux-gnu",
                "/lib",
                "/lib/ld-linux-aarch64.so.1",
            );
            let executable = std::env::current_exe().map_err(|_| {
                failure(IsolationError::Unavailable(
                    "host executable location unavailable",
                ))
            })?;
            let helper = executable
                .parent()
                .ok_or_else(|| failure(IsolationError::Unavailable("worker location unavailable")))?
                .join("vorra-embedding-worker");
            let helper = canonical_file(&helper)?;
            let mut libraries = Vec::with_capacity(8);
            for name in [
                "libgcc_s.so.1",
                "libm.so.6",
                "libc.so.6",
                "libdl.so.2",
                "librt.so.1",
                "libpthread.so.0",
                "libstdc++.so.6",
            ] {
                let destination = Path::new(library_directory).join(name);
                libraries.push(Mount {
                    source: canonical_file(&destination)?,
                    destination,
                });
            }
            libraries.push(Mount {
                source: canonical_file(Path::new(loader))?,
                destination: PathBuf::from(loader),
            });
            Ok(Self {
                helper,
                libraries,
                library_directory,
                loader_directory,
            })
        }

        fn command(&self, artifacts: Option<PinnedArtifacts>) -> Result<Command, EmbeddingError> {
            let probe = artifacts.is_none();
            let helper = rustix::fs::open(
                &self.helper,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| failure(IsolationError::Unavailable("helper descriptor unavailable")))?;
            let metadata = rustix::fs::fstat(&helper)
                .map_err(|_| failure(IsolationError::Unavailable("helper metadata unavailable")))?;
            if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
                return Err(failure(IsolationError::Unavailable(
                    "helper must be a regular file",
                )));
            }
            let mut mappings = Vec::with_capacity(6);
            mappings.push(FdMapping {
                parent_fd: helper,
                child_fd: 3,
            });
            let mut command = Command::new("/usr/bin/bwrap");
            command
                .env_clear()
                .current_dir("/")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args([
                    "--unshare-user",
                    "--unshare-pid",
                    "--as-pid-1",
                    "--unshare-net",
                    "--unshare-ipc",
                    "--unshare-uts",
                    "--unshare-cgroup",
                    "--disable-userns",
                    "--assert-userns-disabled",
                    "--die-with-parent",
                    "--new-session",
                    "--cap-drop",
                    "ALL",
                    "--clearenv",
                    "--setenv",
                    "HOME",
                    "/tmp",
                    "--setenv",
                    "ORT_DISABLE_TELEMETRY",
                    "1",
                    "--setenv",
                    "LANG",
                    "C",
                    "--dir",
                    "/worker",
                    "--dir",
                    "/runtime",
                    "--dir",
                    "/model",
                    "--dir",
                    "/model/onnx",
                    "--dir",
                    self.library_directory,
                    "--dir",
                    self.loader_directory,
                    "--ro-bind-fd",
                    "3",
                    HELPER_DESTINATION,
                ]);
            for library in &self.libraries {
                command
                    .arg("--ro-bind")
                    .arg(&library.source)
                    .arg(&library.destination);
            }
            if let Some(artifacts) = artifacts {
                for (index, (descriptor, destination)) in artifacts.mounts.into_iter().enumerate() {
                    let child_fd = i32::try_from(index + 4).map_err(|_| {
                        failure(IsolationError::Initialization("artifact descriptor limit"))
                    })?;
                    command
                        .arg("--ro-bind-fd")
                        .arg(child_fd.to_string())
                        .arg(destination);
                    mappings.push(FdMapping {
                        parent_fd: descriptor,
                        child_fd,
                    });
                }
            }
            command.fd_mappings(mappings).map_err(|_| {
                failure(IsolationError::Initialization(
                    "artifact descriptor collision",
                ))
            })?;
            command.args([
                "--size",
                "16777216",
                "--tmpfs",
                "/tmp",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--remount-ro",
                "/",
                "--remount-ro",
                "/proc",
                "--remount-ro",
                "/dev",
                "--chdir",
                "/tmp",
                "--",
                HELPER_DESTINATION,
            ]);
            if probe {
                command.arg("--probe");
            }
            Ok(command)
        }
    }

    impl PinnedArtifacts {
        fn open(configuration: &IsolationConfiguration) -> Result<Self, EmbeddingError> {
            let mut artifacts = Self {
                mounts: Vec::with_capacity(5),
            };
            artifacts.pin(
                &configuration.runtime_library,
                PathBuf::from("/runtime/libonnxruntime.so"),
            )?;
            for file in MODEL_FILES {
                artifacts.pin(
                    &configuration.model_bundle.join(file),
                    Path::new("/model").join(file),
                )?;
            }
            Ok(artifacts)
        }

        fn pin(&mut self, source: &Path, destination: PathBuf) -> Result<(), EmbeddingError> {
            let descriptor = rustix::fs::open(
                source,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| {
                failure(IsolationError::Initialization(
                    "artifact metadata unavailable",
                ))
            })?;
            let metadata = rustix::fs::fstat(&descriptor).map_err(|_| {
                failure(IsolationError::Initialization(
                    "artifact metadata unavailable",
                ))
            })?;
            if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
                return Err(failure(IsolationError::Initialization(
                    "artifact must be a regular file without a final symlink",
                )));
            }
            self.mounts.push((descriptor, destination));
            Ok(())
        }
    }

    fn canonical_file(path: &Path) -> Result<PathBuf, EmbeddingError> {
        let canonical = path.canonicalize().map_err(|_| {
            failure(IsolationError::Unavailable(
                "required helper or ELF file unavailable",
            ))
        })?;
        if !canonical.is_file() {
            return Err(failure(IsolationError::Unavailable(
                "required helper or ELF path is not a file",
            )));
        }
        Ok(canonical)
    }

    struct WorkerProcess {
        child: Option<Child>,
        pid: Pid,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        stderr_capture: Vec<u8>,
        inner: Option<OwnedFd>,
        pipes_nonblocking: bool,
        empty_launcher_killed: bool,
    }

    impl WorkerProcess {
        fn spawn(mut command: Command) -> Result<Self, EmbeddingError> {
            let child = command.spawn().map_err(|_| {
                failure(IsolationError::Unavailable(
                    "isolated worker could not start",
                ))
            })?;
            let pid = Pid::from_child(&child);
            let mut process = Self {
                child: Some(child),
                pid,
                stdin: None,
                stdout: None,
                stderr: None,
                stderr_capture: Vec::with_capacity(STDERR_CAPTURE_BYTES),
                inner: None,
                pipes_nonblocking: false,
                empty_launcher_killed: false,
            };
            let child = process
                .child
                .as_mut()
                .ok_or_else(|| failure(IsolationError::Initialization("worker missing")))?;
            process.stdin = child.stdin.take();
            process.stdout = child.stdout.take();
            process.stderr = child.stderr.take();
            let stdin = process.stdin.as_ref().ok_or_else(|| {
                failure(IsolationError::Initialization("worker input unavailable"))
            })?;
            let stdout = process.stdout.as_ref().ok_or_else(|| {
                failure(IsolationError::Initialization("worker output unavailable"))
            })?;
            let stderr = process.stderr.as_ref().ok_or_else(|| {
                failure(IsolationError::Initialization("worker errors unavailable"))
            })?;
            for pipe in [
                std::os::fd::AsFd::as_fd(stdin),
                std::os::fd::AsFd::as_fd(stdout),
                std::os::fd::AsFd::as_fd(stderr),
            ] {
                let flags =
                    fcntl_getfl(pipe).map_err(|_| failure(IsolationError::Communication))?;
                fcntl_setfl(pipe, flags | OFlags::NONBLOCK)
                    .map_err(|_| failure(IsolationError::Communication))?;
            }
            process.pipes_nonblocking = true;
            Ok(process)
        }

        fn ready(
            &mut self,
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<(), EmbeddingError> {
            match self.receive(cancellation, deadline)? {
                WorkerResponse::Ready { version } if version == PROTOCOL_VERSION => {
                    while self.inner.is_none() {
                        checkpoint(cancellation, deadline)?;
                        self.drain_stderr()?;
                        if self.exited()? {
                            break;
                        }
                        let children = process_children(self.pid)?;
                        if children.len() > 1 {
                            return Err(failure(IsolationError::Protocol(
                                "unexpected worker child count",
                            )));
                        }
                        if let Some(pid) = children.first() {
                            self.inner = self.open_inner(*pid)?;
                        }
                        if self.inner.is_some() || self.exited()? {
                            break;
                        }
                        pause(cancellation, deadline)?;
                    }
                    Ok(())
                }
                _ => Err(failure(IsolationError::Protocol("expected ready response"))),
            }
        }

        fn send(
            &mut self,
            request: &WorkerRequest,
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<(), EmbeddingError> {
            checkpoint(cancellation, deadline)?;
            let payload = serde_json::to_vec(request)
                .map_err(|_| failure(IsolationError::Protocol("request serialization failed")))?;
            if payload.len() > MAX_REQUEST_BYTES {
                return Err(failure(IsolationError::Protocol("request limit exceeded")));
            }
            let length = u32::try_from(payload.len())
                .map_err(|_| failure(IsolationError::Protocol("request length overflow")))?;
            self.write_all(&length.to_be_bytes(), cancellation, deadline)?;
            self.write_all(&payload, cancellation, deadline)
        }

        fn receive(
            &mut self,
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<WorkerResponse, EmbeddingError> {
            let mut header = [0; 4];
            self.read_exact(&mut header, cancellation, deadline)?;
            let length = u32::from_be_bytes(header) as usize;
            if length == 0 || length > MAX_RESPONSE_BYTES {
                return Err(failure(IsolationError::Protocol("response limit exceeded")));
            }
            let mut payload = vec![0; length];
            self.read_exact(&mut payload, cancellation, deadline)?;
            serde_json::from_slice(&payload)
                .map_err(|_| failure(IsolationError::Protocol("invalid response frame")))
        }

        fn write_all(
            &mut self,
            mut bytes: &[u8],
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<(), EmbeddingError> {
            while !bytes.is_empty() {
                checkpoint(cancellation, deadline)?;
                self.drain_stderr()?;
                let result = self
                    .stdin
                    .as_mut()
                    .ok_or_else(|| failure(IsolationError::Communication))?
                    .write(bytes);
                match result {
                    Ok(0) => return Err(failure(IsolationError::Communication)),
                    Ok(written) => bytes = &bytes[written..],
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        pause(cancellation, deadline)?;
                    }
                    Err(_) => return Err(failure(IsolationError::Communication)),
                }
            }
            checkpoint(cancellation, deadline)
        }

        fn read_exact(
            &mut self,
            mut bytes: &mut [u8],
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<(), EmbeddingError> {
            while !bytes.is_empty() {
                checkpoint(cancellation, deadline)?;
                self.drain_stderr()?;
                let result = self
                    .stdout
                    .as_mut()
                    .ok_or_else(|| failure(IsolationError::Communication))?
                    .read(bytes);
                match result {
                    Ok(0) => return Err(failure(IsolationError::Communication)),
                    Ok(read) => bytes = &mut bytes[read..],
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        pause(cancellation, deadline)?;
                    }
                    Err(_) => return Err(failure(IsolationError::Communication)),
                }
            }
            checkpoint(cancellation, deadline)
        }

        fn drain_stderr(&mut self) -> Result<(), EmbeddingError> {
            if !self.pipes_nonblocking {
                return Ok(());
            }
            let mut buffer = [0; 4096];
            let mut drained = 0;
            while drained < STDERR_DRAIN_BYTES {
                let Some(stderr) = self.stderr.as_mut() else {
                    break;
                };
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        drained += count;
                        let capture = count.min(STDERR_CAPTURE_BYTES - self.stderr_capture.len());
                        self.stderr_capture.extend_from_slice(&buffer[..capture]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return Err(failure(IsolationError::Communication)),
                }
            }
            Ok(())
        }

        fn open_inner(&self, pid: Pid) -> Result<Option<OwnedFd>, EmbeddingError> {
            if self.exited()? {
                return Ok(None);
            }
            let Some(identity) = process_identity(pid)? else {
                return Ok(None);
            };
            if identity.0 != self.pid.as_raw_pid() {
                return Err(failure(IsolationError::Protocol(
                    "worker parent identity mismatch",
                )));
            }
            let descriptor = match rustix::process::pidfd_open(pid, PidfdFlags::empty()) {
                Ok(descriptor) => descriptor,
                Err(rustix::io::Errno::SRCH) => return Ok(None),
                Err(_) => {
                    return Err(failure(IsolationError::Unavailable(
                        "owned worker pidfd unavailable",
                    )));
                }
            };
            if process_identity(pid)? != Some(identity) || self.exited()? {
                return Ok(None);
            }
            Ok(Some(descriptor))
        }

        fn exited(&self) -> Result<bool, EmbeddingError> {
            loop {
                match rustix::process::waitid(
                    WaitId::Pid(self.pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(status) => return Ok(status.is_some()),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => return Err(failure(IsolationError::Cleanup)),
                }
            }
        }

        fn stop_inner(&mut self) -> Result<(), EmbeddingError> {
            if self.exited()? {
                return Ok(());
            }
            if let Some(inner) = &self.inner {
                return signal_pidfd(inner, Signal::KILL);
            }
            rustix::process::kill_process(self.pid, Signal::STOP)
                .map_err(|_| failure(IsolationError::Cleanup))?;
            let stop_deadline = Instant::now() + PROBE_TIMEOUT;
            loop {
                match rustix::process::waitid(
                    WaitId::Pid(self.pid),
                    WaitIdOptions::EXITED
                        | WaitIdOptions::STOPPED
                        | WaitIdOptions::NOWAIT
                        | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(status)) if status.stopped() => break,
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) | Err(rustix::io::Errno::INTR) => {
                        if Instant::now() >= stop_deadline {
                            if !self.exited()? {
                                let resumed = rustix::process::kill_process(self.pid, Signal::CONT);
                                if !matches!(resumed, Ok(()) | Err(rustix::io::Errno::SRCH)) {
                                    return Err(failure(IsolationError::Cleanup));
                                }
                            }
                            return Err(failure(IsolationError::Cleanup));
                        }
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    Err(_) => return Err(failure(IsolationError::Cleanup)),
                }
            }
            let result = (|| {
                let children = process_children(self.pid)?;
                if children.is_empty() {
                    self.empty_launcher_killed = true;
                    let group = rustix::process::kill_process_group(self.pid, Signal::KILL);
                    let leader = rustix::process::kill_process(self.pid, Signal::KILL);
                    for result in [group, leader] {
                        if !matches!(result, Ok(()) | Err(rustix::io::Errno::SRCH)) {
                            return Err(failure(IsolationError::Cleanup));
                        }
                    }
                    return Ok(());
                }
                for pid in children {
                    let descriptor = self.open_inner(pid)?;
                    if let Some(descriptor) = descriptor {
                        signal_pidfd(&descriptor, Signal::KILL)?;
                    }
                }
                Ok(())
            })();
            if !self.exited()? {
                let resumed = rustix::process::kill_process(self.pid, Signal::CONT);
                if !matches!(resumed, Ok(()) | Err(rustix::io::Errno::SRCH)) {
                    return Err(failure(IsolationError::Cleanup));
                }
            }
            result
        }

        fn wait_for_probe_exit(
            &mut self,
            cancellation: &CancellationToken,
            deadline: Instant,
        ) -> Result<(), EmbeddingError> {
            self.stdin.take();
            loop {
                checkpoint(cancellation, deadline)?;
                self.drain_stderr()?;
                let mut extra = [0; 1];
                match self
                    .stdout
                    .as_mut()
                    .ok_or_else(|| failure(IsolationError::Communication))?
                    .read(&mut extra)
                {
                    Ok(1..) => {
                        return Err(failure(IsolationError::Protocol("unexpected probe output")));
                    }
                    Ok(0) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(failure(IsolationError::Communication)),
                }
                match rustix::process::waitid(
                    WaitId::Pid(self.pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) | Err(rustix::io::Errno::INTR) => pause(cancellation, deadline)?,
                    Err(_) => return Err(failure(IsolationError::Communication)),
                }
            }
        }

        fn stop(&mut self) -> Result<Option<ExitStatus>, EmbeddingError> {
            let Some(mut child) = self.child.take() else {
                return Ok(None);
            };
            self.stdin.take();
            if !self.pipes_nonblocking {
                self.stdout.take();
                self.stderr.take();
            }
            let mut cleanup_failed = self.stop_inner().is_err();
            loop {
                if self.drain_stderr().is_err() {
                    cleanup_failed = true;
                }
                match rustix::process::waitid(
                    WaitId::Pid(self.pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(_)) => break,
                    Ok(None) | Err(rustix::io::Errno::INTR) => {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    Err(_) => {
                        cleanup_failed = true;
                        break;
                    }
                }
            }
            let status = loop {
                match child.wait() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => break result,
                }
            };
            self.stdout.take();
            self.stderr.take();
            self.inner.take();
            match status {
                Ok(status)
                    if !cleanup_failed
                        && (status.signal().is_none() || self.empty_launcher_killed) =>
                {
                    Ok(Some(status))
                }
                _ => Err(failure(IsolationError::Cleanup)),
            }
        }
    }

    impl Drop for WorkerProcess {
        fn drop(&mut self) {
            if self.child.is_some() {
                report_cleanup(self.stop());
            }
        }
    }

    fn process_identity(pid: Pid) -> Result<Option<(i32, u64)>, EmbeddingError> {
        let path = format!("/proc/{}/stat", pid.as_raw_pid());
        let Some(bytes) = read_proc_file(&path)? else {
            return Ok(None);
        };
        let name_end = bytes
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(|| failure(IsolationError::Cleanup))?;
        let suffix = std::str::from_utf8(&bytes[name_end + 1..])
            .map_err(|_| failure(IsolationError::Cleanup))?;
        let fields: Vec<_> = suffix.split_ascii_whitespace().collect();
        let parent = fields
            .get(1)
            .and_then(|value| value.parse::<i32>().ok())
            .ok_or_else(|| failure(IsolationError::Cleanup))?;
        let start = fields
            .get(19)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| failure(IsolationError::Cleanup))?;
        Ok(Some((parent, start)))
    }

    fn process_children(pid: Pid) -> Result<Vec<Pid>, EmbeddingError> {
        let path = format!(
            "/proc/{}/task/{}/children",
            pid.as_raw_pid(),
            pid.as_raw_pid()
        );
        let Some(bytes) = read_proc_file(&path)? else {
            return scan_process_children(pid);
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| failure(IsolationError::Cleanup))?;
        let mut children = Vec::new();
        for value in text.split_ascii_whitespace() {
            if children.len() == 16 {
                return Err(failure(IsolationError::Cleanup));
            }
            let child = value
                .parse::<i32>()
                .ok()
                .and_then(Pid::from_raw)
                .ok_or_else(|| failure(IsolationError::Cleanup))?;
            children.push(child);
        }
        Ok(children)
    }

    fn scan_process_children(parent: Pid) -> Result<Vec<Pid>, EmbeddingError> {
        let entries = std::fs::read_dir("/proc").map_err(|_| failure(IsolationError::Cleanup))?;
        let mut children = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index >= 8192 {
                return Err(failure(IsolationError::Cleanup));
            }
            let entry = entry.map_err(|_| failure(IsolationError::Cleanup))?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .and_then(Pid::from_raw)
            else {
                continue;
            };
            if process_identity(pid)?.is_some_and(|identity| identity.0 == parent.as_raw_pid()) {
                if children.len() == 16 {
                    return Err(failure(IsolationError::Cleanup));
                }
                children.push(pid);
            }
        }
        Ok(children)
    }

    fn read_proc_file(path: &str) -> Result<Option<Vec<u8>>, EmbeddingError> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(failure(IsolationError::Cleanup)),
        };
        let mut bytes = Vec::new();
        file.take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| failure(IsolationError::Cleanup))?;
        if bytes.len() > 8192 {
            return Err(failure(IsolationError::Cleanup));
        }
        Ok(Some(bytes))
    }

    fn signal_pidfd(descriptor: &OwnedFd, signal: Signal) -> Result<(), EmbeddingError> {
        loop {
            match rustix::process::pidfd_send_signal(descriptor, signal) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(failure(IsolationError::Cleanup)),
            }
        }
    }

    fn checkpoint(
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), EmbeddingError> {
        if cancellation.is_cancelled() {
            return Err(EmbeddingError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(failure(IsolationError::Deadline));
        }
        Ok(())
    }

    fn pause(cancellation: &CancellationToken, deadline: Instant) -> Result<(), EmbeddingError> {
        checkpoint(cancellation, deadline)?;
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        checkpoint(cancellation, deadline)
    }

    fn report_cleanup(result: Result<Option<ExitStatus>, EmbeddingError>) {
        if result.is_err() {
            tracing::error!("embedding worker cleanup failed");
        }
    }
}
