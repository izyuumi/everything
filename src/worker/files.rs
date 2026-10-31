use std::path::{Path, PathBuf};

use crate::agent::Agent;

use super::{WorkerError, WorkerFile, WorkerManifest, WorkerResult};

#[derive(Clone, Debug)]
pub struct QuarantinedArtifacts {
    pub directory: PathBuf,
    pub files: Vec<String>,
    pub result_digest: String,
}

#[cfg(unix)]
pub use unix::{capture_inputs, collect_artifacts, prepare_workspace, quarantine_result};

#[cfg(not(unix))]
pub fn capture_inputs(_: &Agent, _: &Path, _: &[String]) -> Result<Vec<WorkerFile>, WorkerError> {
    Err(WorkerError::Unsupported(
        "worker files require Unix descriptors",
    ))
}

#[cfg(not(unix))]
pub fn prepare_workspace(_: &Agent, _: &WorkerManifest) -> Result<PathBuf, WorkerError> {
    Err(WorkerError::Unsupported(
        "worker files require Unix descriptors",
    ))
}

#[cfg(not(unix))]
pub fn collect_artifacts(_: &Agent, _: &WorkerManifest) -> Result<Vec<WorkerFile>, WorkerError> {
    Err(WorkerError::Unsupported(
        "worker files require Unix descriptors",
    ))
}

