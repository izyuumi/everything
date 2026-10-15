use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use sha2::{Digest, Sha256};

use super::wiki::{Provenance, WikiChunk, WikiError, WikiLink, WikiPage, validate_page_path};

const APPLICATION_ID: i32 = 0x4557_494b;
const SCHEMA_VERSION: i32 = 2;
const MAX_BACKLINK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub indexed_pages: usize,
    pub unchanged_pages: usize,
    pub deleted_pages: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub chunk_id: String,
    pub title: String,
    pub heading: String,
    pub excerpt: String,
    pub score: f64,
    pub provenance: Provenance,
}

#[derive(Clone, Debug)]
pub struct IndexedChunk {
    pub chunk: WikiChunk,
    pub title: String,
    pub page_hash: String,
}

#[derive(Clone, Debug)]
pub struct ChunkPage {
    pub chunks: Vec<IndexedChunk>,
    pub next_after_id: Option<String>,
}

pub struct WikiIndex {
    connection: Connection,
}

impl WikiIndex {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WikiError> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let application_id: i32 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if (application_id != 0 && application_id != APPLICATION_ID)
            || version > SCHEMA_VERSION
            || (application_id == APPLICATION_ID && version == 0)
            || (application_id == 0 && version != 0)
        {
            return Err(WikiError::IncompatibleIndex);
        }
        if application_id == 0 {
            let existing_tables: i64 = connection.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if existing_tables != 0 {
                return Err(WikiError::IncompatibleIndex);
            }
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE wiki_settings (key TEXT PRIMARY KEY,value TEXT NOT NULL CHECK(length(CAST(value AS BLOB))<=1024));
                CREATE TABLE wiki_pages (
                    path TEXT PRIMARY KEY,
                    content_hash TEXT NOT NULL,
                    title TEXT NOT NULL,
                    indexed_at INTEGER NOT NULL
                );
                CREATE TABLE wiki_chunks (
                    id TEXT PRIMARY KEY,
                    page_path TEXT NOT NULL REFERENCES wiki_pages(path) ON DELETE CASCADE,
                    ordinal INTEGER NOT NULL,
                    heading TEXT NOT NULL,
                    markdown TEXT NOT NULL,
                    start_line INTEGER NOT NULL CHECK(start_line > 0),
                    end_line INTEGER NOT NULL CHECK(end_line >= start_line),
                    UNIQUE(page_path, ordinal)
                );
                CREATE TABLE wiki_links (
                    page_path TEXT NOT NULL REFERENCES wiki_pages(path) ON DELETE CASCADE,
                    target_page TEXT,
                    target_fragment TEXT,
                    raw_target TEXT NOT NULL,
                    label TEXT NOT NULL,
                    start_line INTEGER NOT NULL,
                    end_line INTEGER NOT NULL
                );
                CREATE INDEX wiki_links_target ON wiki_links(target_page, page_path, start_line);
                CREATE VIRTUAL TABLE wiki_fts USING fts5(
                    chunk_id UNINDEXED,
                    page_path UNINDEXED,
                    title,
                    heading,
                    body,
                    tokenize = 'unicode61 remove_diacritics 2'
                );",
            )?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        if application_id == APPLICATION_ID && version == 1 {
            let transaction = connection.transaction()?;
            transaction.execute_batch("CREATE TABLE wiki_settings (key TEXT PRIMARY KEY,value TEXT NOT NULL CHECK(length(CAST(value AS BLOB))<=1024));")?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { connection })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, WikiError> {
        if query.len() > 4_096 {
            return Err(WikiError::Limit {
                resource: "search query bytes",
                limit: 4_096,
            });
        }
        if limit > 100 {
            return Err(WikiError::Limit {
                resource: "search results",
                limit: 100,
            });
        }
        let terms: Vec<&str> = query
            .split_whitespace()
            .filter(|term| term.chars().any(char::is_alphanumeric))
            .collect();
        if terms.len() > 64 {
            return Err(WikiError::Limit {
                resource: "search terms",
                limit: 64,
            });
        }
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let expression = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut statement = self.connection.prepare(
            "SELECT wiki_fts.chunk_id, wiki_fts.title, wiki_fts.heading,
                snippet(wiki_fts, 4, '[', ']', ' … ', 48),
                bm25(wiki_fts, 0.0, 0.0, 4.0, 2.0, 1.0),
                wiki_chunks.page_path, wiki_chunks.start_line, wiki_chunks.end_line,
                typeof(wiki_fts.chunk_id)='text' AND typeof(wiki_fts.title)='text'
                    AND typeof(wiki_fts.heading)='text'
                    AND typeof(snippet(wiki_fts, 4, '[', ']', ' … ', 48))='text'
                    AND typeof(wiki_chunks.page_path)='text'
                    AND typeof(wiki_chunks.start_line)='integer'
                    AND typeof(wiki_chunks.end_line)='integer',
                length(CAST(wiki_fts.chunk_id AS BLOB))+length(CAST(wiki_fts.title AS BLOB))
                    +length(CAST(wiki_fts.heading AS BLOB))
                    +length(CAST(snippet(wiki_fts, 4, '[', ']', ' … ', 48) AS BLOB))
                    +length(CAST(wiki_chunks.page_path AS BLOB))+16
            FROM wiki_fts JOIN wiki_chunks ON wiki_chunks.id = wiki_fts.chunk_id
            WHERE wiki_fts MATCH ?1
            ORDER BY bm25(wiki_fts, 0.0, 0.0, 4.0, 2.0, 1.0), wiki_fts.chunk_id
            LIMIT ?2",
        )?;
        let mut rows = statement.query(params![expression, limit as i64])?;
        let mut hits = Vec::with_capacity(limit);
        let mut total_bytes = 0_usize;
        while let Some(row) = rows.next()? {
            let valid_types: i64 = row.get(8)?;
            if valid_types != 1 {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            let bytes: usize = row.get(9)?;
            total_bytes = total_bytes.saturating_add(bytes);
            if total_bytes > 16 * 1024 * 1024 {
                return Err(WikiError::Limit {
                    resource: "lexical result bytes",
                    limit: 16 * 1024 * 1024,
                });
            }
            let excerpt: String = row.get(3)?;
            let mut bounded_excerpt: String = excerpt.chars().take(512).collect();
            if bounded_excerpt.len() < excerpt.len() {
                bounded_excerpt.push('…');
            }
            hits.push(SearchHit {
                chunk_id: row.get(0)?,
                title: row.get(1)?,
                heading: row.get(2)?,
                excerpt: bounded_excerpt,
                score: row.get(4)?,
                provenance: Provenance {
                    page_path: row.get(5)?,
                    start_line: row.get(6)?,
                    end_line: row.get(7)?,
                },
            });
        }
        Ok(hits)
    }

    pub(crate) fn ensure_chunking_profile(&mut self, profile: &str) -> Result<(), WikiError> {
        if profile.len() > 1024 {
            return Err(WikiError::Limit {
                resource: "chunking profile bytes",
                limit: 1024,
            });
        }
        let current: Option<String> = self
            .connection
            .query_row(
                "SELECT value,length(CAST(value AS BLOB)) FROM wiki_settings WHERE key='chunking_profile'",
                [],
                |row| {let bytes:usize=row.get(1)?;if bytes>1024 {return Err(rusqlite::Error::InvalidQuery);}row.get(0)},
            )
            .optional()?;
        if current.as_deref() == Some(profile) {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM wiki_fts", [])?;
        transaction.execute("DELETE FROM wiki_pages", [])?;
        transaction.execute("INSERT INTO wiki_settings(key,value) VALUES('chunking_profile',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[profile])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn source_fingerprint(&self) -> Result<String, WikiError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut hash = Sha256::new();
        hash.update(b"everything.wiki-source.v2\0");
        hash.update(b"pulldown-cmark-0.13.4.heading-chunks-v1\0");
        let profile: Option<String> = transaction.query_row("SELECT value,length(CAST(value AS BLOB)) FROM wiki_settings WHERE key='chunking_profile' ",[],|row| {let bytes:usize=row.get(1)?;if bytes>1024 {return Err(rusqlite::Error::InvalidQuery);}row.get(0)}).optional()?;
        let profile = profile.as_deref().unwrap_or("unprofiled");
        hash.update((profile.len() as u64).to_le_bytes());
        hash.update(profile.as_bytes());
        let mut pages = 0_u64;
        {
            let mut statement = transaction.prepare("SELECT path,content_hash,length(CAST(path AS BLOB))+length(CAST(content_hash AS BLOB)) FROM wiki_pages ORDER BY path")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let bytes: usize = row.get(2)?;
                if bytes > 16 * 1024 * 1024 {
                    return Err(WikiError::Limit {
                        resource: "source fingerprint row bytes",
                        limit: 16 * 1024 * 1024,
                    });
                }
                for index in [0, 1] {
                    let value: String = row.get(index)?;
                    hash.update((value.len() as u64).to_le_bytes());
                    hash.update(value.as_bytes());
                }
                pages = pages.saturating_add(1);
            }
        }
        hash.update(pages.to_le_bytes());
        let mut chunks = 0_u64;
        {
            let mut statement=transaction.prepare("SELECT id,length(CAST(markdown AS BLOB)),start_line,end_line,length(CAST(id AS BLOB)) FROM wiki_chunks ORDER BY id")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let id_bytes: usize = row.get(4)?;
                if id_bytes != 64 {
                    return Err(WikiError::IncompatibleIndex);
                }
                let id: String = row.get(0)?;
                hash.update((id.len() as u64).to_le_bytes());
                hash.update(id.as_bytes());
                for index in [1, 2, 3] {
                    let value: u64 = row.get(index)?;
                    hash.update(value.to_le_bytes());
                }
                chunks = chunks.saturating_add(1);
            }
        }
        hash.update(chunks.to_le_bytes());
        transaction.commit()?;
        Ok(format!("{:x}", hash.finalize()))
    }

    pub fn chunks_after(
        &self,
        after: Option<&str>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ChunkPage, WikiError> {
        validate_chunk_page(limit, max_bytes)?;
        if let Some(id) = after {
            validate_chunk_id(id)?;
        }
        let mut statement = self.connection.prepare(&format!("SELECT {CHUNK_COLUMNS} FROM wiki_chunks c JOIN wiki_pages p ON p.path=c.page_path WHERE (?1 IS NULL OR c.id>?1) ORDER BY c.id LIMIT ?2"))?;
        let mut rows = statement.query(params![after, (limit + 1) as i64])?;
        let mut chunks = Vec::new();
        let mut bytes = 0_usize;
        let mut next_after_id = None;
        while let Some(row) = rows.next()? {
            let size: usize = row.get(8)?;
            if chunks.len() >= limit || bytes.saturating_add(size) > max_bytes {
                if chunks.is_empty() {
                    return Err(WikiError::Limit {
                        resource: "individual indexed chunk bytes",
                        limit: max_bytes,
                    });
                }
                next_after_id = chunks
                    .last()
                    .map(|value: &IndexedChunk| value.chunk.id.clone());
                break;
            }
            bytes += size;
            chunks.push(indexed_chunk(row)?);
        }
        Ok(ChunkPage {
            chunks,
            next_after_id,
        })
    }

    pub fn chunks_by_ids(
        &self,
        ids: &[String],
        max_bytes: usize,
    ) -> Result<Vec<IndexedChunk>, WikiError> {
        validate_chunk_page(ids.len().max(1), max_bytes)?;
        let mut statement = self.connection.prepare(&format!("SELECT {CHUNK_COLUMNS} FROM wiki_chunks c JOIN wiki_pages p ON p.path=c.page_path WHERE c.id=?1"))?;
        let mut seen = BTreeSet::new();
        let mut chunks = Vec::new();
        let mut bytes = 0_usize;
        for id in ids {
            validate_chunk_id(id)?;
            if !seen.insert(id) {
                continue;
            }
            let mut rows = statement.query([id])?;
            if let Some(row) = rows.next()? {
                let size: usize = row.get(8)?;
                bytes = bytes.saturating_add(size);
                if bytes > max_bytes {
                    return Err(WikiError::Limit {
                        resource: "indexed chunk page bytes",
                        limit: max_bytes,
                    });
                }
                chunks.push(indexed_chunk(row)?);
            }
        }
        Ok(chunks)
    }

    pub fn page_hash(&self, path: &str) -> Result<Option<String>, WikiError> {
        let path = validate_page_path(path)?;
        self.connection
            .query_row(
                "SELECT content_hash FROM wiki_pages WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()
            .map_err(WikiError::from)
    }

    pub fn backlinks(&self, path: &str, limit: usize) -> Result<Vec<WikiLink>, WikiError> {
        let path = validate_page_path(path)?;
        if limit > 1_000 {
            return Err(WikiError::Limit {
                resource: "backlink results",
                limit: 1_000,
            });
        }
        let mut statement = self.connection.prepare(
            "SELECT raw_target, target_page, target_fragment, label, page_path, start_line, end_line,
                    length(CAST(raw_target AS BLOB)) + length(CAST(target_page AS BLOB)) +
                    coalesce(length(CAST(target_fragment AS BLOB)), 0) +
                    length(CAST(label AS BLOB)) + length(CAST(page_path AS BLOB))
             FROM wiki_links WHERE target_page = ?1 ORDER BY page_path, start_line, rowid LIMIT ?2",
        )?;
        let mut rows = statement.query(params![path, limit as i64])?;
        let mut links = Vec::with_capacity(limit);
        let mut materialized_bytes = limit * size_of::<WikiLink>();
        while let Some(row) = rows.next()? {
            let row_bytes: usize = row.get(7)?;
            materialized_bytes = materialized_bytes.saturating_add(row_bytes);
            if materialized_bytes > MAX_BACKLINK_BYTES {
                return Err(WikiError::Limit {
                    resource: "backlink page bytes",
                    limit: MAX_BACKLINK_BYTES,
                });
            }
            links.push(WikiLink {
                raw_target: row.get(0)?,
                target_page: row.get(1)?,
                target_fragment: row.get(2)?,
                label: row.get(3)?,
                provenance: Provenance {
                    page_path: row.get(4)?,
                    start_line: row.get(5)?,
                    end_line: row.get(6)?,
                },
            });
        }
        Ok(links)
    }

    pub(crate) fn upsert_page(&mut self, page: &WikiPage) -> Result<(), WikiError> {
        let transaction = self.connection.transaction()?;
        remove_page(&transaction, &page.path)?;
        insert_page(&transaction, page)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn remove_page(&mut self, path: &str) -> Result<(), WikiError> {
        let transaction = self.connection.transaction()?;
        remove_page(&transaction, path)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn retain_pages(&mut self, paths: BTreeSet<String>) -> Result<usize, WikiError> {
        let transaction = self.connection.transaction()?;
        let deleted = {
            let mut statement = transaction.prepare("SELECT path FROM wiki_pages")?;
            let indexed_paths = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut deleted = Vec::new();
            for indexed_path in indexed_paths {
                let path = indexed_path?;
                if !paths.contains(&path) {
                    deleted.push(path);
                }
            }
            deleted
        };
        for path in &deleted {
            remove_page(&transaction, path)?;
        }
        transaction.commit()?;
        Ok(deleted.len())
    }

    pub(crate) fn rebuild(
        &mut self,
        pages: impl IntoIterator<Item = Result<WikiPage, WikiError>>,
    ) -> Result<SyncReport, WikiError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM wiki_fts", [])?;
        transaction.execute("DELETE FROM wiki_pages", [])?;
        let mut report = SyncReport::default();
        for page in pages {
            insert_page(&transaction, &page?)?;
            report.indexed_pages += 1;
        }
        transaction.commit()?;
        Ok(report)
    }
}

fn remove_page(transaction: &Transaction<'_>, path: &str) -> Result<(), rusqlite::Error> {
    transaction.execute("DELETE FROM wiki_fts WHERE page_path = ?1", [path])?;
    transaction.execute("DELETE FROM wiki_pages WHERE path = ?1", [path])?;
    Ok(())
}

fn insert_page(transaction: &Transaction<'_>, page: &WikiPage) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO wiki_pages(path, content_hash, title, indexed_at)
         VALUES (?1, ?2, ?3, unixepoch())",
        params![page.path, page.content_hash, page.title],
    )?;
    let mut chunks = transaction.prepare_cached(
        "INSERT INTO wiki_chunks(id, page_path, ordinal, heading, markdown, start_line, end_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut fts = transaction.prepare_cached(
        "INSERT INTO wiki_fts(chunk_id, page_path, title, heading, body)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (ordinal, chunk) in page.chunks.iter().enumerate() {
        chunks.execute(params![
            chunk.id,
            page.path,
            ordinal as i64,
            chunk.heading,
            chunk.markdown,
            chunk.provenance.start_line as i64,
            chunk.provenance.end_line as i64,
        ])?;
        fts.execute(params![
            chunk.id,
            page.path,
            page.title,
            chunk.heading,
            chunk.markdown,
        ])?;
    }
    let mut links = transaction.prepare_cached(
        "INSERT INTO wiki_links(page_path, target_page, target_fragment, raw_target, label,
                                start_line, end_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for link in &page.links {
        links.execute(params![
            page.path,
            link.target_page,
            link.target_fragment,
            link.raw_target,
            link.label,
            link.provenance.start_line as i64,
            link.provenance.end_line as i64,
        ])?;
    }
    Ok(())
}

const CHUNK_COLUMNS: &str = "c.id,p.title,p.content_hash,c.heading,c.markdown,c.page_path,c.start_line,c.end_line,length(CAST(c.id AS BLOB))+length(CAST(p.title AS BLOB))+length(CAST(p.content_hash AS BLOB))+length(CAST(c.heading AS BLOB))+length(CAST(c.markdown AS BLOB))+length(CAST(c.page_path AS BLOB))+256";

fn indexed_chunk(row: &Row<'_>) -> Result<IndexedChunk, rusqlite::Error> {
    Ok(IndexedChunk {
        title: row.get(1)?,
        page_hash: row.get(2)?,
        chunk: WikiChunk {
            id: row.get(0)?,
            heading: row.get(3)?,
            markdown: row.get(4)?,
            provenance: Provenance {
                page_path: row.get(5)?,
                start_line: row.get(6)?,
                end_line: row.get(7)?,
            },
        },
    })
}

fn validate_chunk_page(limit: usize, max_bytes: usize) -> Result<(), WikiError> {
    if !(1..=256).contains(&limit) {
        return Err(WikiError::Limit {
            resource: "indexed chunk page rows",
            limit: 256,
        });
    }
    if !(1_024..=16 * 1024 * 1024).contains(&max_bytes) {
        return Err(WikiError::Limit {
            resource: "indexed chunk page bytes",
            limit: 16 * 1024 * 1024,
        });
    }
    Ok(())
}

fn validate_chunk_id(value: &str) -> Result<(), WikiError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    {
        return Err(WikiError::InvalidPath("invalid indexed chunk ID".into()));
    }
    Ok(())
}
