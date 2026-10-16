use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use super::index::{SearchHit, SyncReport, WikiIndex};

#[derive(Debug, Error)]
pub enum WikiError {
    #[error("wiki I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid wiki page path: {0}")]
    InvalidPath(String),
    #[error("wiki {resource} exceeds limit {limit}")]
    Limit {
        resource: &'static str,
        limit: usize,
    },
    #[error("wiki content is not UTF-8: {0}")]
    InvalidEncoding(String),
    #[error("wiki page already exists: {0}")]
    AlreadyExists(String),
    #[error("wiki index has an incompatible application identifier or schema version")]
    IncompatibleIndex,
    #[error("wiki index error: {0}")]
    Database(#[from] rusqlite::Error),
}

#[derive(Clone, Debug)]
pub struct WikiLimits {
    pub max_page_bytes: usize,
    pub max_chunk_bytes: usize,
    pub max_pages: usize,
    pub max_chunks_per_page: usize,
}

impl Default for WikiLimits {
    fn default() -> Self {
        Self {
            max_page_bytes: 2 * 1024 * 1024,
            max_chunk_bytes: 16 * 1024,
            max_pages: 10_000,
            max_chunks_per_page: 4_096,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub page_path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiChunk {
    pub id: String,
    pub heading: String,
    pub markdown: String,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiLink {
    pub raw_target: String,
    pub target_page: Option<String>,
    pub target_fragment: Option<String>,
    pub label: String,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiPage {
    pub path: String,
    pub title: String,
    pub content_hash: String,
    pub markdown: String,
    pub chunks: Vec<WikiChunk>,
    pub links: Vec<WikiLink>,
}

pub struct Wiki {
    root: PathBuf,
    directory: Dir,
    index: WikiIndex,
    limits: WikiLimits,
}

impl Wiki {
    pub fn open(root: impl AsRef<Path>, index_path: impl AsRef<Path>) -> Result<Self, WikiError> {
        Self::open_with_limits(root, index_path, WikiLimits::default())
    }

    pub fn open_with_limits(
        root: impl AsRef<Path>,
        index_path: impl AsRef<Path>,
        limits: WikiLimits,
    ) -> Result<Self, WikiError> {
        if limits.max_chunk_bytes < 4
            || limits.max_page_bytes < limits.max_chunk_bytes
            || limits.max_page_bytes > 64 * 1024 * 1024
            || limits.max_chunks_per_page == 0
            || limits.max_pages == 0
        {
            return Err(WikiError::Limit {
                resource: "configuration",
                limit: 64 * 1024 * 1024,
            });
        }
        let root = root.as_ref();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(root)
            .map_err(|source| io_error(root, source))?;
        let root = root
            .canonicalize()
            .map_err(|source| io_error(root, source))?;
        let directory = Dir::open_ambient_dir(&root, ambient_authority())
            .map_err(|source| io_error(&root, source))?;
        let mut wiki = Self {
            root,
            directory,
            index: WikiIndex::open(index_path)?,
            limits,
        };
        wiki.synchronize()?;
        Ok(wiki)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn page_paths(&self) -> Result<Vec<String>, WikiError> {
        let mut pages = Vec::new();
        let mut pending = vec![PathBuf::new()];
        let mut entries = 0_usize;
        while let Some(relative) = pending.pop() {
            let entries_path = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &relative
            };
            let directory_entries = self
                .directory
                .read_dir(entries_path)
                .map_err(|source| io_error(self.root.join(&relative), source))?;
            for entry in directory_entries {
                let entry = entry.map_err(|source| io_error(self.root.join(&relative), source))?;
                entries = entries.saturating_add(1);
                if entries > self.limits.max_pages.saturating_mul(32) {
                    return Err(WikiError::Limit {
                        resource: "directory entries",
                        limit: self.limits.max_pages.saturating_mul(32),
                    });
                }
                let filename = entry.file_name();
                if filename.to_str().is_some_and(|name| name.starts_with('.')) {
                    continue;
                }
                let path = relative.join(filename);
                let kind = entry
                    .file_type()
                    .map_err(|source| io_error(self.root.join(&path), source))?;
                if kind.is_symlink() {
                    continue;
                }
                if kind.is_dir() {
                    if path.components().count() > 32 {
                        return Err(WikiError::Limit {
                            resource: "directory depth",
                            limit: 32,
                        });
                    }
                    pending.push(path);
                } else if kind.is_file() && path.extension().is_some_and(|value| value == "md") {
                    let name = path
                        .to_str()
                        .ok_or_else(|| WikiError::InvalidPath(path.display().to_string()))?;
                    pages.push(validate_page_path(name)?);
                    if pages.len() > self.limits.max_pages {
                        return Err(WikiError::Limit {
                            resource: "pages",
                            limit: self.limits.max_pages,
                        });
                    }
                }
            }
        }
        pages.sort_unstable();
        Ok(pages)
    }

    pub fn read_page(&self, path: &str) -> Result<WikiPage, WikiError> {
        let path = validate_page_path(path)?;
        let markdown = self.read_markdown(&path)?;
        parse_page(&path, markdown, &self.limits)
    }

    pub fn create_page(&mut self, path: &str, markdown: &str) -> Result<WikiPage, WikiError> {
        self.persist_page(path, markdown, false)
    }

    pub fn write_page(&mut self, path: &str, markdown: &str) -> Result<WikiPage, WikiError> {
        self.persist_page(path, markdown, true)
    }

    pub fn delete_page(&mut self, path: &str) -> Result<bool, WikiError> {
        let path = validate_page_path(path)?;
        self.reject_symlinks(&path)?;
        let removed = match self.directory.remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => return Err(io_error(self.root.join(&path), source)),
        };
        if removed {
            let parent = Path::new(&path).parent().unwrap_or_else(|| Path::new("."));
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            self.directory
                .open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| io_error(self.root.join(parent), source))?;
        }
        self.index.remove_page(&path)?;
        Ok(removed)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, WikiError> {
        self.index.search(query, limit)
    }

    pub fn backlinks(&self, path: &str, limit: usize) -> Result<Vec<WikiLink>, WikiError> {
        self.index.backlinks(path, limit)
    }

    pub fn synchronize(&mut self) -> Result<SyncReport, WikiError> {
        let paths = self.page_paths()?;
        let mut report = SyncReport::default();
        for path in &paths {
            let markdown = self.read_markdown(path)?;
            let hash = content_hash(markdown.as_bytes());
            if self.index.page_hash(path)?.as_deref() == Some(&hash) {
                report.unchanged_pages += 1;
                continue;
            }
            let page = parse_page(path, markdown, &self.limits)?;
            self.index.upsert_page(&page)?;
            report.indexed_pages += 1;
        }
        report.deleted_pages = self.index.retain_pages(paths.into_iter().collect())?;
        Ok(report)
    }

    pub fn rebuild(&mut self) -> Result<SyncReport, WikiError> {
        let paths = self.page_paths()?;
        let directory = &self.directory;
        let root = &self.root;
        let limits = &self.limits;
        let pages = paths.into_iter().map(|path| {
            let markdown = read_markdown(directory, root, &path, limits)?;
            parse_page(&path, markdown, limits)
        });
        self.index.rebuild(pages)
    }

    fn read_markdown(&self, path: &str) -> Result<String, WikiError> {
        self.reject_symlinks(path)?;
        read_markdown(&self.directory, &self.root, path, &self.limits)
    }

    fn reject_symlinks(&self, path: &str) -> Result<(), WikiError> {
        let mut relative = PathBuf::new();
        for component in Path::new(path).components() {
            relative.push(component);
            match self.directory.symlink_metadata(&relative) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(WikiError::InvalidPath(path.to_owned()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(source) => return Err(io_error(self.root.join(&relative), source)),
            }
        }
        Ok(())
    }

    pub(super) fn preflight_page_write(
        &self,
        path: &str,
        markdown: &str,
        replace: bool,
    ) -> Result<WikiPage, WikiError> {
        let path = validate_page_path(path)?;
        if markdown.len() > self.limits.max_page_bytes {
            return Err(WikiError::Limit {
                resource: "page bytes",
                limit: self.limits.max_page_bytes,
            });
        }
        let page = parse_page(&path, markdown.to_owned(), &self.limits)?;
        self.reject_symlinks(&path)?;
        let exists = match self.directory.symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => return Err(io_error(self.root.join(&path), source)),
        };
        if !exists && self.page_paths()?.len() >= self.limits.max_pages {
            return Err(WikiError::Limit {
                resource: "pages",
                limit: self.limits.max_pages,
            });
        }
        if exists && !replace {
            return Err(WikiError::AlreadyExists(path));
        }
        Ok(page)
    }

    fn persist_page(
        &mut self,
        path: &str,
        markdown: &str,
        replace: bool,
    ) -> Result<WikiPage, WikiError> {
        let page = self.preflight_page_write(path, markdown, replace)?;
        let path = page.path.as_str();
        let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
        if !parent.as_os_str().is_empty() {
            self.directory
                .create_dir_all(parent)
                .map_err(|source| io_error(self.root.join(parent), source))?;
        }
        let parent_directory = self
            .directory
            .open_dir(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })
            .map_err(|source| io_error(self.root.join(parent), source))?;
        let filename = Path::new(path)
            .file_name()
            .ok_or_else(|| WikiError::InvalidPath(path.to_owned()))?;
        let temporary_name = format!(".everything-{}.tmp", Uuid::new_v4());
        let write_result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = parent_directory
                .open_with(&temporary_name, &options)
                .map_err(|source| io_error(self.root.join(parent).join(&temporary_name), source))?;
            file.write_all(markdown.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|source| io_error(self.root.join(parent).join(&temporary_name), source))?;
            if replace {
                parent_directory
                    .rename(&temporary_name, &parent_directory, filename)
                    .map_err(|source| io_error(self.root.join(path), source))?;
            } else {
                parent_directory
                    .hard_link(&temporary_name, &parent_directory, filename)
                    .map_err(|source| {
                        if source.kind() == std::io::ErrorKind::AlreadyExists {
                            WikiError::AlreadyExists(path.to_owned())
                        } else {
                            io_error(self.root.join(path), source)
                        }
                    })?;
                parent_directory
                    .remove_file(&temporary_name)
                    .map_err(|source| {
                        io_error(self.root.join(parent).join(&temporary_name), source)
                    })?;
            }
            parent_directory
                .open(".")
                .and_then(|directory| directory.sync_all())
                .map_err(|source| io_error(self.root.join(parent), source))
        })();
        if let Err(error) = write_result {
            match parent_directory.remove_file(&temporary_name) {
                Ok(()) => {}
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(io_error(
                        self.root.join(parent).join(&temporary_name),
                        source,
                    ));
                }
            }
            return Err(error);
        }
        self.index.upsert_page(&page)?;
        Ok(page)
    }
}

pub fn parse_page(
    path: &str,
    markdown: String,
    limits: &WikiLimits,
) -> Result<WikiPage, WikiError> {
    let path = validate_page_path(path)?;
    if markdown.len() > limits.max_page_bytes || limits.max_chunk_bytes < 4 {
        return Err(WikiError::Limit {
            resource: "page bytes or chunk configuration",
            limit: limits.max_page_bytes,
        });
    }
    let line_offsets: Vec<usize> = std::iter::once(0)
        .chain(markdown.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect();
    let mut headings: Vec<Heading> = Vec::new();
    let mut active_heading: Option<Heading> = None;
    let mut active_link: Option<(String, String, usize)> = None;
    let mut links = Vec::new();
    let options = Options::ENABLE_WIKILINKS | Options::ENABLE_STRIKETHROUGH;
    for (event, range) in Parser::new_ext(&markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active_heading = Some(Heading {
                    level: level as usize,
                    offset: range.start,
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut heading) = active_heading.take() {
                    heading.text = heading.text.trim().to_owned();
                    if heading.text.len() > 4_096 {
                        return Err(WikiError::Limit {
                            resource: "heading bytes",
                            limit: 4_096,
                        });
                    }
                    headings.push(heading);
                    if headings.len() > limits.max_chunks_per_page {
                        return Err(WikiError::Limit {
                            resource: "headings per page",
                            limit: limits.max_chunks_per_page,
                        });
                    }
                }
            }
            Event::Start(Tag::Link {
                link_type: LinkType::WikiLink { .. },
                dest_url,
                ..
            }) => active_link = Some((dest_url.into_string(), String::new(), range.start)),
            Event::End(TagEnd::Link) => {
                if let Some((target, label, start)) = active_link.take() {
                    let (target_page, target_fragment) = resolve_link(&path, &target);
                    links.push(WikiLink {
                        raw_target: target,
                        target_page,
                        target_fragment,
                        label,
                        provenance: provenance(&path, start, range.end, &line_offsets),
                    });
                    if links.len() > 16_384 {
                        return Err(WikiError::Limit {
                            resource: "wiki links per page",
                            limit: 16_384,
                        });
                    }
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading) = &mut active_heading {
                    heading.text.push_str(&text);
                }
                if let Some((_, label, _)) = &mut active_link {
                    label.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(heading) = &mut active_heading {
                    heading.text.push(' ');
                }
                if let Some((_, label, _)) = &mut active_link {
                    label.push(' ');
                }
            }
            _ => {}
        }
    }
    let title = headings
        .first()
        .map(|heading| heading.text.clone())
        .unwrap_or_else(|| path.trim_end_matches(".md").to_owned());
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut occurrences: BTreeMap<String, usize> = BTreeMap::new();
    let preamble_end = headings
        .first()
        .map_or(markdown.len(), |heading| heading.offset);
    append_chunks(
        &mut chunks,
        ChunkSection {
            path: &path,
            anchor: "preamble",
            heading: "",
            markdown: &markdown,
            start: 0,
            end: preamble_end,
            line_offsets: &line_offsets,
        },
        limits,
    )?;
    for (index, heading) in headings.iter().enumerate() {
        while heading_stack
            .last()
            .is_some_and(|(level, _)| *level >= heading.level)
        {
            heading_stack.pop();
        }
        heading_stack.push((heading.level, heading.text.clone()));
        let heading_path = heading_stack
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" › ");
        let slug = heading
            .text
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let occurrence = occurrences.entry(slug.clone()).or_default();
        *occurrence += 1;
        let anchor = format!("heading:{slug}:{}", *occurrence);
        let end = headings
            .get(index + 1)
            .map_or(markdown.len(), |next| next.offset);
        append_chunks(
            &mut chunks,
            ChunkSection {
                path: &path,
                anchor: &anchor,
                heading: &heading_path,
                markdown: &markdown,
                start: heading.offset,
                end,
                line_offsets: &line_offsets,
            },
            limits,
        )?;
    }
    Ok(WikiPage {
        path,
        title,
        content_hash: content_hash(markdown.as_bytes()),
        markdown,
        chunks,
        links,
    })
}

struct Heading {
    level: usize,
    offset: usize,
    text: String,
}

struct ChunkSection<'a> {
    path: &'a str,
    anchor: &'a str,
    heading: &'a str,
    markdown: &'a str,
    start: usize,
    end: usize,
    line_offsets: &'a [usize],
}

