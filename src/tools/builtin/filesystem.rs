use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
use rustix::fs::{Mode, OFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::permissions::ToolScopeRule;
use crate::tools::{
    Tool, ToolCapability, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext,
    ToolOutput,
};

use super::{Redactor, no_effect, output};

const READ_BYTES: usize = 8192;
const FILE_SCAN_BYTES: usize = 262_144;
const SEARCH_SCAN_BYTES: usize = 4_194_304;
const SEARCH_ENTRIES: usize = 4096;
const SEARCH_FILES: usize = 1024;
const SEARCH_DEPTH: usize = 16;
const SEARCH_DURATION: Duration = Duration::from_secs(5);
const SEARCH_EXCERPT_BYTES: usize = 256;
const SEARCH_CONTEXT_BYTES: usize = 96;
const WRITE_BYTES: usize = 262_144;
const PROTECTED: &str = "path is inside the protected Host state directory";

pub(super) struct Protected {
    path: PathBuf,
    dev: u64,
    ino: u64,
    workspace: PathBuf,
}

impl Protected {
    pub(super) fn open(path: &Path) -> io::Result<Arc<Self>> {
        let path = std::fs::canonicalize(path)?;
        let metadata = std::fs::metadata(&path)?;
        let workspace = path.join("workspace");
        Ok(Arc::new(Self {
            dev: std::os::unix::fs::MetadataExt::dev(&metadata),
            ino: std::os::unix::fs::MetadataExt::ino(&metadata),
            workspace: std::fs::canonicalize(&workspace).unwrap_or(workspace),
            path,
        }))
    }

    fn contains(&self, path: &Path) -> bool {
        let resolved = match std::fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return true;
                };
                match std::fs::canonicalize(parent) {
                    Ok(parent) => parent.join(name),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
                    Err(_) => return true,
                }
            }
            Err(_) => return true,
        };
        resolved.starts_with(&self.path) && !self.exempt(&resolved)
    }

    fn exempt(&self, path: &Path) -> bool {
        path.starts_with(&self.workspace)
    }

    fn is(&self, metadata: &Metadata) -> bool {
        metadata.dev() == self.dev && metadata.ino() == self.ino
    }

    fn check(protected: Option<&Self>, directory: &Dir) -> io::Result<()> {
        match protected {
            Some(protected) if protected.is(&directory.dir_metadata()?) => {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, PROTECTED))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Read,
    Write,
    Search,
}

struct FilesystemTool {
    descriptor: ToolDescriptor,
    operation: Operation,
    redactor: Redactor,
    protected: Option<Arc<Protected>>,
}

pub(super) fn registrations(
    redactor: Redactor,
    protected: Option<Arc<Protected>>,
) -> Vec<(Arc<dyn Tool>, ToolScopeRule)> {
    let path = json!({"type":"string","minLength":1,"maxLength":1024});
    [
        (
            Operation::Read,
            "read_file",
            "Read a bounded text range from a regular workspace file; rejects symlinks and hard links",
            json!({"type":"object","properties":{"path":path,"offset":{"type":"integer","minimum":0,"maximum":9_223_372_036_854_775_807_u64}},"required":["path"],"additionalProperties":false}),
        ),
        (
            Operation::Write,
            "write_file",
            "Atomically create or replace a UTF-8 workspace file in an existing directory; rejects symlinks and hard links",
            json!({"type":"object","properties":{"path":path,"text":{"type":"string","maxLength":WRITE_BYTES}},"required":["path","text"],"additionalProperties":false}),
        ),
        (
            Operation::Search,
            "search_files",
            "Search regular UTF-8 workspace files for a literal string with bounded traversal and explicit partial-result metadata",
            json!({"type":"object","properties":{"path":path,"query":{"type":"string","minLength":1,"maxLength":256}},"required":["path","query"],"additionalProperties":false}),
        ),
    ]
    .into_iter()
    .map(|(operation, name, description, input_schema)| {
        let writes = matches!(operation, Operation::Write);
        (
            Arc::new(FilesystemTool {
                descriptor: ToolDescriptor {
                    name: name.into(),
                    description: description.into(),
                    input_schema,
                    capabilities: vec![if writes {
                        ToolCapability::FilesystemWrite
                    } else {
                        ToolCapability::FilesystemRead
                    }],
                    read_only: !writes,
                    concurrency: if writes {
                        ToolConcurrency::Exclusive
                    } else {
                        ToolConcurrency::Concurrent
                    },
                },
                operation,
                redactor: redactor.clone(),
                protected: protected.clone(),
            }) as Arc<dyn Tool>,
            ToolScopeRule::Filesystem {
                path_pointers: vec!["/path".into()],
            },
        )
    })
    .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    path: String,
    query: String,
}

