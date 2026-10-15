# Markdown wiki and lexical retrieval

Issue [#6](https://github.com/izyuumi/everything/issues/6) implements the synchronous `memory::Wiki` service. Markdown files are authoritative. The SQLite index is disposable derived data, separate from the host's execution store. No model inference, embedding, consolidation job, filesystem watcher, or background indexing task is part of this foundation.

## Responsibilities and flow

`src/memory/wiki.rs` owns capability-confined page I/O, Markdown parsing, bounded chunks, link resolution, provenance, content hashing, and synchronization. `src/memory/index.rs` owns one SQLite connection, schema initialization, transactional page replacement, FTS5 ranking, backlink lookup, and index rebuild. Callers using an async runtime must run these synchronous disk and SQLite operations on a blocking boundary. A host should serialize access to one `Wiki` owner; the type requires mutable access for mutations.

`Wiki::open(root, index_path)` creates the wiki root if necessary, opens the independent index, and synchronizes the current files before returning. The index's parent directory must already exist. Use a private host data directory for the index, which contains page content. New wiki directories use mode 0700 and new page files use mode 0600 on Unix; existing directories retain their permissions.

Every synchronization traverses the bounded tree and reads and SHA-256 hashes each Markdown page. An unchanged hash avoids parsing and rewriting that page's index rows. Changed pages are replaced transactionally. Rows for deleted pages are removed after a successful scan. A failed incremental pass can leave previously processed pages updated; the next successful synchronization converges to the canonical files. Startup therefore recovers an index that lagged a prior successful file write.

`Wiki::rebuild()` reads and parses all discovered files inside one index transaction. Readers retain the previous SQLite snapshot until commit. A read, parser, or SQLite error rolls the transaction back. Rebuild does not rewrite Markdown. The schema is identified by an application ID and version; unrelated databases and unsupported schema versions are rejected without taking over their tables. A corrupt database is reported; automatic deletion of database files is intentionally absent. A caller can select a fresh index path and rebuild from Markdown.

## Page API and filesystem boundary

`create_page(path, markdown)` refuses to replace an existing page. `write_page(path, markdown)` atomically creates or replaces a page. `delete_page(path)` removes the canonical file and then its index rows, returning whether a file existed. Writes parse and validate before creating a temporary file in the destination directory. The file is synchronized before atomic rename, or an atomic no-clobber hard link for creation; the directory is synchronized afterward. Temporary files are removed on handled failures. An interrupted process can leave a hidden temporary file, which discovery ignores. A filesystem mutation and SQLite transaction cannot be atomic together: an index error after a file mutation is reported, and synchronization repairs the derived state.

Page names are relative, UTF-8, root-relative paths ending in lowercase `.md`. Absolute paths, parent traversal, dot components, backslashes, colons, control characters, and hidden components are rejected. Discovery skips hidden entries and symlinks. Explicit page operations reject observed symlinks. `cap-std` confines actual file operations to the opened directory capability even if the filesystem changes between validation and use. The wiki directory is a trusted local data location; this is not a sandbox for arbitrary shell execution. Concurrent external editors can replace canonical content, so callers must synchronize before relying on indexed external changes. Cross-process conflict resolution and consolidation serialization belong to later runtime integration.

Defaults bound pages to 2 MiB each, 10,000 pages, 4,096 chunks per page, 16 KiB per chunk, 16,384 links per page, 4 KiB per heading, 32 directory levels, and 1,024 bytes per page path. Discovery also bounds total directory entries. `WikiLimits` can adjust the principal page/chunk limits; page sizes above 64 MiB and invalid chunk configurations are rejected. Search accepts at most 4 KiB and 64 whitespace-separated terms, returns at most 100 results, and bounds excerpts to 512 characters. Backlink lookup returns at most 1,000 occurrences.

## Chunk, link, and provenance contract

`pulldown-cmark` recognizes ATX and Setext headings. Text before the first heading is a preamble section. Each heading starts a section ending at the next heading; its display heading contains the current heading ancestry. Oversized sections split on a nearby newline where possible and otherwise on a UTF-8 character boundary. No content is inserted into the authoritative Markdown.

A chunk ID is SHA-256 over the canonical page path, the case-folded heading text with its occurrence ordinal, and its section part ordinal. Preamble IDs use a separate marker. This keeps IDs stable under ordinary body edits and makes repeated identical headings distinct. Renaming a page or heading changes its ID. Inserting an identical earlier heading or moving content across chunk boundaries can change later occurrence or part identities. Chunk IDs do not promise persistent identity through arbitrary document restructuring.

`WikiPage` returns source Markdown, its SHA-256 content hash, title, chunks, and links. `WikiChunk` returns ID, heading ancestry, source Markdown, and `Provenance`. Provenance is a canonical page path with one-based inclusive start and end lines. Consumers must preserve it when presenting excerpts. A page title is its first parsed heading, falling back to its path without `.md`.

`[[target]]` and `[[target#fragment|label]]` become `WikiLink` records with source provenance. Code spans and fenced code do not create links. Wiki targets are root-relative, receive `.md` if omitted, and can refer to a fragment within the same page using `[[#fragment]]`. Invalid destinations retain their raw target and have no resolved target page; missing destinations remain representable for later linting. Backlinks query indexed occurrences by canonical target page without requiring that the target already exists.

## Derived SQLite schema and search

`wiki_pages` stores path, title, content hash, and indexing time. `wiki_chunks` stores stable IDs, page-local ordinal, heading, Markdown, and source line ranges. `wiki_links` stores raw and resolved destinations, labels, and source line ranges. Foreign keys cascade page deletion. `wiki_fts` stores searchable title, heading, and chunk body plus unindexed chunk and page IDs. Each transaction updates all affected tables together.

The index uses SQLite WAL, foreign keys, a five-second busy timeout, and synchronous `NORMAL`. Search treats user input as literal quoted terms combined with `AND`, rather than accepting arbitrary FTS syntax. Unicode61 tokenization removes diacritics. FTS5 BM25 weights title, heading, and body by 4, 2, and 1; a lower score ranks earlier, with chunk ID breaking ties. `SearchHit` includes title, heading, a bounded highlighted excerpt, score, chunk ID, and source provenance. Search reads the last synchronized index and never runs inference.

## Validation and remaining work

Formatting, workspace compilation, strict Clippy, and release compilation are required for this foundation. Disposable manual runtime walkthroughs exercised create/edit/delete, duplicate headings and stable body-edit IDs, link provenance, stale-term removal, unchanged hashing, external edit/deletion synchronization, startup recovery, rebuild, and rejected traversal and symlink writes. Embeddings and hybrid retrieval remain gated on their own issues. Automated parser, filesystem race, crash, migration, and retrieval coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32); manual checks do not establish those acceptance criteria.
