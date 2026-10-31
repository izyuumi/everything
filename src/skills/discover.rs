use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::frontmatter::{self, Document};
use super::lock::{self, Lock, LockEntry};
use super::{
    MAX_COMPATIBILITY_BYTES, MAX_DESCRIPTION_BYTES, MAX_SKILL_DIRS_PER_ROOT, MAX_SKILL_FILE_BYTES,
    SkillDirName, SkillIssue, SkillName, SkillRecord, SkillRoot,
};

#[derive(Clone, Debug)]
pub struct SkillRoots {
    pub home: PathBuf,
    pub workspace: Option<PathBuf>,
    pub extra: Vec<PathBuf>,
}

impl SkillRoots {
    pub fn new(home: PathBuf, workspace: Option<PathBuf>, extra: Option<Vec<PathBuf>>) -> Self {
        let extra = extra.unwrap_or_else(|| vec![default_extra_root(&home)]);
        Self {
            home,
            workspace,
            extra,
        }
    }

    fn global_root(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }

    fn roots(&self) -> Vec<SkillRoot> {
        let identity = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let global = SkillRoot::Global(self.global_root());
        let mut seen = BTreeSet::from([identity(global.path())]);
        let mut roots = Vec::new();
        if let Some(workspace) = &self.workspace {
            let project = workspace.join(".agents").join("skills");
            if seen.insert(identity(&project)) {
                roots.push(SkillRoot::Project(project));
            }
        }
        roots.push(global);
        for extra in &self.extra {
            if seen.insert(identity(extra)) {
                roots.push(SkillRoot::Extra(extra.clone()));
            }
        }
        roots
    }
}

fn default_extra_root(home: &Path) -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".claude"), PathBuf::from)
        .join("skills")
}