#[async_trait]
impl Tool for FilesystemTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let name = self.descriptor.name.clone();
        let worker_name = name.clone();
        let root = context.workspace_root().to_owned();
        let cancellation = context.cancellation().clone();
        let redactor = self.redactor.for_task(context);
        let operation = self.operation;
        let error_redactor = redactor.clone();
        let retained_context = context.clone();
        let protected = self.protected.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _retained_context = retained_context;
            check_cancelled(&worker_name, &cancellation)?;
            let protected = protected.as_deref();
            if let Some(guard) = protected
                && let Some(path) = arguments.get("path").and_then(Value::as_str)
            {
                let path =
                    relative_path(&worker_name, path, matches!(operation, Operation::Search))?;
                if guard.contains(&root.join(path)) {
                    return Err(no_effect(&worker_name, PROTECTED));
                }
            }
            let directory = open_root(&root, protected)
                .map_err(|error| no_effect(&worker_name, redactor.error(error)))?;
            match operation {
                Operation::Read => {
                    read_file(&directory, arguments, &redactor, &cancellation, protected)
                }
                Operation::Write => {
                    write_file(&directory, arguments, &redactor, &cancellation, protected)
                }
                Operation::Search => {
                    search_files(&directory, arguments, &redactor, &cancellation, protected)
                }
            }
        })
        .await
        .map_err(|error| ToolError::EffectsUnknown {
            name,
            message: error_redactor.error(format!("filesystem worker failed: {error}")),
            cleanup_completed: false,
        })?;
        error_redactor.finish(result)
    }
}

fn parse<T: serde::de::DeserializeOwned>(name: &str, value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| no_effect(name, error.to_string()))
}

fn relative_path(name: &str, value: &str, allow_root: bool) -> Result<PathBuf, ToolError> {
    if allow_root && value == "." {
        return Ok(PathBuf::new());
    }
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path
            .components()
            .any(|part| matches!(part,Component::Normal(name) if reserved(name)))
    {
        return Err(no_effect(
            name,
            "path requires normal workspace-relative components, at most 1024 bytes, without controls, backslashes, or reserved runtime metadata",
        ));
    }
    Ok(path.to_owned())
}

fn reserved(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.eq_ignore_ascii_case(".everything-agents")
            || name.eq_ignore_ascii_case(".everything-workspace.json")
    })
}

fn same_entry(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn reserved_identity(directory: &Dir, name: &str, metadata: &Metadata) -> io::Result<bool> {
    match directory.symlink_metadata(name) {
        Ok(reserved) => Ok(same_entry(&reserved, metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_workspace_directory(
    directory: &Dir,
    path: &Path,
    protected: Option<&Protected>,
) -> io::Result<Dir> {
    let child = open_directory(directory, path)?;
    Protected::check(protected, &child)?;
    if reserved_identity(directory, ".everything-agents", &child.dir_metadata()?)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reserved runtime directory",
        ));
    }
    Ok(child)
}

fn ordinary_file(directory: &Dir, metadata: &Metadata) -> io::Result<()> {
    regular(metadata)?;
    if reserved_identity(directory, ".everything-workspace.json", metadata)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reserved runtime metadata",
        ));
    }
    Ok(())
}

fn open_root(root: &Path, protected: Option<&Protected>) -> io::Result<Dir> {
    if !root.is_absolute() {
        return Err(io::Error::other("workspace root is not absolute"));
    }
    let protected = protected
        .filter(|guard| !std::fs::canonicalize(root).is_ok_and(|root| guard.exempt(&root)));
    let mut directory = Dir::open_ambient_dir("/", ambient_authority())?;
    for part in root.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_directory(&directory, Path::new(name))?,
            _ => return Err(io::Error::other("invalid workspace root")),
        }
        Protected::check(protected, &directory)?;
    }
    Ok(directory)
}

