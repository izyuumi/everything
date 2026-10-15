use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, DirBuilderExt, MetadataExt, OpenOptions, OpenOptionsExt};
use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::protocol::TaskId;

use super::types::SchedulerError;

const CONTAINER: &str = ".everything-agents";
const MARKER: &str = ".everything-workspace.json";

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Ownership {
    version: u16,
    task_id: TaskId,
    parent_task_id: TaskId,
}

fn directory(parent: &Dir, path: &Path) -> Result<Dir, SchedulerError> {
    let file = rustix::fs::openat(
        parent,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(Dir::from_std_file(file.into()))
}

fn root(path: &Path) -> Result<Dir, SchedulerError> {
    if !path.is_absolute() {
        return Err(SchedulerError::Invalid("workspace root is not absolute"));
    }
    let mut result = Dir::open_ambient_dir("/", ambient_authority())?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => result = directory(&result, Path::new(name))?,
            _ => return Err(SchedulerError::Invalid("workspace root contains traversal")),
        }
    }
    Ok(result)
}

fn private(directory: &Dir) -> Result<(), SchedulerError> {
    let metadata = directory.dir_metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o7777 != 0o700 {
        return Err(SchedulerError::Invalid(
            "child workspace must be an owner-only private directory",
        ));
    }
    Ok(())
}

fn create(parent: &Dir, name: &str) -> Result<(Dir, bool), SchedulerError> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    let created = match parent.create_dir_with(name, &builder) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let child = directory(parent, Path::new(name))?;
    private(&child)?;
    if created {
        parent.open(".")?.sync_all()?;
    }
    Ok((child, created))
}

pub(super) fn prepare(
    parent_root: &Path,
    parent_id: TaskId,
    id: TaskId,
) -> Result<PathBuf, SchedulerError> {
    let parent = root(parent_root)?;
    let (container, _) = create(&parent, CONTAINER)?;
    let mut count = 0;
    for entry in container.entries()? {
        entry?;
        count += 1;
        if count > 64 {
            return Err(SchedulerError::Capacity("private child workspace count"));
        }
    }
    if count == 64 && !container.try_exists(id.to_string())? {
        return Err(SchedulerError::Capacity("private child workspace count"));
    }
    let (child, created) = create(&container, &id.to_string())?;
    let identity = Ownership {
        version: 1,
        task_id: id,
        parent_task_id: parent_id,
    };
    if created {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut marker = child.open_with(MARKER, &options)?;
        marker.write_all(&serde_json::to_vec(&identity)?)?;
        marker.sync_all()?;
        child.open(".")?.sync_all()?;
    } else {
        validate_marker(&child, &identity)?;
        for entry in child.entries()? {
            if entry?.file_name() != MARKER {
                return Err(SchedulerError::Invalid(
                    "unclaimed child workspace contains unexpected files",
                ));
            }
        }
    }
    Ok(parent_root.join(CONTAINER).join(id.to_string()))
}

fn validate_marker(child: &Dir, identity: &Ownership) -> Result<(), SchedulerError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY).bits() as i32);
    let marker = child.open_with(MARKER, &options)?;
    let metadata = marker.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(SchedulerError::Invalid(
            "workspace ownership marker is not private",
        ));
    }
    let mut bytes = Vec::new();
    marker.take(1025).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 || serde_json::from_slice::<Ownership>(&bytes)? != *identity {
        return Err(SchedulerError::Invalid(
            "workspace ownership identity conflicts",
        ));
    }
    Ok(())
}

pub(super) fn validate(path: &Path, parent: TaskId, id: TaskId) -> Result<(), SchedulerError> {
    let directory = root(path)?;
    private(&directory)?;
    validate_marker(
        &directory,
        &Ownership {
            version: 1,
            task_id: id,
            parent_task_id: parent,
        },
    )
}