#[derive(Clone, Debug, Serialize)]
pub struct RejectedSkill {
    pub label: String,
    pub root: SkillRoot,
    pub dir: PathBuf,
    pub issues: Vec<SkillIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MissingSkill {
    pub name: String,
    pub scope: &'static str,
    pub entry: LockEntry,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub skills: Vec<SkillRecord>,
    pub rejected: Vec<RejectedSkill>,
    pub missing: Vec<MissingSkill>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Cancelled;

enum Scanned {
    Skill(Box<SkillRecord>),
    Rejected(RejectedSkill),
}

pub fn discover(
    roots: &SkillRoots,
    cancellation: &CancellationToken,
) -> Result<Catalog, Cancelled> {
    let mut catalog = Catalog::default();
    let global_lock = lock::read_global(&roots.home, &mut catalog.warnings);
    let project_lock = roots
        .workspace
        .as_deref()
        .map(|workspace| lock::read_project(workspace, &mut catalog.warnings))
        .unwrap_or_default();
    let mut offered = BTreeSet::new();
    for root in roots.roots() {
        for scanned in scan_root(&root, &mut catalog.warnings, cancellation)? {
            match scanned {
                Scanned::Skill(mut record) => {
                    let lock = match record.root {
                        SkillRoot::Project(_) => &project_lock,
                        SkillRoot::Global(_) | SkillRoot::Extra(_) => &global_lock,
                    };
                    record.source = lock
                        .find(record.name.as_str())
                        .map(|(_, entry)| entry.clone());
                    record.shadowed = !offered.insert(record.name.dir_name());
                    catalog.skills.push(*record);
                }
                Scanned::Rejected(rejected) => catalog.rejected.push(rejected),
            }
        }
    }
    let folder =
        |dir: &Path| SkillDirName::sanitize(&dir.file_name().unwrap_or_default().to_string_lossy());
    let installed = |project: bool| -> BTreeSet<SkillDirName> {
        let in_scope = |root: &SkillRoot| matches!(root, SkillRoot::Project(_)) == project;
        catalog
            .skills
            .iter()
            .filter(|record| in_scope(&record.root))
            .flat_map(|record| [record.name.dir_name(), folder(&record.dir)])
            .chain(
                catalog
                    .rejected
                    .iter()
                    .filter(|rejected| in_scope(&rejected.root))
                    .map(|rejected| folder(&rejected.dir)),
            )
            .collect()
    };
    let installed_project = installed(true);
    let installed_global = installed(false);
    for (lock, installed, scope) in [
        (&project_lock, &installed_project, "project"),
        (&global_lock, &installed_global, "global"),
    ] {
        missing(lock, installed, scope, &mut catalog.missing);
    }
    Ok(catalog)
}

fn missing(
    lock: &Lock,
    installed: &BTreeSet<SkillDirName>,
    scope: &'static str,
    output: &mut Vec<MissingSkill>,
) {
    output.extend(
        lock.entries
            .iter()
            .filter(|(key, _)| !installed.contains(&SkillDirName::sanitize(key)))
            .map(|(key, entry)| MissingSkill {
                name: key.clone(),
                scope,
                entry: entry.clone(),
            }),
    );
}

fn scan_root(
    root: &SkillRoot,
    warnings: &mut Vec<String>,
    cancellation: &CancellationToken,
) -> Result<Vec<Scanned>, Cancelled> {
    if let SkillRoot::Project(path) = root
        && let Err(reason) = trusted_directory(path)
    {
        if let Some(reason) = reason {
            warnings.push(format!(
                "project root {} was not scanned: {reason}",
                path.display()
            ));
        }
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(root.path()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            warnings.push(format!(
                "{} root {} could not be read: {error}",
                root.label(),
                root.path().display()
            ));
            return Ok(Vec::new());
        }
    };
    let mut candidates = Vec::new();
    for entry in entries {
        if cancellation.is_cancelled() {
            return Err(Cancelled);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "{} root {} could not be listed: {error}",
                    root.label(),
                    root.path().display()
                ));
                break;
            }
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        if candidates.len() == MAX_SKILL_DIRS_PER_ROOT {
            warnings.push(format!(
                "{} root {} has more than {MAX_SKILL_DIRS_PER_ROOT} directories; the rest were not scanned",
                root.label(),
                root.path().display()
            ));
            break;
        }
        candidates.push(entry.path());
    }
    candidates.sort();
    let mut scanned = Vec::with_capacity(candidates.len());
    for dir in candidates {
        if cancellation.is_cancelled() {
            return Err(Cancelled);
        }
        scanned.extend(scan_dir(root, dir));
    }
    Ok(scanned)
}

fn trusted_directory(path: &Path) -> Result<(), Option<String>> {
    let directory = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Err(None),
        Err(error) => return Err(Some(io::Error::from(error).to_string())),
    };
    let status = rustix::fs::fstat(&directory).map_err(|error| Some(error.to_string()))?;
    if status.st_uid != rustix::process::geteuid().as_raw() {
        return Err(Some("not owned by the current user".to_owned()));
    }
    if status.st_mode & 0o022 != 0 {
        return Err(Some("group- or world-writable".to_owned()));
    }
    Ok(())
}