fn open_directory(directory: &Dir, path: &Path) -> io::Result<Dir> {
    let file = rustix::fs::openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(Dir::from_std_file(file.into()))
}

fn parent(
    directory: &Dir,
    path: &Path,
    protected: Option<&Protected>,
) -> io::Result<(Dir, std::ffi::OsString)> {
    let mut current = directory.try_clone()?;
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::other("file path has no filename"))?
        .to_owned();
    if let Some(path) = path.parent() {
        for part in path.components() {
            current = open_workspace_directory(&current, Path::new(part.as_os_str()), protected)?;
        }
    }
    Ok((current, filename))
}

fn regular(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::other(
            "only regular files with exactly one hard link are supported",
        ));
    }
    Ok(())
}

enum OpenFailure {
    Before(io::Error),
    After(io::Error),
}

impl OpenFailure {
    fn tool_error(self, name: &str, redactor: &Redactor) -> ToolError {
        match self {
            Self::Before(error) => no_effect(name, redactor.error(error)),
            Self::After(error) => ToolError::EffectsUnknown {
                name: name.into(),
                message: redactor.error(error),
                cleanup_completed: true,
            },
        }
    }
}

fn open_regular(
    directory: &Dir,
    filename: &std::ffi::OsStr,
) -> Result<cap_std::fs::File, OpenFailure> {
    ordinary_file(
        directory,
        &directory
            .symlink_metadata(filename)
            .map_err(OpenFailure::Before)?,
    )
    .map_err(OpenFailure::Before)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY).bits() as i32);
    let file = directory
        .open_with(filename, &options)
        .map_err(OpenFailure::Before)?;
    ordinary_file(directory, &file.metadata().map_err(OpenFailure::After)?)
        .map_err(OpenFailure::After)?;
    Ok(file)
}

fn read_file(
    directory: &Dir,
    arguments: Value,
    redactor: &Redactor,
    cancellation: &CancellationToken,
    protected: Option<&Protected>,
) -> Result<ToolOutput, ToolError> {
    let name = "read_file";
    let arguments: ReadArguments = parse(name, arguments)?;
    let path = relative_path(name, &arguments.path, false)?;
    let (directory, filename) = parent(directory, &path, protected)
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    let mut file =
        open_regular(&directory, &filename).map_err(|error| error.tool_error(name, redactor))?;
    let total_bytes = file
        .metadata()
        .map_err(|error| no_effect(name, redactor.error(error)))?
        .len();
    let lookaround = redactor.lookaround();
    let beginning = arguments.offset.saturating_sub(lookaround as u64);
    let skipped = (arguments.offset - beginning) as usize;
    file.seek(SeekFrom::Start(beginning))
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    check_cancelled(name, cancellation)?;
    let mut bytes = Vec::with_capacity(skipped + READ_BYTES + lookaround + 1);
    file.take((skipped + READ_BYTES + lookaround + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    check_cancelled(name, cancellation)?;
    let start = skipped.min(bytes.len());
    let end = (start + READ_BYTES).min(bytes.len());
    let more = bytes.len() > end;
    let utf8_lossy = std::str::from_utf8(&bytes[start..end]).is_err();
    let bytes_read = end - start;
    let text = redactor.range(&bytes, start..end, READ_BYTES, false);
    output(
        name,
        json!({"text":text.text,"offset":arguments.offset,"bytes_read":bytes_read,"total_bytes":total_bytes,"truncated":more || text.truncated,"utf8_lossy":utf8_lossy,"redacted":text.redacted}),
        false,
    )
}

fn write_file(
    directory: &Dir,
    arguments: Value,
    redactor: &Redactor,
    cancellation: &CancellationToken,
    protected: Option<&Protected>,
) -> Result<ToolOutput, ToolError> {
    let name = "write_file";
    let arguments: WriteArguments = parse(name, arguments)?;
    if arguments.text.len() > WRITE_BYTES {
        return Err(no_effect(name, "write content exceeds 262144 bytes"));
    }
    let path = relative_path(name, &arguments.path, false)?;
    let (directory, filename) = parent(directory, &path, protected)
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    match directory.symlink_metadata(&filename) {
        Ok(metadata) => ordinary_file(&directory, &metadata)
            .map_err(|error| no_effect(name, redactor.error(error)))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(no_effect(name, redactor.error(error))),
    }
    check_cancelled(name, cancellation)?;
    let temporary = format!(".vorra-write-{}.tmp", Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = directory
        .open_with(&temporary, &options)
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    let result = (|| -> io::Result<()> {
        for chunk in arguments.text.as_bytes().chunks(8192) {
            if cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "write cancelled before commit",
                ));
            }
            file.write_all(chunk)?;
        }
        file.sync_all()?;
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "write cancelled before commit",
            ));
        }
        directory.rename(&temporary, &directory, &filename)?;
        directory.open(".")?.sync_all()
    })();
    if let Err(error) = result {
        let cleanup = match directory.remove_file(&temporary) {
            Ok(()) => directory.open(".").and_then(|file| file.sync_all()).is_ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        return Err(ToolError::EffectsUnknown {
            name: name.into(),
            message: redactor.error(error),
            cleanup_completed: cleanup,
        });
    }
    output(
        name,
        json!({"bytes_written":arguments.text.len(),"atomic":true,"mode":"0600"}),
        false,
    )
}

