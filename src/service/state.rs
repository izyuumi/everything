use std::fs::File;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use super::ServiceError;

enum DirectoryPolicy {
    Private,
    #[cfg(target_os = "macos")]
    Shared,
    #[cfg(target_os = "macos")]
    Trusted,
}

impl DirectoryPolicy {
    fn accepts_root(&self) -> bool {
        match self {
            Self::Private => false,
            #[cfg(target_os = "macos")]
            Self::Shared => false,
            #[cfg(target_os = "macos")]
            Self::Trusted => true,
        }
    }
}

pub(crate) struct StateDirectory {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    pub(crate) uid: u32,
}

pub struct StateLease {
    pub(crate) directory: Arc<StateDirectory>,
    _lock: File,
}

impl StateLease {
    pub async fn acquire(path: impl AsRef<Path>) -> Result<Arc<Self>, ServiceError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let directory = StateDirectory::open(&path, true)?;
            let lock = directory.lock()?;
            Ok(Arc::new(Self {
                directory: Arc::new(directory),
                _lock: lock,
            }))
        })
        .await
        .map_err(|_| ServiceError::Worker)?
    }

    pub fn path(&self) -> &Path {
        &self.directory.path
    }

    pub(crate) fn validate_database(&self) -> Result<File, ServiceError> {
        let descriptor = rustix::fs::openat(
            &self.directory.file,
            "everything.sqlite3",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?;
        let metadata = rustix::fs::fstat(&descriptor)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || metadata.st_uid != self.directory.uid
            || metadata.st_mode & 0o7777 != 0o600
            || metadata.st_nlink != 1
        {
            return Err(ServiceError::UnsafePath(
                "host database must be an owner-only single-link regular file",
            ));
        }
        Ok(File::from(descriptor))
    }
}

impl StateDirectory {
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self, ServiceError> {
        Self::open_directory(path, create, DirectoryPolicy::Private)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_shared(path: &Path, create: bool) -> Result<Self, ServiceError> {
        Self::open_directory(path, create, DirectoryPolicy::Shared)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_trusted(path: &Path) -> Result<Self, ServiceError> {
        Self::open_directory(path, false, DirectoryPolicy::Trusted)
    }

    fn open_directory(
        path: &Path,
        create: bool,
        policy: DirectoryPolicy,
    ) -> Result<Self, ServiceError> {
        if path.as_os_str().as_bytes().len() > 4_096 {
            return Err(ServiceError::UnsafePath(
                "directory path exceeds its byte limit",
            ));
        }
        if !path.is_absolute() || (path == Path::new("/") && !policy.accepts_root()) {
            return Err(ServiceError::UnsafePath(
                "state directory must be an absolute nonroot path",
            ));
        }
        let uid = rustix::process::geteuid().as_raw();
        if uid != rustix::process::getuid().as_raw() {
            return Err(ServiceError::UnsafePath("setuid execution is unsupported"));
        }
        let components: Vec<_> = path.components().collect();
        let mut directory = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for (index, component) in components.iter().enumerate().skip(1) {
            let Component::Normal(name) = component else {
                return Err(ServiceError::UnsafePath(
                    "relative or parent path components are forbidden",
                ));
            };
            let final_component = index + 1 == components.len();
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let next = match rustix::fs::openat(&directory, *name, flags, Mode::empty()) {
                Ok(next) => next,
                Err(error) if error == rustix::io::Errno::NOENT && create => {
                    match rustix::fs::mkdirat(&directory, *name, Mode::from_raw_mode(0o700)) {
                        Ok(()) => {}
                        Err(error) if error == rustix::io::Errno::EXIST => {}
                        Err(error) => return Err(io::Error::from(error).into()),
                    }
                    rustix::fs::openat(&directory, *name, flags, Mode::empty())?
                }
                Err(error) => return Err(io::Error::from(error).into()),
            };
            let metadata = rustix::fs::fstat(&next)?;
            if metadata.st_uid != uid && metadata.st_uid != 0 {
                return Err(ServiceError::UnsafePath(
                    "directory ancestors must belong to this user or root",
                ));
            }
            if !final_component && metadata.st_mode & 0o022 != 0 && metadata.st_mode & 0o1000 == 0 {
                return Err(ServiceError::UnsafePath(
                    "writable directory ancestors require the sticky bit",
                ));
            }
            directory = next;
        }
        let metadata = rustix::fs::fstat(&directory)?;
        let trusted = match policy {
            DirectoryPolicy::Private => {
                metadata.st_uid == uid && metadata.st_mode & 0o7777 == 0o700
            }
            #[cfg(target_os = "macos")]
            DirectoryPolicy::Shared => metadata.st_uid == uid && metadata.st_mode & 0o022 == 0,
            #[cfg(target_os = "macos")]
            DirectoryPolicy::Trusted => {
                (metadata.st_uid == uid || metadata.st_uid == 0)
                    && (metadata.st_mode & 0o022 == 0 || metadata.st_mode & 0o1000 != 0)
            }
        };
        if !trusted {
            return Err(ServiceError::UnsafePath(
                "directory ownership or permissions do not satisfy its trust policy",
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            file: File::from(directory),
            uid,
        })
    }

    pub(crate) fn lock(&self) -> Result<File, ServiceError> {
        let descriptor = rustix::fs::openat(
            &self.file,
            "host.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?;
        let metadata = rustix::fs::fstat(&descriptor)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || metadata.st_uid != self.uid
            || metadata.st_mode & 0o7777 != 0o600
            || metadata.st_nlink != 1
        {
            return Err(ServiceError::UnsafePath(
                "host lock must be a private single-link regular file",
            ));
        }
        let mut file = File::from(descriptor);
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                ServiceError::AlreadyRunning
            } else {
                ServiceError::Io(error)
            }
        })?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_data()?;
        Ok(file)
    }

    pub(crate) fn socket_metadata(&self) -> Result<Option<rustix::fs::Stat>, ServiceError> {
        match rustix::fs::statat(&self.file, "host.sock", AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => {
                if FileType::from_raw_mode(metadata.st_mode) != FileType::Socket
                    || metadata.st_uid != self.uid
                    || metadata.st_mode & 0o7777 != 0o600
                {
                    return Err(ServiceError::UnsafePath(
                        "host socket must be a private socket owned by this user",
                    ));
                }
                Ok(Some(metadata))
            }
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(error) => Err(io::Error::from(error).into()),
        }
    }

    pub(crate) fn unlink_socket(&self) -> Result<(), ServiceError> {
        rustix::fs::unlinkat(&self.file, "host.sock", AtFlags::empty())?;
        Ok(())
    }
}