fn scan_dir(root: &SkillRoot, dir: PathBuf) -> Option<Scanned> {
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rejected = |label: String, issues: Vec<SkillIssue>| {
        Some(Scanned::Rejected(RejectedSkill {
            label,
            root: root.clone(),
            dir: dir.clone(),
            issues,
        }))
    };
    let contents = match read_skill_file(&dir) {
        Ok(Some(contents)) => contents,
        Ok(None) => return None,
        Err(issue) => return rejected(folder, vec![issue]),
    };
    let Some(Document { frontmatter, .. }) = frontmatter::split(&contents) else {
        return rejected(folder, vec![SkillIssue::MissingFrontmatter]);
    };
    let fields = match frontmatter::parse(frontmatter) {
        Ok(fields) => fields,
        Err(issue) => return rejected(folder, vec![issue]),
    };
    let mut issues = fields.issues;
    let Some(raw_name) = fields.name else {
        if !issues.contains(&SkillIssue::NotAString { field: "name" }) {
            issues.insert(0, SkillIssue::MissingName);
        }
        return rejected(folder, issues);
    };
    let name = match SkillName::parse(&raw_name) {
        Ok(name) => name,
        Err(issue) => {
            issues.insert(0, issue);
            return rejected(raw_name, issues);
        }
    };
    if !name.follows_spec_grammar() {
        issues.push(SkillIssue::NameGrammar);
    }
    if folder != name.as_str() {
        issues.push(SkillIssue::DirectoryMismatch {
            directory: folder.clone(),
        });
    }
    let description = fields.description.unwrap_or_default();
    if description.is_empty() {
        issues.push(SkillIssue::MissingDescription);
    } else if description.len() > MAX_DESCRIPTION_BYTES {
        issues.push(SkillIssue::DescriptionTooLong {
            bytes: description.len(),
        });
    }
    if let Some(compatibility) = &fields.compatibility
        && compatibility.len() > MAX_COMPATIBILITY_BYTES
    {
        issues.push(SkillIssue::CompatibilityTooLong {
            bytes: compatibility.len(),
        });
    }
    Some(Scanned::Skill(Box::new(SkillRecord {
        dir_name: name.dir_name(),
        name,
        description,
        root: root.clone(),
        dir,
        license: fields.license,
        compatibility: fields.compatibility,
        metadata: fields.metadata,
        allowed_tools: fields.allowed_tools,
        source: None,
        issues,
        shadowed: false,
    })))
}

pub fn read_body(record: &SkillRecord) -> Result<String, SkillIssue> {
    let contents = read_skill_file(&record.dir)?.ok_or_else(|| SkillIssue::Unreadable {
        message: "SKILL.md no longer exists".to_owned(),
    })?;
    let document = frontmatter::split(&contents).ok_or(SkillIssue::MissingFrontmatter)?;
    Ok(document.body.to_owned())
}

fn read_skill_file(dir: &Path) -> Result<Option<String>, SkillIssue> {
    let Some(directory) = open_skill_dir(dir)? else {
        return Ok(None);
    };
    let file = match rustix::fs::openat(
        &directory,
        "SKILL.md",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => return Err(SkillIssue::SkillFileNotRegular),
        Err(error) => return Err(unreadable(error)),
    };
    let status = rustix::fs::fstat(&file).map_err(unreadable)?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(SkillIssue::SkillFileNotRegular);
    }
    let size = u64::try_from(status.st_size).unwrap_or(u64::MAX);
    if size > MAX_SKILL_FILE_BYTES {
        return Err(SkillIssue::SkillFileTooLarge { bytes: size });
    }
    let mut bytes = Vec::new();
    File::from(file)
        .take(MAX_SKILL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SkillIssue::Unreadable {
            message: error.to_string(),
        })?;
    if bytes.len() as u64 > MAX_SKILL_FILE_BYTES {
        return Err(SkillIssue::SkillFileTooLarge {
            bytes: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| SkillIssue::NotUtf8)
}

fn open_skill_dir(dir: &Path) -> Result<Option<OwnedFd>, SkillIssue> {
    let link = std::fs::symlink_metadata(dir)
        .map_err(|error| SkillIssue::Unreadable {
            message: error.to_string(),
        })?
        .file_type()
        .is_symlink();
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !link {
        flags |= OFlags::NOFOLLOW;
    }
    let directory = match rustix::fs::open(dir, flags, Mode::empty()) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) if link => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    if link {
        let status = rustix::fs::fstat(&directory).map_err(unreadable)?;
        if status.st_uid != rustix::process::geteuid().as_raw() {
            return Err(SkillIssue::SymlinkRefused {
                reason: "target is not owned by the current user",
            });
        }
        if status.st_mode & 0o022 != 0 {
            return Err(SkillIssue::SymlinkRefused {
                reason: "target is group- or world-writable",
            });
        }
    }
    Ok(Some(directory))
}

fn unreadable(error: rustix::io::Errno) -> SkillIssue {
    SkillIssue::Unreadable {
        message: io::Error::from(error).to_string(),
    }
}