fn append_chunks(
    chunks: &mut Vec<WikiChunk>,
    section: ChunkSection<'_>,
    limits: &WikiLimits,
) -> Result<(), WikiError> {
    let mut start = section.start;
    let mut part = 0;
    while start < section.end {
        let mut end = section
            .end
            .min(start.saturating_add(limits.max_chunk_bytes));
        while !section.markdown.is_char_boundary(end) {
            end -= 1;
        }
        if end < section.end {
            if let Some(newline) = section.markdown[start..end].rfind('\n') {
                if newline >= limits.max_chunk_bytes / 2 {
                    end = start + newline + 1;
                }
            }
        }
        let text = &section.markdown[start..end];
        if !text.trim().is_empty() {
            if chunks.len() >= limits.max_chunks_per_page {
                return Err(WikiError::Limit {
                    resource: "chunks per page",
                    limit: limits.max_chunks_per_page,
                });
            }
            let id =
                content_hash(format!("{}\0{}\0{part}", section.path, section.anchor).as_bytes());
            chunks.push(WikiChunk {
                id,
                heading: section.heading.to_owned(),
                markdown: text.to_owned(),
                provenance: provenance(section.path, start, end, section.line_offsets),
            });
        }
        part += 1;
        start = end;
    }
    Ok(())
}

