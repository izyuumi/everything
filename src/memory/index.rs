use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::wiki::{Provenance, WikiError, WikiLink, WikiPage, validate_page_path};

const APPLICATION_ID: i32 = 0x4557_494b;
const SCHEMA_VERSION: i32 = 1;
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
            || (application_id == APPLICATION_ID && version != SCHEMA_VERSION)
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
                "CREATE TABLE wiki_pages (
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
                wiki_chunks.page_path, wiki_chunks.start_line, wiki_chunks.end_line
            FROM wiki_fts JOIN wiki_chunks ON wiki_chunks.id = wiki_fts.chunk_id
            WHERE wiki_fts MATCH ?1
            ORDER BY bm25(wiki_fts, 0.0, 0.0, 4.0, 2.0, 1.0), wiki_fts.chunk_id
            LIMIT ?2",
        )?;
        let hits = statement.query_map(params![expression, limit as i64], |row| {
            let excerpt: String = row.get(3)?;
            let mut bounded_excerpt: String = excerpt.chars().take(512).collect();
            if bounded_excerpt.len() < excerpt.len() {
                bounded_excerpt.push('…');
            }
            Ok(SearchHit {
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
            })
        })?;
        hits.collect::<Result<Vec<_>, _>>().map_err(WikiError::from)
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
