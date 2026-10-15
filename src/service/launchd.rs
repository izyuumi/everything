use std::path::{Path, PathBuf};

use plist::{Dictionary, Value};
use tokio_util::sync::CancellationToken;

use super::ServiceError;

const SERVICE_LABEL: &str = "com.everything.host";

#[derive(Debug, Clone)]
pub struct LaunchdService {
    executable: PathBuf,
    state_directory: PathBuf,
    launch_agents_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
    pub process_id: Option<u32>,
    pub last_exit_status: Option<i32>,
}

impl LaunchdService {
    pub fn new(
        executable: impl Into<PathBuf>,
        state_directory: impl Into<PathBuf>,
        launch_agents_directory: impl Into<PathBuf>,
    ) -> Result<Self, ServiceError> {
        let service = Self {
            executable: executable.into(),
            state_directory: state_directory.into(),
            launch_agents_directory: launch_agents_directory.into(),
        };
        for path in [
            &service.executable,
            &service.state_directory,
            &service.launch_agents_directory,
        ] {
            validate_path(path)?;
        }
        Ok(service)
    }

    pub fn label(&self) -> &'static str {
        SERVICE_LABEL
    }

    pub fn plist_path(&self) -> PathBuf {
        self.launch_agents_directory
            .join(format!("{SERVICE_LABEL}.plist"))
    }

    pub fn render_plist(&self) -> Result<Vec<u8>, ServiceError> {
        let mut dictionary = Dictionary::new();
        dictionary.insert("Label".to_owned(), Value::String(SERVICE_LABEL.to_owned()));
        dictionary.insert(
            "ProgramArguments".to_owned(),
            Value::Array(vec![
                Value::String(path_string(&self.executable)?.to_owned()),
                Value::String("--state-dir".to_owned()),
                Value::String(path_string(&self.state_directory)?.to_owned()),
                Value::String("serve".to_owned()),
            ]),
        );
        dictionary.insert(
            "WorkingDirectory".to_owned(),
            Value::String(path_string(&self.state_directory)?.to_owned()),
        );
        dictionary.insert("RunAtLoad".to_owned(), Value::Boolean(true));
        dictionary.insert("KeepAlive".to_owned(), Value::Boolean(true));
        dictionary.insert(
            "ProcessType".to_owned(),
            Value::String("Background".to_owned()),
        );
        dictionary.insert("ThrottleInterval".to_owned(), Value::Integer(10u64.into()));
        dictionary.insert("ExitTimeOut".to_owned(), Value::Integer(20u64.into()));
        dictionary.insert("Umask".to_owned(), Value::Integer(0o077u64.into()));
        dictionary.insert(
            "StandardOutPath".to_owned(),
            Value::String(path_string(&self.state_directory.join("host.stdout.log"))?.to_owned()),
        );
        dictionary.insert(
            "StandardErrorPath".to_owned(),
            Value::String(path_string(&self.state_directory.join("host.stderr.log"))?.to_owned()),
        );
        let mut bytes = Vec::new();
        Value::Dictionary(dictionary).to_writer_xml(&mut bytes)?;
        Ok(bytes)
    }

    pub async fn install(&self, cancellation: CancellationToken) -> Result<PathBuf, ServiceError> {
        #[cfg(target_os = "macos")]
        {
            if cancellation.is_cancelled() {
                return Err(ServiceError::Cancelled);
            }
            let service = self.clone();
            let task = tokio::task::spawn_blocking(move || service.install_file());
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ServiceError::Cancelled),
                result = task => result.map_err(|_| ServiceError::InvalidResponse("installer worker failed"))?,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cancellation;
            Err(ServiceError::UnsupportedPlatform)
        }
    }

    pub async fn start(&self, cancellation: CancellationToken) -> Result<(), ServiceError> {
        #[cfg(target_os = "macos")]
        {
            let status = self.status(cancellation.clone()).await?;
            if !status.installed {
                return Err(ServiceError::UnsafePath(
                    "service must be installed before starting",
                ));
            }
            if status.process_id.is_some() {
                return Ok(());
            }
            self.verify_executable()?;
            super::state::StateDirectory::open(&self.state_directory, false)?;
            if !status.loaded {
                self.require_success(
                    "bootstrap",
                    vec![
                        "bootstrap".to_owned(),
                        user_domain(),
                        path_string(&self.plist_path())?.to_owned(),
                    ],
                    cancellation.clone(),
                )
                .await?;
            }
            self.require_success(
                "kickstart",
                vec!["kickstart".to_owned(), service_target()],
                cancellation,
            )
            .await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cancellation;
            Err(ServiceError::UnsupportedPlatform)
        }
    }

    pub async fn stop(&self, cancellation: CancellationToken) -> Result<(), ServiceError> {
        #[cfg(target_os = "macos")]
        {
            let output =
                run_launchctl(vec!["bootout".to_owned(), service_target()], cancellation).await?;
            if output.status.success() || output.status.code() == Some(113) {
                Ok(())
            } else {
                Err(ServiceError::Launchctl {
                    operation: "bootout",
                    status: output.status.code(),
                })
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cancellation;
            Err(ServiceError::UnsupportedPlatform)
        }
    }

    pub async fn set_enabled(
        &self,
        enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<(), ServiceError> {
        #[cfg(target_os = "macos")]
        {
            if enabled {
                self.verify_executable()?;
                if !self.installed_plist()? {
                    return Err(ServiceError::UnsafePath(
                        "service must be installed before enabling",
                    ));
                }
                super::state::StateDirectory::open(&self.state_directory, false)?;
            }
            let operation = if enabled { "enable" } else { "disable" };
            self.require_success(
                operation,
                vec![operation.to_owned(), service_target()],
                cancellation,
            )
            .await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (enabled, cancellation);
            Err(ServiceError::UnsupportedPlatform)
        }
    }

    pub async fn status(
        &self,
        cancellation: CancellationToken,
    ) -> Result<ServiceStatus, ServiceError> {
        #[cfg(target_os = "macos")]
        {
            let installed = self.installed_plist()?;
            let output =
                run_launchctl(vec!["print".to_owned(), service_target()], cancellation).await?;
            if output.status.code() == Some(113) {
                return Ok(ServiceStatus {
                    installed,
                    loaded: false,
                    process_id: None,
                    last_exit_status: None,
                });
            }
            if !output.status.success() {
                return Err(ServiceError::Launchctl {
                    operation: "print",
                    status: output.status.code(),
                });
            }
            let text = std::str::from_utf8(&output.stdout)
                .map_err(|_| ServiceError::InvalidResponse("launchctl output is not UTF-8"))?;
            let mut process_id = None;
            let mut last_exit_status = None;
            for line in text.lines() {
                if let Some(value) = line.trim().strip_prefix("pid = ") {
                    process_id = Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| ServiceError::InvalidResponse("invalid process ID"))?,
                    );
                }
                if let Some(value) = line.trim().strip_prefix("last exit code = ") {
                    last_exit_status = Some(
                        value
                            .parse::<i32>()
                            .map_err(|_| ServiceError::InvalidResponse("invalid exit code"))?,
                    );
                }
            }
            Ok(ServiceStatus {
                installed,
                loaded: true,
                process_id,
                last_exit_status,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cancellation;
            Err(ServiceError::UnsupportedPlatform)
        }
    }

    #[cfg(target_os = "macos")]
    fn install_file(&self) -> Result<PathBuf, ServiceError> {
        use super::state::StateDirectory;
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};
        use std::io::Write;
        self.verify_executable()?;
        let uid = rustix::process::geteuid().as_raw();
        let _state = StateDirectory::open(&self.state_directory, true)?;
        let agents = StateDirectory::open_shared(&self.launch_agents_directory, true)?;
        let file_name = format!("{SERVICE_LABEL}.plist");
        match rustix::fs::statat(&agents.file, &file_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata)
                if FileType::from_raw_mode(metadata.st_mode) == FileType::RegularFile
                    && metadata.st_uid == uid
                    && metadata.st_nlink == 1 => {}
            Ok(_) => {
                return Err(ServiceError::UnsafePath(
                    "existing service property list is unsafe",
                ));
            }
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Err(error) => return Err(error.into()),
        }
        let temporary_name = format!(".{SERVICE_LABEL}.{}.tmp", uuid::Uuid::new_v4());
        let descriptor = rustix::fs::openat(
            &agents.file,
            &temporary_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let result = (|| {
            let mut file = std::fs::File::from(descriptor);
            file.write_all(&self.render_plist()?)?;
            file.sync_all()?;
            rustix::fs::renameat(&agents.file, &temporary_name, &agents.file, &file_name)?;
            agents.file.sync_all()?;
            Ok(self.plist_path())
        })();
        if result.is_err() {
            match rustix::fs::unlinkat(&agents.file, &temporary_name, AtFlags::empty()) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::NOENT => {}
                Err(error) => {
                    tracing::warn!(error = %error, "could not remove failed service installation temporary file")
                }
            }
        }
        result
    }

    #[cfg(target_os = "macos")]
    fn verify_executable(&self) -> Result<(), ServiceError> {
        use rustix::fs::{FileType, Mode, OFlags};
        let parent = self.executable.parent().ok_or(ServiceError::UnsafePath(
            "executable has no parent directory",
        ))?;
        let directory = super::state::StateDirectory::open_trusted(parent)?;
        let name = self
            .executable
            .file_name()
            .ok_or(ServiceError::UnsafePath("executable has no filename"))?;
        let executable = rustix::fs::openat(
            &directory.file,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        let metadata = rustix::fs::fstat(&executable)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || (metadata.st_uid != directory.uid && metadata.st_uid != 0)
            || metadata.st_mode & 0o022 != 0
            || metadata.st_mode & 0o111 == 0
        {
            return Err(ServiceError::UnsafePath(
                "host executable must be a trusted executable regular file",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn installed_plist(&self) -> Result<bool, ServiceError> {
        use rustix::fs::{AtFlags, FileType};
        let directory =
            match super::state::StateDirectory::open_shared(&self.launch_agents_directory, false) {
                Ok(directory) => directory,
                Err(ServiceError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(false);
                }
                Err(ServiceError::Filesystem(error)) if error == rustix::io::Errno::NOENT => {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
        match rustix::fs::statat(
            &directory.file,
            format!("{SERVICE_LABEL}.plist"),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(metadata) => {
                if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
                    || metadata.st_uid != directory.uid
                    || metadata.st_mode & 0o022 != 0
                    || metadata.st_nlink != 1
                {
                    return Err(ServiceError::UnsafePath(
                        "installed service property list is not a trusted regular file",
                    ));
                }
                Ok(true)
            }
            Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(target_os = "macos")]
    async fn require_success(
        &self,
        operation: &'static str,
        arguments: Vec<String>,
        cancellation: CancellationToken,
    ) -> Result<(), ServiceError> {
        let output = run_launchctl(arguments, cancellation).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ServiceError::Launchctl {
                operation,
                status: output.status.code(),
            })
        }
    }
}

fn validate_path(path: &Path) -> Result<(), ServiceError> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ServiceError::UnsafePath(
            "service paths must be absolute and cannot contain parent traversal",
        ));
    }
    path_string(path)?;
    Ok(())
}

fn path_string(path: &Path) -> Result<&str, ServiceError> {
    let value = path.to_str().ok_or(ServiceError::UnsafePath(
        "property list paths must be valid UTF-8",
    ))?;
    if value.chars().any(char::is_control) || value.len() > 4_096 {
        return Err(ServiceError::UnsafePath(
            "service paths must be bounded and contain no controls",
        ));
    }
    Ok(value)
}

#[cfg(target_os = "macos")]
fn user_domain() -> String {
    format!("gui/{}", rustix::process::geteuid().as_raw())
}

#[cfg(target_os = "macos")]
fn service_target() -> String {
    format!("{}/{SERVICE_LABEL}", user_domain())
}

#[cfg(target_os = "macos")]
async fn run_launchctl(
    arguments: Vec<String>,
    cancellation: CancellationToken,
) -> Result<std::process::Output, ServiceError> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    if cancellation.is_cancelled() {
        return Err(ServiceError::Cancelled);
    }
    if rustix::process::geteuid() != rustix::process::getuid() {
        return Err(ServiceError::UnsafePath("setuid execution is unsupported"));
    }
    let mut child = tokio::process::Command::new("/bin/launchctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().ok_or(ServiceError::InvalidResponse(
        "launchctl has no stdout pipe",
    ))?;
    let stderr = child.stderr.take().ok_or(ServiceError::InvalidResponse(
        "launchctl has no stderr pipe",
    ))?;
    let read = |reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>| async move {
        let mut bytes = Vec::new();
        reader.take(65_537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65_536 {
            return Err(std::io::Error::other("launchctl output exceeded its limit"));
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let operation = async {
        let (stdout, stderr, status) =
            tokio::try_join!(read(Box::new(stdout)), read(Box::new(stderr)), child.wait())?;
        Ok::<_, ServiceError>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ServiceError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(10), operation) => result.map_err(|_| ServiceError::Timeout)?,
    }
}