fn resolve_link(source: &str, target: &str) -> (Option<String>, Option<String>) {
    let (page, fragment) = target
        .split_once('#')
        .map_or((target, None), |(page, fragment)| {
            (page, Some(fragment.to_owned()))
        });
    let page = if page.is_empty() {
        source.to_owned()
    } else if page.ends_with(".md") {
        page.to_owned()
    } else {
        format!("{page}.md")
    };
    (validate_page_path(&page).ok(), fragment)
}

fn provenance(path: &str, start: usize, end: usize, lines: &[usize]) -> Provenance {
    Provenance {
        page_path: path.to_owned(),
        start_line: lines.partition_point(|offset| *offset <= start),
        end_line: lines.partition_point(|offset| *offset <= end.saturating_sub(1).max(start)),
    }
}

pub(crate) fn validate_page_path(path: &str) -> Result<String, WikiError> {
    if path.is_empty()
        || path.len() > 1_024
        || path.contains(['\\', ':'])
        || path.split('/').any(|component| component == ".")
        || path.chars().any(char::is_control)
        || !path.ends_with(".md")
        || Path::new(path).components().count() > 32
    {
        return Err(WikiError::InvalidPath(path.to_owned()));
    }
    let mut normalized = Vec::new();
    for component in Path::new(path).components() {
        if let Component::Normal(value) = component {
            let value = value
                .to_str()
                .ok_or_else(|| WikiError::InvalidPath(path.to_owned()))?;
            if value.starts_with('.') {
                return Err(WikiError::InvalidPath(path.to_owned()));
            }
            normalized.push(value);
        } else {
            return Err(WikiError::InvalidPath(path.to_owned()));
        }
    }
    if normalized.is_empty() {
        return Err(WikiError::InvalidPath(path.to_owned()));
    }
    Ok(normalized.join("/"))
}