#[cfg(not(unix))]
pub fn quarantine_result(
    _: &Agent,
    _: &WorkerManifest,
    _: &WorkerResult,
) -> Result<QuarantinedArtifacts, WorkerError> {
    Err(WorkerError::Unsupported(
        "worker files require Unix descriptors",
    ))
}

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Component;
    use std::time::{Duration, Instant};

    use cap_std::ambient_authority;
    use cap_std::fs::{Dir, Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
    use rustix::fs::{Mode, OFlags};

    use super::*;
    use crate::worker::types::{MAX_WORKER_FILE_BYTES, MAX_WORKER_FILES, relative_path};

    const MAX_STATE_ENTRIES: usize = 16_384;
    const MAX_AREA_DIRECTORIES: usize = 512;
    const MAX_TREE_ENTRIES: usize = 1_088;
    const MAX_RECORD_BYTES: usize = 512 * 1024;
    const SCAN_DURATION: Duration = Duration::from_secs(5);

    type Identity = (u64, u64);
    type Contents = BTreeMap<String, Vec<u8>>;
    type Result<T> = std::result::Result<T, WorkerError>;

    struct State {
        directory: Dir,
        path: PathBuf,
        uid: u32,
        database: Identity,
    }

    struct Area {
        directory: Dir,
        path: PathBuf,
        remaining: usize,
        _lock: File,
    }

    #[derive(Clone, Copy)]
    enum AreaKind {
        Jobs,
        Results,
    }

    impl State {
        fn open(agent: &Agent) -> Result<Self> {
            let path = agent.state_directory().ok_or(WorkerError::LeaseRequired)?;
            let state = crate::service::state::StateDirectory::open(path, false)
                .map_err(crate::agent::AgentError::from)?;
            let directory = Dir::from_std_file(state.file);
            let database = directory.symlink_metadata("vorra.sqlite3")?;
            regular(&database)?;
            if database.uid() != state.uid || database.mode() & 0o7777 != 0o600 {
                return Err(WorkerError::Invalid(
                    "worker administrative files must be owner-only",
                ));
            }
            Ok(Self {
                database: identity(&database),
                directory,
                path: state.path,
                uid: state.uid,
            })
        }

        fn area(&self, kind: AreaKind) -> Result<Area> {
            let name = match kind {
                AreaKind::Jobs => "worker-jobs",
                AreaKind::Results => "worker-results",
            };
            let directory = private_directory(&self.directory, OsStr::new(name), self.uid, true)?;
            let lock = private_file(&directory, OsStr::new(".lock"), self.uid, true)?.into_std();
            fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    WorkerError::Conflict("worker file area is busy")
                } else {
                    WorkerError::Io(error)
                }
            })?;
            let mut count = 0;
            let mut visited = 0;
            count_area(
                &directory,
                kind,
                0,
                self.uid,
                &mut count,
                &mut visited,
                Instant::now(),
            )?;
            Ok(Area {
                directory,
                path: self.path.join(name),
                remaining: MAX_AREA_DIRECTORIES.saturating_sub(count),
                _lock: lock,
            })
        }
    }

    pub fn capture_inputs(
        agent: &Agent,
        workspace: &Path,
        paths: &[String],
    ) -> Result<Vec<WorkerFile>> {
        if paths.len() > MAX_WORKER_FILES {
            return Err(WorkerError::Capacity("input file count"));
        }
        let mut unique = BTreeSet::new();
        for path in paths {
            relative_path(path)?;
            if !unique.insert(path) {
                return Err(WorkerError::Conflict("duplicate input path"));
            }
        }
        let state = State::open(agent)?;
        let mut protected = BTreeSet::from([state.database]);
        let mut visited = 0;
        inventory(
            &state.directory,
            0,
            &mut visited,
            &mut protected,
            Instant::now(),
        )?;
        if workspace.starts_with(&state.path) {
            return Err(WorkerError::Invalid("Agent state cannot be a worker input"));
        }
        let canonical = std::fs::canonicalize(workspace)?;
        if canonical.starts_with(&state.path) {
            return Err(WorkerError::Invalid("Agent state cannot be a worker input"));
        }
        let directory = source_root(workspace, &protected)?;
        let mut bytes = 0;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let contents = selected_file(
                &directory,
                path,
                MAX_WORKER_FILE_BYTES - bytes,
                Some(&protected),
            )?
            .ok_or(WorkerError::Invalid("selected input file is missing"))?;
            bytes = bytes
                .checked_add(contents.len())
                .ok_or(WorkerError::Capacity("input bytes"))?;
            files.push(WorkerFile::from_bytes(path.clone(), &contents)?);
        }
        Ok(files)
    }

    pub fn prepare_workspace(agent: &Agent, manifest: &WorkerManifest) -> Result<PathBuf> {
        manifest.validate()?;
        let inputs = decode_files(&manifest.inputs)?;
        let state = State::open(agent)?;
        let mut area = state.area(AreaKind::Jobs)?;
        let delegation = admin_directory(
            &area.directory,
            &manifest.delegation_id.to_string(),
            state.uid,
            &mut area.remaining,
        )?;
        let attempt = admin_directory(
            &delegation,
            &manifest.attempt_id.to_string(),
            state.uid,
            &mut area.remaining,
        )?;
        let digest = manifest.fingerprint()?;
        let pending = format!(".manifest-{digest}.pending");
        check_names(
            &attempt,
            &["manifest.json", "workspace", "staging", &pending],
        )?;
        let expected = serde_json::to_vec(manifest)?;
        if has_entry(&attempt, "workspace")? {
            if has_entry(&attempt, "staging")? || has_entry(&attempt, &pending)? {
                return Err(WorkerError::Conflict(
                    "completed workspace has unexpected staging",
                ));
            }
            exact_private_file(&attempt, "manifest.json", &expected, state.uid)?;
            private_directory(&attempt, OsStr::new("workspace"), state.uid, false)?;
            return Ok(area
                .path
                .join(manifest.delegation_id.to_string())
                .join(manifest.attempt_id.to_string())
                .join("workspace"));
        }
        if has_entry(&attempt, "staging")? && !has_entry(&attempt, "manifest.json")? {
            return Err(WorkerError::Conflict(
                "staging workspace has no durable manifest",
            ));
        }
        publish_record(&attempt, "manifest.json", &pending, &expected, state.uid)?;
        let staging = private_directory(&attempt, OsStr::new("staging"), state.uid, true)?;
        materialize(&staging, &inputs, state.uid)?;
        publish_directory(&attempt, "staging", "workspace")?;
        Ok(area
            .path
            .join(manifest.delegation_id.to_string())
            .join(manifest.attempt_id.to_string())
            .join("workspace"))
    }

    pub fn collect_artifacts(agent: &Agent, manifest: &WorkerManifest) -> Result<Vec<WorkerFile>> {
        manifest.validate()?;
        let state = State::open(agent)?;
        let area = state.area(AreaKind::Jobs)?;
        let delegation = private_directory(
            &area.directory,
            OsStr::new(&manifest.delegation_id.to_string()),
            state.uid,
            false,
        )?;
        let attempt = private_directory(
            &delegation,
            OsStr::new(&manifest.attempt_id.to_string()),
            state.uid,
            false,
        )?;
        check_names(&attempt, &["manifest.json", "workspace"])?;
        exact_private_file(
            &attempt,
            "manifest.json",
            &serde_json::to_vec(manifest)?,
            state.uid,
        )?;
        let workspace = private_directory(&attempt, OsStr::new("workspace"), state.uid, false)?;
        let mut bytes = 0;
        let mut files = Vec::new();
        for path in &manifest.outputs {
            if let Some(contents) =
                selected_file(&workspace, path, MAX_WORKER_FILE_BYTES - bytes, None)?
            {
                bytes = bytes
                    .checked_add(contents.len())
                    .ok_or(WorkerError::Capacity("artifact bytes"))?;
                files.push(WorkerFile::from_bytes(path.clone(), &contents)?);
            }
        }
        Ok(files)
    }

    pub fn quarantine_result(
        agent: &Agent,
        manifest: &WorkerManifest,
        result: &WorkerResult,
    ) -> Result<QuarantinedArtifacts> {
        manifest.validate()?;
        result.validate(manifest)?;
        let artifacts = decode_files(&result.artifacts)?;
        let digest = result.fingerprint()?;
        let expected = serde_json::to_vec(result)?;
        let state = State::open(agent)?;
        let mut area = state.area(AreaKind::Results)?;
        let delegation = admin_directory(
            &area.directory,
            &manifest.delegation_id.to_string(),
            state.uid,
            &mut area.remaining,
        )?;
        let attempt = admin_directory(
            &delegation,
            &manifest.attempt_id.to_string(),
            state.uid,
            &mut area.remaining,
        )?;
        let pending = format!(".quarantine-{digest}.pending");
        let receipt_pending = format!(".receipt-{digest}.pending");
        check_names(
            &attempt,
            &["result.json", &receipt_pending, &pending, &digest],
        )?;
        if has_entry(&attempt, &digest)? {
            if has_entry(&attempt, &pending)? || has_entry(&attempt, &receipt_pending)? {
                return Err(WorkerError::Conflict(
                    "published result has unexpected staging",
                ));
            }
            exact_private_file(&attempt, "result.json", &expected, state.uid)?;
            let directory = private_directory(&attempt, OsStr::new(&digest), state.uid, false)?;
            verify_quarantine(&directory, &artifacts, &expected, state.uid)?;
        } else {
            if has_entry(&attempt, &pending)? && !has_entry(&attempt, "result.json")? {
                return Err(WorkerError::Conflict(
                    "quarantine staging has no durable result",
                ));
            }
            publish_record(
                &attempt,
                "result.json",
                &receipt_pending,
                &expected,
                state.uid,
            )?;
            let staging = admin_directory(&attempt, &pending, state.uid, &mut area.remaining)?;
            check_names(&staging, &["receipt.json", "files"])?;
            write_private_file(&staging, "receipt.json", &expected, state.uid)?;
            let files = private_directory(&staging, OsStr::new("files"), state.uid, true)?;
            materialize(&files, &artifacts, state.uid)?;
            sync(&staging)?;
            publish_directory(&attempt, &pending, &digest)?;
        }
        Ok(QuarantinedArtifacts {
            directory: area
                .path
                .join(manifest.delegation_id.to_string())
                .join(manifest.attempt_id.to_string())
                .join(&digest),
            files: result
                .artifacts
                .iter()
                .map(|file| file.path.clone())
                .collect(),
            result_digest: digest,
        })
    }

    fn verify_quarantine(
        directory: &Dir,
        files: &Contents,
        receipt: &[u8],
        uid: u32,
    ) -> Result<()> {
        check_names(directory, &["receipt.json", "files"])?;
        exact_private_file(directory, "receipt.json", receipt, uid)?;
        let artifacts = private_directory(directory, OsStr::new("files"), uid, false)?;
        validate_tree(&artifacts, files, uid, false)?;
        Ok(())
    }

    fn identity(metadata: &Metadata) -> Identity {
        (metadata.dev(), metadata.ino())
    }

    fn inventory(
        directory: &Dir,
        depth: usize,
        count: &mut usize,
        protected: &mut BTreeSet<Identity>,
        started: Instant,
    ) -> Result<()> {
        if depth > 64 || started.elapsed() >= SCAN_DURATION {
            return Err(WorkerError::Capacity("protected state inventory"));
        }
        protected.insert(identity(&directory.dir_metadata()?));
        for entry in directory.entries()? {
            *count += 1;
            if *count > MAX_STATE_ENTRIES || started.elapsed() >= SCAN_DURATION {
                return Err(WorkerError::Capacity("protected state inventory"));
            }
            let name = entry?.file_name();
            let metadata = directory.symlink_metadata(&name)?;
            protected.insert(identity(&metadata));
            if metadata.is_dir() {
                let child = open_directory(directory, &name)?;
                if identity(&child.dir_metadata()?) != identity(&metadata) {
                    return Err(WorkerError::Conflict(
                        "state directory changed during inventory",
                    ));
                }
                inventory(&child, depth + 1, count, protected, started)?;
            }
        }
        Ok(())
    }

    fn source_root(path: &Path, protected: &BTreeSet<Identity>) -> Result<Dir> {
        if !path.is_absolute() || path == Path::new("/") || path.as_os_str().len() > 4096 {
            return Err(WorkerError::Invalid(
                "input workspace must be an absolute nonroot path",
            ));
        }
        let mut directory = Dir::open_ambient_dir("/", ambient_authority())?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => directory = open_directory(&directory, name)?,
                _ => return Err(WorkerError::Invalid("workspace path is not normalized")),
            }
            if protected.contains(&identity(&directory.dir_metadata()?)) {
                return Err(WorkerError::Invalid("protected Agent state directory"));
            }
        }
        Ok(directory)
    }

    fn selected_file(
        directory: &Dir,
        path: &str,
        maximum: usize,
        protected: Option<&BTreeSet<Identity>>,
    ) -> Result<Option<Vec<u8>>> {
        relative_path(path)?;
        let (parent, filename) = match parent(directory, path, protected) {
            Ok(parent) => parent,
            Err(WorkerError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let metadata = match parent.symlink_metadata(&filename) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        regular(&metadata)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY).bits() as i32);
        let mut file = parent.open_with(&filename, &options)?;
        let opened = file.metadata()?;
        regular(&opened)?;
        if protected.is_some_and(|protected| protected.contains(&identity(&opened)))
            || reserved_alias(&parent, &opened)?
        {
            return Err(WorkerError::Invalid("protected runtime file"));
        }
        if opened.len() > maximum as u64 {
            return Err(WorkerError::Capacity("aggregate selected file bytes"));
        }
        let mut contents = Vec::new();
        Read::by_ref(&mut file)
            .take(maximum as u64 + 1)
            .read_to_end(&mut contents)?;
        let after = file.metadata()?;
        regular(&after)?;
        if contents.len() > maximum {
            return Err(WorkerError::Capacity("aggregate selected file bytes"));
        }
        if opened.len() != contents.len() as u64 || after.len() != opened.len() {
            return Err(WorkerError::Conflict(
                "selected file changed size during read",
            ));
        }
        Ok(Some(contents))
    }

    fn parent(
        directory: &Dir,
        path: &str,
        protected: Option<&BTreeSet<Identity>>,
    ) -> Result<(Dir, OsString)> {
        let path = Path::new(path);
        let mut directory = directory.try_clone()?;
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                let next = open_directory(&directory, component.as_os_str())?;
                let metadata = next.dir_metadata()?;
                if protected.is_some_and(|protected| protected.contains(&identity(&metadata)))
                    || reserved_alias(&directory, &metadata)?
                {
                    return Err(WorkerError::Invalid("protected runtime directory"));
                }
                directory = next;
            }
        }
        Ok((
            directory,
            path.file_name()
                .ok_or(WorkerError::Invalid("file name is required"))?
                .to_owned(),
        ))
    }

    fn reserved_alias(directory: &Dir, metadata: &Metadata) -> Result<bool> {
        for name in [".everything-agents", ".everything-workspace.json"] {
            match directory.symlink_metadata(name) {
                Ok(reserved) if identity(&reserved) == identity(metadata) => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }

    fn regular(metadata: &Metadata) -> Result<()> {
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(WorkerError::Invalid(
                "worker files must be regular and singly linked",
            ));
        }
        Ok(())
    }

    fn open_directory(parent: &Dir, name: &OsStr) -> Result<Dir> {
        let descriptor = rustix::fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        Ok(Dir::from_std_file(descriptor.into()))
    }

    fn private_directory(parent: &Dir, name: &OsStr, uid: u32, create: bool) -> Result<Dir> {
        let directory = match open_directory(parent, name) {
            Ok(directory) => directory,
            Err(WorkerError::Io(error)) if error.kind() == io::ErrorKind::NotFound && create => {
                match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) => sync(parent)?,
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(io::Error::from(error).into()),
                }
                open_directory(parent, name)?
            }
            Err(error) => return Err(error),
        };
        let metadata = directory.dir_metadata()?;
        if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
            return Err(WorkerError::Invalid(
                "worker directories must be owner-only",
            ));
        }
        Ok(directory)
    }

    fn private_file(
        parent: &Dir,
        name: &OsStr,
        uid: u32,
        create: bool,
    ) -> Result<cap_std::fs::File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY).bits() as i32);
        let file = parent.open_with(name, &options)?;
        let metadata = file.metadata()?;
        regular(&metadata)?;
        if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 {
            return Err(WorkerError::Invalid(
                "worker administrative files must be owner-only",
            ));
        }
        Ok(file)
    }

    fn write_private_file(parent: &Dir, name: &str, expected: &[u8], uid: u32) -> Result<()> {
        if expected.len() > MAX_RECORD_BYTES {
            return Err(WorkerError::Capacity("worker private record bytes"));
        }
        let mut file = private_file(parent, OsStr::new(name), uid, true)?;
        let mut current = Vec::new();
        Read::by_ref(&mut file)
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut current)?;
        if !expected.starts_with(&current) {
            return Err(WorkerError::Conflict("partial worker file content"));
        }
        file.seek(SeekFrom::Start(current.len() as u64))?;
        file.write_all(&expected[current.len()..])?;
        file.sync_all()?;
        sync(parent)?;
        Ok(())
    }

    fn exact_private_file(parent: &Dir, name: &str, expected: &[u8], uid: u32) -> Result<()> {
        let mut file = private_file(parent, OsStr::new(name), uid, false)?;
        let mut actual = Vec::new();
        Read::by_ref(&mut file)
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut actual)?;
        if actual != expected {
            return Err(WorkerError::Conflict("immutable worker record changed"));
        }
        Ok(())
    }

    fn publish_record(
        parent: &Dir,
        name: &str,
        pending: &str,
        expected: &[u8],
        uid: u32,
    ) -> Result<()> {
        if has_entry(parent, name)? {
            if has_entry(parent, pending)? {
                return Err(WorkerError::Conflict(
                    "immutable worker record has unexpected staging",
                ));
            }
            return exact_private_file(parent, name, expected, uid);
        }
        write_private_file(parent, pending, expected, uid)?;
        rustix::fs::renameat_with(
            parent,
            pending,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)?;
        sync(parent)
    }

    fn publish_directory(parent: &Dir, pending: &str, name: &str) -> Result<()> {
        rustix::fs::renameat_with(
            parent,
            pending,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)?;
        sync(parent)
    }

    fn sync(directory: &Dir) -> Result<()> {
        rustix::fs::fsync(directory).map_err(io::Error::from)?;
        Ok(())
    }

    fn has_entry(directory: &Dir, name: &str) -> Result<bool> {
        match directory.symlink_metadata(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn check_names(directory: &Dir, allowed: &[&str]) -> Result<()> {
        for (index, entry) in directory.entries()?.enumerate() {
            if index >= allowed.len() {
                return Err(WorkerError::Conflict(
                    "unexpected worker administrative entry",
                ));
            }
            let name = entry?.file_name();
            if !allowed.iter().any(|allowed| name == OsStr::new(allowed)) {
                return Err(WorkerError::Conflict(
                    "unexpected worker administrative entry",
                ));
            }
        }
        Ok(())
    }

    fn decode_files(files: &[WorkerFile]) -> Result<Contents> {
        let mut decoded = BTreeMap::new();
        let mut bytes = 0_usize;
        for file in files {
            let contents = file.decode()?;
            bytes = bytes
                .checked_add(contents.len())
                .ok_or(WorkerError::Capacity("worker file bytes"))?;
            if decoded.len() >= MAX_WORKER_FILES || bytes > MAX_WORKER_FILE_BYTES {
                return Err(WorkerError::Capacity(
                    "worker file count or aggregate bytes",
                ));
            }
            if decoded.insert(file.path.clone(), contents).is_some() {
                return Err(WorkerError::Conflict("duplicate worker file"));
            }
        }
        if directories(&decoded)
            .iter()
            .any(|path| decoded.contains_key(path))
        {
            return Err(WorkerError::Invalid(
                "worker file overlaps an input directory",
            ));
        }
        Ok(decoded)
    }

    fn directories(files: &Contents) -> BTreeSet<String> {
        let mut directories = BTreeSet::new();
        for name in files.keys() {
            let mut path = Path::new(name).parent();
            while let Some(parent) = path.filter(|path| !path.as_os_str().is_empty()) {
                directories.insert(parent.to_string_lossy().into_owned());
                path = parent.parent();
            }
        }
        directories
    }

    fn validate_tree(directory: &Dir, files: &Contents, uid: u32, partial: bool) -> Result<()> {
        let directories = directories(files);
        let mut found_files = BTreeSet::new();
        let mut found_directories = BTreeSet::new();
        let mut pending = vec![(directory.try_clone()?, String::new())];
        let mut visited = 0;
        while let Some((current, prefix)) = pending.pop() {
            for entry in current.entries()? {
                visited += 1;
                if visited > MAX_TREE_ENTRIES {
                    return Err(WorkerError::Capacity("staged workspace entries"));
                }
                let name = entry?.file_name();
                let name = name
                    .to_str()
                    .ok_or(WorkerError::Invalid("staged name is not UTF-8"))?;
                let path = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                let metadata = current.symlink_metadata(name)?;
                if directories.contains(&path) && metadata.is_dir() {
                    let child = private_directory(&current, OsStr::new(name), uid, false)?;
                    found_directories.insert(path.clone());
                    pending.push((child, path));
                } else if let Some(expected) = files.get(&path) {
                    if partial {
                        let mut file = private_file(&current, OsStr::new(name), uid, false)?;
                        let mut actual = Vec::new();
                        Read::by_ref(&mut file)
                            .take(expected.len() as u64 + 1)
                            .read_to_end(&mut actual)?;
                        if !expected.starts_with(&actual) {
                            return Err(WorkerError::Conflict(
                                "staged input differs from manifest",
                            ));
                        }
                    } else {
                        exact_private_file(&current, name, expected, uid)?;
                    }
                    found_files.insert(path);
                } else {
                    return Err(WorkerError::Conflict("unexpected staged workspace entry"));
                }
            }
        }
        if !partial && (found_files.len() != files.len() || found_directories != directories) {
            return Err(WorkerError::Conflict("staged workspace is incomplete"));
        }
        Ok(())
    }

    fn materialize(directory: &Dir, files: &Contents, uid: u32) -> Result<()> {
        validate_tree(directory, files, uid, true)?;
        for (path, contents) in files {
            let mut parent = directory.try_clone()?;
            if let Some(path) = Path::new(path).parent() {
                for component in path.components() {
                    parent = private_directory(&parent, component.as_os_str(), uid, true)?;
                }
            }
            let name = Path::new(path)
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or(WorkerError::Invalid("input file name"))?;
            write_private_file(&parent, name, contents, uid)?;
        }
        validate_tree(directory, files, uid, false)?;
        sync(directory)
    }

    fn admin_directory(parent: &Dir, name: &str, uid: u32, remaining: &mut usize) -> Result<Dir> {
        if !has_entry(parent, name)? {
            if *remaining == 0 {
                return Err(WorkerError::Capacity("retained worker directories"));
            }
            *remaining -= 1;
        }
        private_directory(parent, OsStr::new(name), uid, true)
    }

    fn count_area(
        directory: &Dir,
        kind: AreaKind,
        depth: usize,
        uid: u32,
        count: &mut usize,
        visited: &mut usize,
        started: Instant,
    ) -> Result<()> {
        for entry in directory.entries()? {
            *visited += 1;
            if *visited > MAX_AREA_DIRECTORIES * 4 + 1 || started.elapsed() >= SCAN_DURATION {
                return Err(WorkerError::Capacity("worker area scan"));
            }
            let name = entry?.file_name();
            if depth == 0 && name == OsStr::new(".lock") {
                private_file(directory, &name, uid, false)?;
                continue;
            }
            let text = name
                .to_str()
                .ok_or(WorkerError::Invalid("worker administrative name"))?;
            if depth == 2
                && matches!(kind, AreaKind::Results)
                && (text == "result.json" || temporary_digest(text, ".receipt-"))
            {
                private_file(directory, &name, uid, false)?;
                continue;
            }
            let valid = if depth < 2 {
                uuid::Uuid::parse_str(text).is_ok_and(|id| !id.is_nil() && id.to_string() == text)
            } else {
                digest_name(text) || temporary_digest(text, ".quarantine-")
            };
            if !valid {
                return Err(WorkerError::Conflict(
                    "unexpected retained worker directory",
                ));
            }
            *count += 1;
            if *count > MAX_AREA_DIRECTORIES {
                return Err(WorkerError::Capacity("retained worker directories"));
            }
            let child = private_directory(directory, &name, uid, false)?;
            if depth < 1 || (depth == 1 && matches!(kind, AreaKind::Results)) {
                count_area(&child, kind, depth + 1, uid, count, visited, started)?;
            }
        }
        Ok(())
    }

    fn digest_name(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn temporary_digest(value: &str, prefix: &str) -> bool {
        value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(".pending"))
            .is_some_and(digest_name)
    }
}