struct SearchState<'a> {
    query: &'a str,
    redactor: &'a Redactor,
    cancellation: &'a CancellationToken,
    protected: Option<&'a Protected>,
    deadline: Instant,
    entries: usize,
    files: usize,
    bytes: usize,
    skipped: usize,
    matches: Vec<Value>,
    limits: Vec<&'static str>,
    warnings: Vec<String>,
}

fn search_files(
    directory: &Dir,
    arguments: Value,
    redactor: &Redactor,
    cancellation: &CancellationToken,
    protected: Option<&Protected>,
) -> Result<ToolOutput, ToolError> {
    let name = "search_files";
    let arguments: SearchArguments = parse(name, arguments)?;
    if arguments.query.is_empty()
        || arguments.query.len() > 1024
        || arguments.query.contains(['\r', '\n'])
    {
        return Err(no_effect(
            name,
            "query requires 1–1024 UTF-8 bytes without line breaks",
        ));
    }
    let path = relative_path(name, &arguments.path, true)?;
    let mut root = directory
        .try_clone()
        .map_err(|error| no_effect(name, redactor.error(error)))?;
    for part in path.components() {
        root = open_workspace_directory(&root, Path::new(part.as_os_str()), protected)
            .map_err(|error| no_effect(name, redactor.error(error)))?;
    }
    let mut state = SearchState {
        query: &arguments.query,
        redactor,
        cancellation,
        protected,
        deadline: Instant::now() + SEARCH_DURATION,
        entries: 0,
        files: 0,
        bytes: 0,
        skipped: 0,
        matches: Vec::new(),
        limits: Vec::new(),
        warnings: Vec::new(),
    };
    state.visit(&root, &path, 0)?;
    output(
        name,
        json!({"matches":state.matches,"complete":state.limits.is_empty() && state.warnings.is_empty(),"limits":state.limits,"warnings":state.warnings,"entries_visited":state.entries,"files_scanned":state.files,"bytes_scanned":state.bytes,"skipped_entries":state.skipped}),
        false,
    )
}