fn read_markdown(
    directory: &Dir,
    root: &Path,
    path: &str,
    limits: &WikiLimits,
) -> Result<String, WikiError> {
    let metadata = directory
        .metadata(path)
        .map_err(|source| io_error(root.join(path), source))?;
    if !metadata.is_file() || metadata.len() > limits.max_page_bytes as u64 {
        return Err(WikiError::Limit {
            resource: "page bytes or file type",
            limit: limits.max_page_bytes,
        });
    }
    let file = directory
        .open(path)
        .map_err(|source| io_error(root.join(path), source))?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error(root.join(path), source))?;
    if !metadata.is_file() || metadata.len() > limits.max_page_bytes as u64 {
        return Err(WikiError::Limit {
            resource: "page bytes or file type",
            limit: limits.max_page_bytes,
        });
    }
    let mut bytes = Vec::new();
    file.take(limits.max_page_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(root.join(path), source))?;
    if bytes.len() > limits.max_page_bytes {
        return Err(WikiError::Limit {
            resource: "page bytes",
            limit: limits.max_page_bytes,
        });
    }
    String::from_utf8(bytes).map_err(|_| WikiError::InvalidEncoding(path.to_owned()))
}

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> WikiError {
    WikiError::Io {
        path: path.into(),
        source,
    }
}