impl SearchState<'_> {
    fn stopped(&mut self) -> Result<bool, ToolError> {
        check_cancelled("search_files", self.cancellation)?;
        let limit = if self.limits.contains(&"output_bytes") {
            Some("output_bytes")
        } else if Instant::now() >= self.deadline {
            Some("duration")
        } else if self.entries >= SEARCH_ENTRIES {
            Some("entries")
        } else if self.files >= SEARCH_FILES {
            Some("files")
        } else if self.bytes >= SEARCH_SCAN_BYTES {
            Some("scan_bytes")
        } else if self.matches.len() >= 100 {
            Some("matches")
        } else {
            None
        };
        if let Some(limit) = limit {
            self.limit(limit);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn limit(&mut self, value: &'static str) {
        if !self.limits.contains(&value) {
            self.limits.push(value);
        }
    }

    fn warning(&mut self, error: impl std::fmt::Display) {
        self.skipped += 1;
        if self.warnings.len() < 8 {
            self.warnings
                .push(self.redactor.apply(&error.to_string(), 256, false).text);
        }
    }

    fn visit(&mut self, directory: &Dir, prefix: &Path, depth: usize) -> Result<(), ToolError> {
        if self.stopped()? {
            return Ok(());
        }
        if depth > SEARCH_DEPTH {
            self.limit("depth");
            return Ok(());
        }
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(error) => {
                self.warning(error);
                return Ok(());
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            if self.entries + names.len() >= SEARCH_ENTRIES {
                self.limit("entries");
                break;
            }
            match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    if !reserved(&name) {
                        names.push(name);
                    }
                }
                Err(error) => self.warning(error),
            }
            if self.stopped()? {
                return Ok(());
            }
        }
        names.sort();
        for name in names {
            if self.stopped()? {
                break;
            }
            self.entries += 1;
            let path = prefix.join(&name);
            let Some(path_text) = path
                .to_str()
                .filter(|path| path.len() <= 1024 && !path.chars().any(char::is_control))
            else {
                self.warning("non-UTF-8, control-bearing, or oversized path skipped");
                continue;
            };
            let metadata = match directory.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.warning(error);
                    continue;
                }
            };
            if metadata.is_dir() {
                match open_workspace_directory(directory, Path::new(&name), self.protected) {
                    Ok(child) => self.visit(&child, &path, depth + 1)?,
                    Err(error) => self.warning(error),
                }
            } else if metadata.is_file() && metadata.nlink() == 1 {
                self.scan(directory, &name, path_text)?;
            } else {
                self.warning("symlink, hard-linked file, or special file skipped");
            }
        }
        Ok(())
    }

    fn scan(
        &mut self,
        directory: &Dir,
        filename: &std::ffi::OsStr,
        path: &str,
    ) -> Result<(), ToolError> {
        let file = match open_regular(directory, filename) {
            Ok(file) => file,
            Err(OpenFailure::Before(error)) => {
                self.warning(error);
                return Ok(());
            }
            Err(error) => return Err(error.tool_error("search_files", self.redactor)),
        };
        let maximum = FILE_SCAN_BYTES.min(SEARCH_SCAN_BYTES - self.bytes);
        let mut bytes = Vec::with_capacity(maximum + 1);
        if let Err(error) = file.take((maximum + 1) as u64).read_to_end(&mut bytes) {
            self.warning(error);
            return Ok(());
        }
        self.files += 1;
        self.bytes += bytes.len().min(maximum);
        let incomplete = bytes.len() > maximum;
        bytes.truncate(maximum);
        if incomplete {
            self.limit("file_bytes");
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                self.warning("non-UTF-8 file skipped");
                return Ok(());
            }
        };
        let mut line_start = 0;
        for (number, raw_line) in text.split_inclusive('\n').enumerate() {
            let start = line_start;
            line_start += raw_line.len();
            let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if self.stopped()? {
                break;
            }
            if let Some(index) = line.find(self.query) {
                let mut from = index
                    .saturating_sub(SEARCH_CONTEXT_BYTES)
                    .min(line.len().saturating_sub(SEARCH_EXCERPT_BYTES));
                while !line.is_char_boundary(from) {
                    from -= 1;
                }
                let snippet = self.redactor.range(
                    &bytes,
                    start + from..start + line.len(),
                    SEARCH_EXCERPT_BYTES,
                    incomplete,
                );
                let rendered_path = self.redactor.apply(path, 1024, false);
                let entry = json!({"path":rendered_path.text,"line":number+1,"text":snippet.text,"truncated":snippet.truncated || from>0,"redacted":snippet.redacted || rendered_path.redacted});
                self.matches.push(entry);
                if serde_json::to_vec(&self.matches).map_or(true, |bytes| bytes.len() > 45_000) {
                    self.matches.pop();
                    self.limit("output_bytes");
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

fn check_cancelled(name: &str, cancellation: &CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(no_effect(
            name,
            "filesystem operation cancelled before content changes",
        ));
    }
    Ok(())
}
