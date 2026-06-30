//! SQLite storage: one file holding two indexes, an FTS5 table for keywords and
//! a per-row f32 vector blob for semantics (which we cosine-match by brute force).
//! We stick with the default rollback journal on purpose, so the store stays a
//! single, copyable `store.db` (desc.md §11).

use crate::embed;
use crate::util::{now, AppError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub struct Store {
    pub conn: Connection,
}

#[derive(Debug, Clone)]
pub struct NoteRow {
    /// Short lowercase-hex public id, written in the note's md frontmatter
    /// (and equal to the filename stem). It survives an index rebuild (`reindex`).
    pub id: String,
    pub body: String,
    pub tags: String,
    pub source: Option<String>,
    pub origin: Option<String>,
    pub kind: String,
    #[allow(dead_code)] // stored for completeness; output uses created_iso
    pub created_at: i64,
    pub created_iso: String,
}

#[derive(Debug, Clone)]
pub struct LogRow {
    pub op: String,
    pub detail: Option<String>,
    pub created_iso: String,
}

/// One derived graph edge. `dst_id` is the note it resolved to, or `None` if it's
/// dangling; `dst_raw` is the target exactly as it was written in the md.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRow {
    pub src_id: String,
    pub predicate: String,
    pub dst_id: Option<String>,
    pub dst_raw: String,
    pub source: String,
}

/// One source-code symbol definition (feature `code`). Mirrors a row of
/// `code_symbols`; `symbol_id` is the content-addressed key other edges resolve to.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeSymbolRow {
    pub symbol_id: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: i64,
    pub signature: Option<String>,
    pub is_test: bool,
}

/// One source-code graph edge (feature `code`): `defines` (file → symbol) or
/// `uses` (symbol → symbol). `dst` is the resolved symbol_id or `None` (dangling).
#[derive(Debug, Clone, PartialEq)]
pub struct CodeEdgeRow {
    pub src: String,
    pub predicate: String,
    pub dst: Option<String>,
    pub dst_raw: String,
    pub line: i64,
}

#[derive(Debug, Clone)]
pub struct ImportRow {
    /// Canonical `imports/<name>` path (relative to the memory folder).
    pub source: String,
    /// Original filename, for the chunk `origin` breadcrumb.
    pub orig_name: String,
    pub tags: String,
    pub chunks: i64,
    pub content_hash: String,
    pub created_iso: String,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS notes (
  id          TEXT NOT NULL UNIQUE,
  body        TEXT NOT NULL,
  tags        TEXT NOT NULL DEFAULT '',
  source      TEXT,
  origin      TEXT,
  kind        TEXT NOT NULL DEFAULT 'note',
  slug        TEXT,                 -- optional human handle for graph links (derived)
  created_at  INTEGER NOT NULL,
  created_iso TEXT NOT NULL,
  embedding   BLOB
);

CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
  body, tags, origin,
  tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS oplog (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  op          TEXT NOT NULL,
  detail      TEXT,
  created_at  INTEGER NOT NULL,
  created_iso TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS imports (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  source       TEXT NOT NULL UNIQUE,
  orig_name    TEXT NOT NULL DEFAULT '',
  tags         TEXT NOT NULL DEFAULT '',
  chunks       INTEGER NOT NULL,
  content_hash TEXT NOT NULL DEFAULT '',
  created_at   INTEGER NOT NULL,
  created_iso  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Per-file change-detection for incremental `reindex` (derived; wiped on --all).
-- One row per source-of-truth file: notes/<id>.md or imports/<name>.
CREATE TABLE IF NOT EXISTS sync (
  path         TEXT PRIMARY KEY,   -- relative path under the memory folder
  kind         TEXT NOT NULL,      -- 'note' | 'import'
  ref          TEXT,               -- note id (note) or import source key (import)
  content_hash TEXT NOT NULL,
  mtime        INTEGER NOT NULL,
  indexed_at   INTEGER NOT NULL
);

-- Knowledge graph, DERIVED from md (frontmatter `relations` + body `[[links]]`).
-- Bulk-rebuilt by reindex / on each remember; wiped on --all. dst_id is the
-- resolved note id, or NULL for a dangling target; dst_raw is the verbatim human
-- target written in md, so wipe+rebuild reproduces the identical table.
CREATE TABLE IF NOT EXISTS edges (
  src_id    TEXT NOT NULL,         -- hex id of the note the edge starts from
  predicate TEXT NOT NULL,         -- lower-cased verb; 'links_to' for wiki-links
  dst_id    TEXT,                  -- resolved hex id, or NULL = dangling
  dst_raw   TEXT NOT NULL,         -- verbatim human target as written in md
  source    TEXT NOT NULL CHECK(source IN ('relation','wikilink')),
  PRIMARY KEY (src_id, predicate, dst_raw, source)
);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src_id);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst_id);

-- Doc↔code anchors, DERIVED from a note's md `code:` field (wiped on --all,
-- rebuilt by reindex). Each row is "note documents code-symbol <name>". It is its
-- OWN table, not an `edges` row, so these anchors never leak into the notes-graph
-- traversals (`related`/`backlinks`); they're resolved by NAME against code_symbols
-- live at recall time (a name survives line shifts; a code_symbols.symbol_id would
-- not). A separate table also means an existing store.db gains it on next open
-- (CREATE IF NOT EXISTS) with no column migration.
CREATE TABLE IF NOT EXISTS note_code_refs (
  note_id TEXT NOT NULL,             -- hex id of the documenting note
  name    TEXT NOT NULL,             -- referenced code-symbol name (verbatim)
  PRIMARY KEY (note_id, name)
);
CREATE INDEX IF NOT EXISTS note_code_refs_note ON note_code_refs(note_id);
CREATE INDEX IF NOT EXISTS note_code_refs_name ON note_code_refs(name);

-- ============================================================================
-- SOURCE-CODE GRAPH (feature `code`) — DELIBERATELY SEPARATE from the notes
-- graph above. Code lives in its own code_* tables and is queried only through
-- `cm map`; it never mixes into notes/notes_fts/edges, so `recall`/`related`
-- stay byte-identical. All three tables are pure derived cache over the live
-- working tree (the source files themselves are the truth; nothing is copied
-- into the memory folder), so they wipe+rebuild from disk like the rest.
-- ============================================================================

-- One row per indexed source file. content_hash drives incremental `cm map`
-- (an unchanged file is skipped) exactly like `sync` does for notes.
CREATE TABLE IF NOT EXISTS code_files (
  path         TEXT PRIMARY KEY,   -- path relative to the mapped root, '/'-separated
  lang         TEXT NOT NULL,      -- registry name: 'rust', 'python', ...
  content_hash TEXT NOT NULL,
  mtime        INTEGER NOT NULL,
  indexed_at   INTEGER NOT NULL
);

-- One row per symbol DEFINITION (a tree-sitter-tags @definition.*). symbol_id is
-- content-addressed (hash of path+kind+name+line) so it's stable across rebuilds.
CREATE TABLE IF NOT EXISTS code_symbols (
  symbol_id  TEXT PRIMARY KEY,
  path       TEXT NOT NULL,        -- file it's defined in (FK-ish -> code_files.path)
  name       TEXT NOT NULL,        -- 'Store', 'upsert_note'
  kind       TEXT NOT NULL,        -- tags syntax_type: 'function','struct','class',...
  line       INTEGER NOT NULL,     -- 1-based line of the definition
  signature  TEXT,                 -- first line of the definition, trimmed
  is_test    INTEGER NOT NULL DEFAULT 0  -- 1 = defined in test code (hidden by default)
);
CREATE INDEX IF NOT EXISTS code_symbols_name ON code_symbols(name);
CREATE INDEX IF NOT EXISTS code_symbols_path ON code_symbols(path);

-- Graph edge in the code graph. predicate is 'defines' (file path -> symbol) or
-- 'uses' (referencing symbol_id -> referenced symbol). dst is the resolved target
-- symbol_id, or NULL when a referenced name doesn't resolve (dangling, revived on
-- a later map pass — same first-class-dangling trick as the notes graph). dst_raw
-- keeps the verbatim referenced name so wipe+rebuild reproduces the table.
CREATE TABLE IF NOT EXISTS code_edges (
  src       TEXT NOT NULL,         -- file path (defines) or symbol_id (uses)
  predicate TEXT NOT NULL CHECK(predicate IN ('defines','uses')),
  dst       TEXT,                  -- resolved symbol_id, or NULL = dangling
  dst_raw   TEXT NOT NULL,         -- verbatim target name / symbol_id
  line      INTEGER NOT NULL,      -- 1-based line where the edge originates
  PRIMARY KEY (src, predicate, dst_raw, line)
);
CREATE INDEX IF NOT EXISTS code_edges_src ON code_edges(src);
CREATE INDEX IF NOT EXISTS code_edges_dst ON code_edges(dst);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    /// Create the schema in a fresh file (used by `init`).
    pub fn create(path: &Path) -> Result<()> {
        let _ = Store::open(path)?;
        Ok(())
    }

    // ---- meta ------------------------------------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- notes -----------------------------------------------------------

    /// Insert a fresh note/chunk with a freshly minted id and the current time,
    /// keeping its FTS + vector rows in sync, and return the new hex id. This is
    /// really just a convenience for tests; in production the caller mints the id
    /// and sets `created` itself (notes in `remember`, chunks in `import`).
    #[cfg(test)]
    pub fn insert_note(
        &self,
        body: &str,
        tags: &str,
        source: Option<&str>,
        origin: Option<&str>,
        kind: &str,
        embedding: &[f32],
    ) -> Result<String> {
        let id = self.fresh_id()?;
        let (epoch, iso) = now();
        self.upsert_note(
            &id, body, tags, source, origin, kind, epoch, &iso, embedding,
        )?;
        Ok(id)
    }

    /// Insert-or-replace a note/chunk by its public hex id, keeping our hand-rolled
    /// `notes` <-> `notes_fts` sync honest. The caller owns the id and `created`
    /// (the md frontmatter is the source of truth), so a rebuild lands on the same
    /// row. When we replace an existing id we delete the old row and its FTS entry
    /// (by rowid) first, so no FTS rowid is ever left pointing at nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_note(
        &self,
        id: &str,
        body: &str,
        tags: &str,
        source: Option<&str>,
        origin: Option<&str>,
        kind: &str,
        created_at: i64,
        created_iso: &str,
        embedding: &[f32],
    ) -> Result<()> {
        if let Some(rowid) = self.rowid_of(id)? {
            self.conn
                .execute("DELETE FROM notes WHERE rowid = ?1", params![rowid])?;
            self.conn
                .execute("DELETE FROM notes_fts WHERE rowid = ?1", params![rowid])?;
        }
        let blob = embed::encode(embedding);
        self.conn.execute(
            "INSERT INTO notes(id, body, tags, source, origin, kind, created_at, created_iso, embedding)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, body, tags, source, origin, kind, created_at, created_iso, blob],
        )?;
        let rowid = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO notes_fts(rowid, body, tags, origin) VALUES(?1, ?2, ?3, ?4)",
            params![rowid, body, tags, origin.unwrap_or("")],
        )?;
        Ok(())
    }

    /// The internal integer rowid behind a public hex id; this is the FTS join key.
    fn rowid_of(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT rowid FROM notes WHERE id = ?1", params![id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?)
    }

    /// Mint a fresh 8-hex-char id that no note is already using (we check).
    pub fn fresh_id(&self) -> Result<String> {
        for _ in 0..10_000 {
            let cand = mint_hex();
            if self.rowid_of(&cand)?.is_none() {
                return Ok(cand);
            }
        }
        Err(AppError::new("could not mint a unique note id"))
    }

    pub fn get(&self, id: &str) -> Result<Option<NoteRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, body, tags, source, origin, kind, created_at, created_iso
                 FROM notes WHERE id = ?1",
                params![id],
                map_note,
            )
            .optional()?;
        Ok(row)
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        let Some(rowid) = self.rowid_of(id)? else {
            return Ok(false);
        };
        self.conn
            .execute("DELETE FROM notes WHERE rowid = ?1", params![rowid])?;
        self.conn
            .execute("DELETE FROM notes_fts WHERE rowid = ?1", params![rowid])?;
        Ok(true)
    }

    /// Most recent notes first. We order by the internal `rowid` (i.e. insertion
    /// order), because the public hex id is random and tells you nothing about age.
    pub fn list(&self, recent: usize) -> Result<Vec<NoteRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, body, tags, source, origin, kind, created_at, created_iso
             FROM notes ORDER BY rowid DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![recent as i64], map_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every note in insertion order (export uses this when there's no query filter).
    pub fn all(&self) -> Result<Vec<NoteRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, body, tags, source, origin, kind, created_at, created_iso
             FROM notes ORDER BY rowid ASC",
        )?;
        let rows = stmt
            .query_map([], map_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Set (or clear) a note's optional graph slug. reindex calls this right after
    /// `upsert_note`; the slug comes straight from the note's md frontmatter.
    pub fn set_note_slug(&self, id: &str, slug: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET slug = ?2 WHERE id = ?1",
            params![id, slug],
        )?;
        Ok(())
    }

    /// Replace a note's code-graph anchors (the `code:` md field) with `names`.
    /// Delete-then-insert so the table mirrors the md exactly; reindex calls this
    /// right after `upsert_note`, `remember` right after the slug. Blank names are
    /// skipped, duplicates collapse on the (note_id, name) primary key.
    pub fn set_note_code_refs(&self, id: &str, names: &[String]) -> Result<()> {
        self.delete_note_code_refs(id)?;
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO note_code_refs(note_id, name) VALUES(?1, ?2)",
                params![id, name],
            )?;
        }
        Ok(())
    }

    /// Drop all of a note's code-graph anchors (on `forget`, or before a rewrite).
    pub fn delete_note_code_refs(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM note_code_refs WHERE note_id = ?1", params![id])?;
        Ok(())
    }

    /// The code-symbol names a note documents (its `code:` anchors), name-sorted for
    /// a deterministic, byte-stable recall row. Empty when the note documents none.
    pub fn note_code_refs(&self, id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM note_code_refs WHERE note_id = ?1 ORDER BY name")?;
        let rows = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The ids of every note that documents the code symbol `name` (the reverse of
    /// `note_code_refs`, for `cm backlinks --symbol <name>` — "which docs cover this
    /// symbol"). Id-sorted for a deterministic result.
    pub fn notes_documenting(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT note_id FROM note_code_refs WHERE name = ?1 ORDER BY note_id")?;
        let rows = stmt
            .query_map(params![name], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(id, slug)` for every note that has a non-empty slug, ordered by id
    /// ascending so a duplicate slug resolves to the lowest id (see
    /// `graph::build_slug_map`).
    pub fn note_slugs(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug FROM notes
             WHERE kind = 'note' AND slug IS NOT NULL AND slug <> '' ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All note ids as a set (used to resolve `id:`-prefixed graph targets).
    pub fn note_ids(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM notes WHERE kind = 'note'")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        Ok(rows)
    }

    // ---- search ----------------------------------------------------------

    /// FTS5 keyword search. `match_expr` has to be a valid FTS5 MATCH string.
    /// Returns (public hex id, bm25 rank), where a lower rank is a better match.
    /// We join `notes` on the internal rowid to get the public id back; if an FTS
    /// row somehow has no `notes` row behind it (shouldn't happen, given we keep
    /// the two in sync by hand) it just drops out here.
    pub fn fts_search(&self, match_expr: &str, limit: usize) -> Result<Vec<(String, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, bm25(notes_fts) AS rank
             FROM notes_fts JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![match_expr, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Ids whose metadata passes an optional pre-filter: `tag` has to be one of a
    /// note's comma tags (case-insensitive), and `origin_prefix` has to be a prefix
    /// of its `origin`. recall uses this to narrow candidates before scoring (plan
    /// R4). We just filter in Rust over a single scan, which is plenty for a
    /// brute-force store; worth revisiting alongside a real vector index once the
    /// KB gets big.
    pub fn ids_matching(
        &self,
        tag: Option<&str>,
        origin_prefix: Option<&str>,
    ) -> Result<std::collections::HashSet<String>> {
        let want_tag = tag.map(|t| t.to_lowercase());
        let mut stmt = self.conn.prepare("SELECT id, tags, origin FROM notes")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            let (id, tags, origin) = row?;
            if let Some(t) = &want_tag {
                let has = tags
                    .split(',')
                    .map(|x| x.trim().to_lowercase())
                    .any(|x| &x == t);
                if !has {
                    continue;
                }
            }
            if let Some(pfx) = origin_prefix {
                match &origin {
                    Some(o) if o.starts_with(pfx) => {}
                    _ => continue,
                }
            }
            set.insert(id);
        }
        Ok(set)
    }

    /// Every note paired with its decoded embedding, for brute-force vector scoring.
    pub fn all_embeddings(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding FROM notes WHERE embedding IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| {
                let id: String = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                Ok((id, embed::decode(&blob)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- log & imports ---------------------------------------------------

    pub fn log_op(&self, op: &str, detail: Option<&str>) -> Result<()> {
        let (epoch, iso) = now();
        self.conn.execute(
            "INSERT INTO oplog(op, detail, created_at, created_iso) VALUES(?1, ?2, ?3, ?4)",
            params![op, detail, epoch, iso],
        )?;
        Ok(())
    }

    pub fn recent_logs(&self, n: usize) -> Result<Vec<LogRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT op, detail, created_iso FROM oplog ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt
            .query_map(params![n as i64], |r| {
                Ok(LogRow {
                    op: r.get(0)?,
                    detail: r.get(1)?,
                    created_iso: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Upsert an import registry record, keyed by its canonical `imports/<name>`
    /// path. The `imports` table is almost-but-not-quite derived: it holds the
    /// user-supplied `tags`, which you can't get back from the file alone. So it
    /// SURVIVES `wipe_derived` and `reindex` reconciles it instead of rebuilding it.
    pub fn record_import(
        &self,
        source: &str,
        orig_name: &str,
        tags: &str,
        chunks: i64,
        content_hash: &str,
    ) -> Result<()> {
        let (epoch, iso) = now();
        self.conn.execute(
            "INSERT INTO imports(source, orig_name, tags, chunks, content_hash, created_at, created_iso)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source) DO UPDATE SET
               orig_name = excluded.orig_name, tags = excluded.tags,
               chunks = excluded.chunks, content_hash = excluded.content_hash",
            params![source, orig_name, tags, chunks, content_hash, epoch, iso],
        )?;
        Ok(())
    }

    pub fn import_record(&self, source: &str) -> Result<Option<ImportRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT source, orig_name, tags, chunks, content_hash, created_iso
                 FROM imports WHERE source = ?1",
                params![source],
                map_import,
            )
            .optional()?)
    }

    pub fn list_imports(&self) -> Result<Vec<ImportRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, orig_name, tags, chunks, content_hash, created_iso
             FROM imports ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], map_import)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Remove an import record along with the chunk rows it produced. `reindex`
    /// calls this once the original under `imports/` has disappeared.
    pub fn delete_import(&self, source: &str) -> Result<()> {
        self.delete_chunks_for_source(source)?;
        self.conn
            .execute("DELETE FROM imports WHERE source = ?1", params![source])?;
        Ok(())
    }

    /// Delete all chunk rows (and their FTS entries) derived from one import.
    pub fn delete_chunks_for_source(&self, source: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notes_fts WHERE rowid IN
               (SELECT rowid FROM notes WHERE kind = 'chunk' AND source = ?1)",
            params![source],
        )?;
        self.conn.execute(
            "DELETE FROM notes WHERE kind = 'chunk' AND source = ?1",
            params![source],
        )?;
        Ok(())
    }

    // ---- sync (file change-detection) ------------------------------------

    /// The `(content_hash, mtime)` we last indexed for a source-of-truth file.
    pub fn file_state_get(&self, path: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content_hash, mtime FROM sync WHERE path = ?1",
                params![path],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    pub fn file_state_set(
        &self,
        path: &str,
        kind: &str,
        reference: &str,
        content_hash: &str,
        mtime: i64,
    ) -> Result<()> {
        let (epoch, _) = now();
        self.conn.execute(
            "INSERT INTO sync(path, kind, ref, content_hash, mtime, indexed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
               kind = excluded.kind, ref = excluded.ref,
               content_hash = excluded.content_hash, mtime = excluded.mtime,
               indexed_at = excluded.indexed_at",
            params![path, kind, reference, content_hash, mtime, epoch],
        )?;
        Ok(())
    }

    pub fn file_state_delete(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sync WHERE path = ?1", params![path])?;
        Ok(())
    }

    // ---- graph edges (derived from md) -----------------------------------

    /// Insert a derived edge. The primary key dedups, so identical edges collapse.
    pub fn insert_edge(
        &self,
        src_id: &str,
        predicate: &str,
        dst_id: Option<&str>,
        dst_raw: &str,
        source: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO edges(src_id, predicate, dst_id, dst_raw, source)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![src_id, predicate, dst_id, dst_raw, source],
        )?;
        Ok(())
    }

    /// Edges that point AT `dst_id` — the inbound, backlink direction (D6/D7). The
    /// graph is otherwise strictly one-way (`edges_from` walks `src_id`), so this
    /// is the only way to ask "who links to this note?". Uses the `edges_dst`
    /// index. Ordered for deterministic output.
    pub fn edges_to(&self, dst_id: &str) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT src_id, predicate, dst_id, dst_raw, source FROM edges
             WHERE dst_id = ?1 ORDER BY src_id, predicate, source",
        )?;
        let rows = stmt
            .query_map(params![dst_id], map_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct source ids that currently own at least one dangling edge
    /// (`dst_id IS NULL`). Incremental `reindex` uses this to re-resolve forward
    /// references: when a fresh slug/id appears, the notes that were left dangling
    /// on it get their edges re-derived even though their own md never changed
    /// (closes B1/L3). Ordered for deterministic processing.
    pub fn dangling_edge_sources(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT src_id FROM edges WHERE dst_id IS NULL ORDER BY src_id")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A note's outgoing edges (both resolved and dangling), in a fixed order.
    pub fn edges_from(&self, src_id: &str) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT src_id, predicate, dst_id, dst_raw, source FROM edges
             WHERE src_id = ?1 ORDER BY predicate, dst_raw, source",
        )?;
        let rows = stmt
            .query_map(params![src_id], map_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop a note's outgoing edges before we re-derive them on (re)index.
    pub fn delete_edges_from(&self, src_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM edges WHERE src_id = ?1", params![src_id])?;
        Ok(())
    }

    /// When a note is removed (forgotten or pruned), turn every edge that pointed
    /// AT it back into a dangling one (`dst_id = NULL`), leaving `dst_raw` — the
    /// verbatim md target — intact. This keeps the graph honest: the link is still
    /// authored in the source note's md, so it must re-dangle (and spring back to
    /// life if a note with that slug/id reappears), not silently vanish or stay
    /// resolved to a dead row. Closes B2/B3 (orphaned resolved edges). Returns how
    /// many edges were re-dangled. Uses the `edges_dst` index.
    pub fn dangle_edges_to(&self, dst_id: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE edges SET dst_id = NULL WHERE dst_id = ?1",
            params![dst_id],
        )?;
        Ok(n)
    }

    // ---- code graph (feature `code`; separate from the notes graph) ------

    /// The `(content_hash, mtime)` we last indexed for a source file, or `None`.
    /// Drives incremental `cm map` exactly like `file_state_get` does for notes.
    pub fn code_file_state(&self, path: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content_hash, mtime FROM code_files WHERE path = ?1",
                params![path],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    /// Record (or refresh) a source file's indexed state.
    pub fn upsert_code_file(
        &self,
        path: &str,
        lang: &str,
        content_hash: &str,
        mtime: i64,
    ) -> Result<()> {
        let (epoch, _) = now();
        self.conn.execute(
            "INSERT INTO code_files(path, lang, content_hash, mtime, indexed_at)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               lang = excluded.lang, content_hash = excluded.content_hash,
               mtime = excluded.mtime, indexed_at = excluded.indexed_at",
            params![path, lang, content_hash, mtime, epoch],
        )?;
        Ok(())
    }

    /// Every indexed source path (sorted) — used to prune files that vanished.
    pub fn code_file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM code_files ORDER BY path")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop a source file and everything derived from it: its definitions, the
    /// `defines` edges from the file, and the `uses` edges out of its symbols.
    /// (Inbound `uses` edges that pointed at those symbols are re-dangled by
    /// `dangle_code_edges_to` in the caller, mirroring the notes-graph trick.)
    pub fn delete_code_file(&self, path: &str) -> Result<()> {
        // uses-edges originate from this file's symbols (src = symbol_id).
        self.conn.execute(
            "DELETE FROM code_edges WHERE predicate = 'uses' AND src IN
               (SELECT symbol_id FROM code_symbols WHERE path = ?1)",
            params![path],
        )?;
        // defines-edges originate from the file path itself.
        self.conn.execute(
            "DELETE FROM code_edges WHERE predicate = 'defines' AND src = ?1",
            params![path],
        )?;
        self.conn
            .execute("DELETE FROM code_symbols WHERE path = ?1", params![path])?;
        self.conn
            .execute("DELETE FROM code_files WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// Insert a symbol definition (PK dedups, so re-running is idempotent).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_code_symbol(
        &self,
        symbol_id: &str,
        path: &str,
        name: &str,
        kind: &str,
        line: i64,
        signature: Option<&str>,
        is_test: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO code_symbols(symbol_id, path, name, kind, line, signature, is_test)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![symbol_id, path, name, kind, line, signature, is_test as i64],
        )?;
        Ok(())
    }

    /// Insert a code edge (`defines`/`uses`). PK dedups identical edges.
    pub fn insert_code_edge(
        &self,
        src: &str,
        predicate: &str,
        dst: Option<&str>,
        dst_raw: &str,
        line: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO code_edges(src, predicate, dst, dst_raw, line)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![src, predicate, dst, dst_raw, line],
        )?;
        Ok(())
    }

    /// Re-dangle every `uses` edge pointing at a now-removed symbol_id, keeping
    /// `dst_raw` so it re-resolves when a symbol with that name reappears.
    pub fn dangle_code_edges_to(&self, dst: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE code_edges SET dst = NULL WHERE dst = ?1",
            params![dst],
        )?;
        Ok(n)
    }

    /// `(name -> symbol_id)` for every definition, used to resolve `uses` targets
    /// by name. A name defined more than once keeps the lowest symbol_id (stable,
    /// deterministic — mirrors the slug-collision rule in the notes graph).
    pub fn code_symbol_name_map(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, symbol_id FROM code_symbols ORDER BY symbol_id")?;
        let mut map = std::collections::BTreeMap::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (name, id) = row?;
            map.entry(name).or_insert(id);
        }
        Ok(map)
    }

    /// Distinct sources owning at least one dangling `uses` edge — the incremental
    /// `cm map` re-resolution pass walks these (same idea as `dangling_edge_sources`).
    pub fn dangling_code_sources(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT src FROM code_edges WHERE dst IS NULL AND predicate = 'uses'
             ORDER BY src",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Re-resolve a single dangling `uses` edge's `dst` in place once its target
    /// name appears (incremental revival). Matches on the verbatim `dst_raw`.
    pub fn resolve_code_edge(&self, src: &str, dst_raw: &str, dst: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE code_edges SET dst = ?3
             WHERE src = ?1 AND dst_raw = ?2 AND dst IS NULL AND predicate = 'uses'",
            params![src, dst_raw, dst],
        )?;
        Ok(())
    }

    /// SQL fragment that drops test-defined symbols unless `include_tests`. Appended
    /// to a WHERE that already has a condition, so it leads with ` AND`.
    fn test_clause(include_tests: bool) -> &'static str {
        if include_tests {
            ""
        } else {
            " AND is_test = 0"
        }
    }

    /// Look up symbol definitions by exact name (for `cm map --query <name>`).
    /// Test-defined symbols are excluded unless `include_tests`.
    pub fn code_symbols_by_name(
        &self,
        name: &str,
        include_tests: bool,
    ) -> Result<Vec<CodeSymbolRow>> {
        let sql = format!(
            "SELECT symbol_id, path, name, kind, line, signature, is_test FROM code_symbols
             WHERE name = ?1{} ORDER BY path, line",
            Self::test_clause(include_tests)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![name], map_code_symbol)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Symbols defined in a file by EXACT path key, in source order. Used by the
    /// internal re-map / prune paths, which always hold the canonical stored key —
    /// they need ALL symbols (tests included), so there's no test filter here.
    pub fn code_symbols_in(&self, path: &str) -> Result<Vec<CodeSymbolRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol_id, path, name, kind, line, signature, is_test FROM code_symbols
             WHERE path = ?1 ORDER BY line",
        )?;
        let rows = stmt
            .query_map(params![path], map_code_symbol)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Symbols defined in a file for `cm map --defines <path>`, matched leniently
    /// so the user needn't know the exact stored key: a path matches when it equals
    /// the stored key, the stored key ends with `/<path>`, OR `<path>` ends with
    /// `/storedkey`. That makes `src/code.rs` and `code.rs` interchangeable whether
    /// the tree was mapped from the repo root or from `./src`. Grouped by path then
    /// line for stable output. Test symbols excluded unless `include_tests`. The
    /// `LIKE` escapes are unnecessary here: stored keys are source paths with no
    /// `%`/`_` metacharacters in practice.
    pub fn code_symbols_in_like(
        &self,
        path: &str,
        include_tests: bool,
    ) -> Result<Vec<CodeSymbolRow>> {
        let key_ends_with_path = format!("%/{path}"); // stored 'a/b/code.rs' vs query 'code.rs'
        let sql = format!(
            "SELECT symbol_id, path, name, kind, line, signature, is_test FROM code_symbols
             WHERE (path = ?1
                OR path LIKE ?2
                OR ?1 LIKE '%/' || path){}
             ORDER BY path, line",
            Self::test_clause(include_tests)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![path, key_ends_with_path], map_code_symbol)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Symbol definitions whose name CONTAINS `needle` (case-insensitive), for
    /// `cm map --like <substr>` — the structural answer to "show me every `code_*`
    /// function" that exact-name `--query` can't give. `_`/`%` in the needle are
    /// escaped so they're treated literally (ESCAPE '\'). Test symbols excluded
    /// unless `include_tests`. Ordered by name for a stable, scannable list.
    pub fn code_symbols_like(
        &self,
        needle: &str,
        include_tests: bool,
    ) -> Result<Vec<CodeSymbolRow>> {
        let escaped = needle
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let sql = format!(
            "SELECT symbol_id, path, name, kind, line, signature, is_test FROM code_symbols
             WHERE name LIKE ?1 ESCAPE '\\'{} ORDER BY name, path, line",
            Self::test_clause(include_tests)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![pattern], map_code_symbol)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Outgoing calls FROM a symbol — what `name`'s definition(s) depend on, for
    /// `cm map --calls <name>`. Each row is `(target_name, edge_line, resolved)`:
    /// `resolved` is true when the call links to an in-project definition (dst set),
    /// false for external/stdlib names (`map`, `unwrap`, …) that never resolved.
    /// `resolved_only` drops the unresolved noise — the default, because ~3/4 of
    /// `uses` edges are stdlib combinators that bury the few real dependencies.
    /// When several symbols share `name`, edges from all of them are merged.
    pub fn code_callees_of(
        &self,
        name: &str,
        resolved_only: bool,
    ) -> Result<Vec<(String, i64, bool)>> {
        let sql = if resolved_only {
            "SELECT e.dst_raw, e.line, e.dst IS NOT NULL
             FROM code_edges e
             JOIN code_symbols s ON s.symbol_id = e.src
             WHERE e.predicate = 'uses' AND s.name = ?1 AND e.dst IS NOT NULL
             ORDER BY e.line, e.dst_raw"
        } else {
            "SELECT e.dst_raw, e.line, e.dst IS NOT NULL
             FROM code_edges e
             JOIN code_symbols s ON s.symbol_id = e.src
             WHERE e.predicate = 'uses' AND s.name = ?1
             ORDER BY e.line, e.dst_raw"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![name], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All symbol definitions, for `cm map --list` (a table of contents). Optional
    /// `kind` filter (exact tag kind: function/method/struct→class/…). Test symbols
    /// excluded unless `include_tests`. Ordered by path then line so a file's
    /// symbols read top-to-bottom.
    pub fn code_list(&self, kind: Option<&str>, include_tests: bool) -> Result<Vec<CodeSymbolRow>> {
        // Build one SQL string; `kind` becomes a WHERE, test-exclusion appends with
        // the right connective (AND after a kind clause, WHERE on its own).
        let mut sql = String::from(
            "SELECT symbol_id, path, name, kind, line, signature, is_test FROM code_symbols",
        );
        if kind.is_some() {
            sql.push_str(" WHERE kind = ?1");
            if !include_tests {
                sql.push_str(" AND is_test = 0");
            }
        } else if !include_tests {
            sql.push_str(" WHERE is_test = 0");
        }
        sql.push_str(" ORDER BY path, line");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match kind {
            Some(k) => stmt
                .query_map(params![k], map_code_symbol)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map([], map_code_symbol)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// God-node ranking for `cm map --list` (token-efficiency, borrowed from
    /// graphify's degree-centrality "god nodes"): symbol definitions ordered by their
    /// connectivity in the `uses` graph — in-degree (how many symbols reference it)
    /// plus resolved out-degree (how many in-project symbols it references). The
    /// high-degree symbols are the hubs to read first to grasp an architecture, so a
    /// capped top-N is a tiny slice that still covers the skeleton. `limit = None`
    /// returns the whole ranking (the `--all` escape hatch). Same `kind`/`is_test`
    /// filters as `code_list`; ties break on a stable path/line for determinism.
    pub fn code_hubs(
        &self,
        kind: Option<&str>,
        include_tests: bool,
        limit: Option<usize>,
    ) -> Result<Vec<(CodeSymbolRow, i64)>> {
        let mut sql = String::from(
            "SELECT s.symbol_id, s.path, s.name, s.kind, s.line, s.signature, s.is_test,
                    (SELECT count(*) FROM code_edges e
                       WHERE e.predicate = 'uses' AND e.dst = s.symbol_id)
                  + (SELECT count(*) FROM code_edges e
                       WHERE e.predicate = 'uses' AND e.src = s.symbol_id AND e.dst IS NOT NULL)
                    AS degree
             FROM code_symbols s",
        );
        if kind.is_some() {
            sql.push_str(" WHERE s.kind = ?1");
            if !include_tests {
                sql.push_str(" AND s.is_test = 0");
            }
        } else if !include_tests {
            sql.push_str(" WHERE s.is_test = 0");
        }
        sql.push_str(" ORDER BY degree DESC, s.path, s.line");
        if let Some(n) = limit {
            // `n` is a usize from our own parse, never user SQL text — no injection.
            sql.push_str(&format!(" LIMIT {n}"));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row<'_>| Ok((map_code_symbol(r)?, r.get::<_, i64>(7)?));
        let rows = match kind {
            Some(k) => stmt
                .query_map(params![k], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map([], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Callers of a symbol: the defining symbol rows on the `src` end of every
    /// resolved `uses` edge whose target is one of `name`'s definitions
    /// (for `cm map --uses <name>`). Returns each calling symbol with the line.
    /// Callers defined in test code are excluded unless `include_tests`.
    pub fn code_callers_of(
        &self,
        name: &str,
        include_tests: bool,
    ) -> Result<Vec<(CodeSymbolRow, i64)>> {
        let sql = format!(
            "SELECT s.symbol_id, s.path, s.name, s.kind, s.line, s.signature, s.is_test, e.line
             FROM code_edges e
             JOIN code_symbols t ON t.symbol_id = e.dst
             JOIN code_symbols s ON s.symbol_id = e.src
             WHERE e.predicate = 'uses' AND t.name = ?1{}
             ORDER BY s.path, e.line",
            if include_tests {
                ""
            } else {
                " AND s.is_test = 0"
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![name], |r| {
                Ok((
                    CodeSymbolRow {
                        symbol_id: r.get(0)?,
                        path: r.get(1)?,
                        name: r.get(2)?,
                        kind: r.get(3)?,
                        line: r.get(4)?,
                        signature: r.get(5)?,
                        is_test: r.get::<_, i64>(6)? != 0,
                    },
                    r.get::<_, i64>(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts for `cm map` summary output.
    pub fn code_counts(&self) -> Result<(i64, i64, i64)> {
        let files = self
            .conn
            .query_row("SELECT count(*) FROM code_files", [], |r| r.get(0))?;
        let symbols = self
            .conn
            .query_row("SELECT count(*) FROM code_symbols", [], |r| r.get(0))?;
        let edges = self
            .conn
            .query_row("SELECT count(*) FROM code_edges", [], |r| r.get(0))?;
        Ok((files, symbols, edges))
    }

    // ---- derived-wipe (reindex --all) ------------------------------------

    /// Wipe the purely-derived tables (notes, notes_fts, sync, imports, edges, and
    /// the code_* graph) for a full rebuild. Everything in here can be reconstructed
    /// from the files: notes/*.md (plus their frontmatter relations and body
    /// `[[links]]`), the imports/ originals with their `.meta.json` sidecars, and —
    /// for the code graph — the source tree on disk. We deliberately leave `oplog`
    /// (operation history) and `meta` (the embedder signature) alone, since those
    /// can't be.
    pub fn wipe_derived(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM notes_fts; DELETE FROM notes; DELETE FROM sync;
             DELETE FROM imports; DELETE FROM edges; DELETE FROM note_code_refs;
             DELETE FROM code_files; DELETE FROM code_symbols; DELETE FROM code_edges;",
        )?;
        Ok(())
    }

    /// How many rows of a given kind there are (the reindex legacy-data guard uses this).
    pub fn count_notes_kind(&self, kind: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM notes WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )?)
    }

    // ---- integrity probes (read-only; for `cm doctor`) -------------------
    // These are pure SELECTs that surface the hand-maintained invariants the
    // store relies on but doesn't enforce with foreign keys. `cm doctor` runs
    // them to report drift; the fix is always a `reindex` (the store is derived).

    /// `(notes without an fts row, fts rows without a note)` — both should be 0.
    /// `notes` ↔ `notes_fts` are synced BY HAND (no `content=` table, no FK), so a
    /// crash mid-write or a manual edit could desync them; doctor checks BOTH
    /// directions because each catches a different half-write.
    pub fn fts_desync_counts(&self) -> Result<(i64, i64)> {
        let notes_only: i64 = self.conn.query_row(
            "SELECT count(*) FROM notes n
               LEFT JOIN notes_fts f ON f.rowid = n.rowid
              WHERE f.rowid IS NULL",
            [],
            |r| r.get(0),
        )?;
        let fts_only: i64 = self.conn.query_row(
            "SELECT count(*) FROM notes_fts f
               LEFT JOIN notes n ON n.rowid = f.rowid
              WHERE n.rowid IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok((notes_only, fts_only))
    }

    /// Count of RESOLVED note-graph edges whose target note no longer exists.
    /// Dangling edges (`dst_id IS NULL`) are first-class (forward refs) and are
    /// excluded — only a non-null `dst_id` pointing nowhere is the inconsistency
    /// (a delete that failed to re-dangle its inbound edges).
    pub fn resolved_edge_orphans(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM edges
              WHERE dst_id IS NOT NULL AND dst_id NOT IN (SELECT id FROM notes)",
            [],
            |r| r.get(0),
        )?)
    }

    /// All note ids of a given `kind` (e.g. `note`, `chunk`), for store↔disk drift
    /// checks (a DB row whose `notes/<id>.md` is gone, or vice-versa).
    pub fn note_ids_of_kind(&self, kind: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM notes WHERE kind = ?1")?;
        let rows = stmt
            .query_map(params![kind], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(count of notes carrying an embedding, the DISTINCT byte-lengths of those
    /// blobs)`. Each f32 is 4 bytes, so a length is `dimension * 4`; doctor flags a
    /// length not divisible by 4 (corrupt blob) or a dimension that disagrees with
    /// `config.embedding.dimension` (embedder drift — cosine silently scores 0).
    pub fn embedding_stats(&self) -> Result<(i64, Vec<i64>)> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM notes WHERE embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT length(embedding) FROM notes
              WHERE embedding IS NOT NULL ORDER BY 1",
        )?;
        let lengths = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((count, lengths))
    }

    /// Code-graph orphans: `(symbols whose file is gone from code_files, resolved
    /// `uses` edges whose target symbol_id is gone)`. Both should be 0 after a clean
    /// `map`; non-zero means a prune that didn't cascade. (Tables always exist even
    /// without the `code` feature, so this is always safe to call — it returns 0,0.)
    pub fn code_orphans(&self) -> Result<(i64, i64)> {
        let sym: i64 = self.conn.query_row(
            "SELECT count(*) FROM code_symbols
              WHERE path NOT IN (SELECT path FROM code_files)",
            [],
            |r| r.get(0),
        )?;
        let edge: i64 = self.conn.query_row(
            "SELECT count(*) FROM code_edges
              WHERE dst IS NOT NULL AND dst NOT IN (SELECT symbol_id FROM code_symbols)",
            [],
            |r| r.get(0),
        )?;
        Ok((sym, edge))
    }
}

fn map_import(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRow> {
    Ok(ImportRow {
        source: r.get(0)?,
        orig_name: r.get(1)?,
        tags: r.get(2)?,
        chunks: r.get(3)?,
        content_hash: r.get(4)?,
        created_iso: r.get(5)?,
    })
}

fn map_code_symbol(r: &rusqlite::Row<'_>) -> rusqlite::Result<CodeSymbolRow> {
    Ok(CodeSymbolRow {
        symbol_id: r.get(0)?,
        path: r.get(1)?,
        name: r.get(2)?,
        kind: r.get(3)?,
        line: r.get(4)?,
        signature: r.get(5)?,
        is_test: r.get::<_, i64>(6)? != 0,
    })
}

fn map_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<EdgeRow> {
    Ok(EdgeRow {
        src_id: r.get(0)?,
        predicate: r.get(1)?,
        dst_id: r.get(2)?,
        dst_raw: r.get(3)?,
        source: r.get(4)?,
    })
}

/// A per-process counter, so two ids minted in the same nanosecond still differ.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An 8-hex-char id, FNV-1a-folded from the wall-clock nanos, the pid, and that
/// process counter. It's not cryptographic, just a short handle, and `fresh_id`
/// retries on the rare chance one's already taken.
fn mint_hex() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let ctr = ID_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for word in [nanos, pid, ctr] {
        for b in word.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{:08x}", h & 0xffff_ffff)
}

fn map_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        id: r.get(0)?,
        body: r.get(1)?,
        tags: r.get(2)?,
        source: r.get(3)?,
        origin: r.get(4)?,
        kind: r.get(5)?,
        created_at: r.get(6)?,
        created_iso: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::iso_utc;
    use tempfile::TempDir;

    fn mem() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    /// `insert_note` with defaults for the boring fields.
    fn ins(s: &Store, body: &str) -> String {
        s.insert_note(body, "", None, None, "note", &[0.1, 0.2])
            .unwrap()
    }

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn open_memory_creates_full_schema() {
        let s = mem();
        let n = count(
            &s,
            "SELECT count(*) FROM sqlite_master
             WHERE name IN ('notes','notes_fts','oplog','imports','meta','sync','edges','note_code_refs')",
        );
        assert_eq!(n, 8);
    }

    #[test]
    fn note_code_refs_set_get_delete_and_dedup() {
        let s = mem();
        let id = s.insert_note("body", "", None, None, "note", &[]).unwrap();
        s.set_note_code_refs(
            &id,
            &["Beta".into(), "Alpha".into(), "Alpha".into(), "  ".into()],
        )
        .unwrap();
        // Name-sorted, deduped (PK collapses the repeat), blanks dropped.
        assert_eq!(s.note_code_refs(&id).unwrap(), vec!["Alpha", "Beta"]);
        // Reverse lookup: which notes document a symbol (backlinks --symbol).
        assert_eq!(s.notes_documenting("Alpha").unwrap(), vec![id.clone()]);
        assert!(s.notes_documenting("Nonexistent").unwrap().is_empty());
        // Re-set replaces wholesale, so the table always mirrors the md.
        s.set_note_code_refs(&id, &["Gamma".into()]).unwrap();
        assert_eq!(s.note_code_refs(&id).unwrap(), vec!["Gamma"]);
        assert!(s.notes_documenting("Alpha").unwrap().is_empty()); // re-set dropped it
                                                                   // Delete clears them.
        s.delete_note_code_refs(&id).unwrap();
        assert!(s.note_code_refs(&id).unwrap().is_empty());
    }

    #[test]
    fn insert_note_returns_id_and_roundtrips_via_get() {
        let s = mem();
        let id = s
            .insert_note(
                "body text",
                "a,b",
                Some("src"),
                Some("orig"),
                "note",
                &[1.0],
            )
            .unwrap();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let row = s.get(&id).unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.body, "body text");
        assert_eq!(row.tags, "a,b");
        assert_eq!(row.source.as_deref(), Some("src"));
        assert_eq!(row.origin.as_deref(), Some("orig"));
        assert_eq!(row.kind, "note");
    }

    #[test]
    fn embedding_blob_roundtrip_via_all_embeddings() {
        let s = mem();
        let id = s
            .insert_note("x", "", None, None, "note", &[1.5, -2.0, 0.25])
            .unwrap();
        let all = s.all_embeddings().unwrap();
        let (gid, vec) = all.iter().find(|(i, _)| i == &id).unwrap();
        assert_eq!(*gid, id);
        assert_eq!(vec, &vec![1.5, -2.0, 0.25]);
    }

    #[test]
    fn fts_search_ranks_better_match_lower() {
        let s = mem();
        let strong = s
            .insert_note("auth auth auth token", "", None, None, "note", &[0.0])
            .unwrap();
        let weak = s
            .insert_note("auth", "", None, None, "note", &[0.0])
            .unwrap();
        let hits = s.fts_search("\"auth\" OR \"token\"", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, strong); // best match first (lowest bm25)
        assert!(hits[0].1 <= hits[1].1);
        assert!(hits.iter().any(|(id, _)| id == &weak));
    }

    #[test]
    fn forget_deletes_from_notes_and_fts_keeps_counts_synced() {
        let s = mem();
        let id = ins(&s, "deletable note");
        assert!(s.forget(&id).unwrap());
        assert!(s.get(&id).unwrap().is_none());
        assert!(s.fts_search("\"deletable\"", 10).unwrap().is_empty());
        assert_eq!(
            count(&s, "SELECT count(*) FROM notes"),
            count(&s, "SELECT count(*) FROM notes_fts"),
        );
    }

    #[test]
    fn forget_missing_id_returns_false_no_error() {
        assert!(!mem().forget("ffffffff").unwrap());
    }

    // ---- integrity probes (cm doctor) -----------------------------------

    #[test]
    fn fts_desync_counts_zero_when_synced_and_detects_break() {
        let s = mem();
        ins(&s, "alpha");
        ins(&s, "beta");
        assert_eq!(s.fts_desync_counts().unwrap(), (0, 0));
        // Drop the fts side by hand to simulate a half-write: every note now lacks
        // its fts row (notes_only = 2), nothing orphaned the other way.
        s.conn.execute("DELETE FROM notes_fts", []).unwrap();
        assert_eq!(s.fts_desync_counts().unwrap(), (2, 0));
    }

    #[test]
    fn note_ids_of_kind_filters_by_kind() {
        let s = mem();
        let n = ins(&s, "a note");
        s.insert_note("a chunk", "", None, None, "chunk", &[0.0])
            .unwrap();
        assert_eq!(s.note_ids_of_kind("note").unwrap(), vec![n]);
        assert_eq!(s.note_ids_of_kind("chunk").unwrap().len(), 1);
        assert!(s.note_ids_of_kind("nope").unwrap().is_empty());
    }

    #[test]
    fn embedding_stats_counts_and_distinct_lengths() {
        let s = mem();
        // `ins` stores a 2-float embedding -> 8 bytes.
        ins(&s, "x");
        ins(&s, "y");
        assert_eq!(s.embedding_stats().unwrap(), (2, vec![8]));
        // A different dimension shows up as a second distinct length (drift).
        s.insert_note("z", "", None, None, "note", &[1.0, 2.0, 3.0])
            .unwrap();
        assert_eq!(s.embedding_stats().unwrap(), (3, vec![8, 12]));
    }

    #[test]
    fn resolved_edge_orphans_counts_only_broken_resolved_edges() {
        let s = mem();
        let a = ins(&s, "target");
        let b = ins(&s, "source");
        // A healthy resolved edge b -> a, plus a dangling one (excluded).
        s.insert_edge(&b, "links_to", Some(&a), &a, "wikilink")
            .unwrap();
        s.insert_edge(&b, "links_to", None, "ghost", "wikilink")
            .unwrap();
        assert_eq!(s.resolved_edge_orphans().unwrap(), 0);
        // A resolved edge whose target id doesn't exist is the inconsistency.
        s.insert_edge(&b, "rel", Some("deadbeef"), "deadbeef", "relation")
            .unwrap();
        assert_eq!(s.resolved_edge_orphans().unwrap(), 1);
    }

    #[test]
    fn code_orphans_zero_on_empty_graph() {
        // No code feature / empty graph: the queries are still valid and yield 0,0.
        assert_eq!(mem().code_orphans().unwrap(), (0, 0));
    }

    #[test]
    fn list_orders_desc_and_respects_limit() {
        let s = mem();
        let a = ins(&s, "one");
        let b = ins(&s, "two");
        let c = ins(&s, "three");
        assert!(s.list(0).unwrap().is_empty());
        let all: Vec<String> = s.list(100).unwrap().iter().map(|r| r.id.clone()).collect();
        assert_eq!(all, vec![c.clone(), b.clone(), a]); // DESC (insertion order)
        let two: Vec<String> = s.list(2).unwrap().iter().map(|r| r.id.clone()).collect();
        assert_eq!(two, vec![c, b]);
    }

    #[test]
    fn all_orders_asc_and_empty_on_fresh() {
        let s = mem();
        assert!(s.all().unwrap().is_empty());
        let a = ins(&s, "one");
        let b = ins(&s, "two");
        let ids: Vec<String> = s.all().unwrap().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec![a, b]); // ASC (insertion order)
    }

    #[test]
    fn meta_set_get_upsert_overwrites() {
        let s = mem();
        s.meta_set("k", "1").unwrap();
        assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("1"));
        s.meta_set("k", "2").unwrap();
        assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("2"));
        assert_eq!(count(&s, "SELECT count(*) FROM meta"), 1);
        assert_eq!(s.meta_get("absent").unwrap(), None);
    }

    #[test]
    fn log_op_and_recent_logs_desc() {
        let s = mem();
        s.log_op("first", Some("d1")).unwrap();
        s.log_op("second", None).unwrap();
        let logs = s.recent_logs(10).unwrap();
        assert_eq!(logs[0].op, "second");
        assert_eq!(logs[0].detail, None);
        assert_eq!(logs[1].op, "first");
        assert_eq!(logs[1].detail.as_deref(), Some("d1"));
    }

    #[test]
    fn insert_note_none_origin_stored_as_null_in_notes() {
        let s = mem();
        let id = s
            .insert_note("findable body", "", None, None, "note", &[0.0])
            .unwrap();
        assert_eq!(s.get(&id).unwrap().unwrap().origin, None);
        assert!(!s.fts_search("\"findable\"", 10).unwrap().is_empty());
    }

    #[test]
    fn empty_embedding_still_listed_and_decodes_empty() {
        let s = mem();
        let id = s.insert_note("x", "", None, None, "note", &[]).unwrap();
        let all = s.all_embeddings().unwrap();
        let (_, vec) = all.iter().find(|(i, _)| i == &id).unwrap();
        assert!(vec.is_empty());
    }

    #[test]
    fn fts_search_unicode_diacritics_fold() {
        let s = mem();
        s.insert_note("café résumé", "", None, None, "note", &[0.0])
            .unwrap();
        // remove_diacritics 2 folds é -> e.
        assert!(!s.fts_search("\"cafe\"", 10).unwrap().is_empty());
        // Cyrillic is indexed too.
        s.insert_note("авторизация", "", None, None, "note", &[0.0])
            .unwrap();
        assert!(!s.fts_search("\"авторизация\"", 10).unwrap().is_empty());
    }

    #[test]
    fn fts_search_invalid_expr_returns_err() {
        let s = mem();
        ins(&s, "anything");
        let err = s.fts_search("(", 10).unwrap_err();
        assert!(err.msg.contains("storage error"));
    }

    #[test]
    fn fts_indexes_tags_and_origin_not_source() {
        let s = mem();
        s.insert_note(
            "plainbody",
            "tagword",
            Some("sourceword"),
            Some("originword"),
            "note",
            &[0.0],
        )
        .unwrap();
        assert!(s.fts_search("\"sourceword\"", 10).unwrap().is_empty()); // source NOT indexed
        assert!(!s.fts_search("\"tagword\"", 10).unwrap().is_empty());
        assert!(!s.fts_search("\"originword\"", 10).unwrap().is_empty());
    }

    #[test]
    fn ids_matching_tag_and_origin_prefix() {
        let s = mem();
        let a = s
            .insert_note(
                "a",
                "auth,spec",
                None,
                Some("arch.md › Auth"),
                "chunk",
                &[0.0],
            )
            .unwrap();
        let b = s
            .insert_note("b", "Spec", None, Some("arch.md › DB"), "chunk", &[0.0])
            .unwrap();
        let c = s
            .insert_note("c", "other", None, Some("readme.md"), "chunk", &[0.0])
            .unwrap();

        // No filter args -> every id (search.rs only calls this with a filter set).
        assert_eq!(s.ids_matching(None, None).unwrap().len(), 3);

        // Tag is matched per-token, case-insensitively ("Spec" == "spec").
        let by_tag = s.ids_matching(Some("spec"), None).unwrap();
        assert!(by_tag.contains(&a) && by_tag.contains(&b) && !by_tag.contains(&c));

        // Origin prefix narrows to one source file.
        let by_origin = s.ids_matching(None, Some("arch.md")).unwrap();
        assert!(by_origin.contains(&a) && by_origin.contains(&b) && !by_origin.contains(&c));

        // Both filters compose (AND).
        let both = s.ids_matching(Some("auth"), Some("arch.md")).unwrap();
        assert_eq!(both.into_iter().collect::<Vec<_>>(), vec![a]);
    }

    #[test]
    fn record_import_and_list_imports_desc() {
        let s = mem();
        s.record_import("imports/first.md", "first.md", "a", 3, "h1")
            .unwrap();
        s.record_import("imports/second.md", "second.md", "b,c", 5, "h2")
            .unwrap();
        let imps = s.list_imports().unwrap();
        assert_eq!(imps[0].source, "imports/second.md"); // DESC
        assert_eq!(imps[0].orig_name, "second.md");
        assert_eq!(imps[0].tags, "b,c");
        assert_eq!(imps[0].chunks, 5);
        assert_eq!(imps[0].content_hash, "h2");
        assert_eq!(imps[1].source, "imports/first.md");
    }

    #[test]
    fn record_import_upserts_by_source() {
        let s = mem();
        s.record_import("imports/a.md", "a.md", "t1", 3, "h1")
            .unwrap();
        s.record_import("imports/a.md", "a.md", "t2", 7, "h2")
            .unwrap();
        let imps = s.list_imports().unwrap();
        assert_eq!(imps.len(), 1); // same source -> one row, replaced in place
        assert_eq!(imps[0].tags, "t2");
        assert_eq!(imps[0].chunks, 7);
        assert_eq!(
            s.import_record("imports/a.md")
                .unwrap()
                .unwrap()
                .content_hash,
            "h2"
        );
        assert!(s.import_record("imports/missing.md").unwrap().is_none());
    }

    #[test]
    fn file_state_roundtrip_and_delete() {
        let s = mem();
        assert!(s.file_state_get("notes/a.md").unwrap().is_none());
        s.file_state_set("notes/a.md", "note", "a", "deadbeef", 1234)
            .unwrap();
        assert_eq!(
            s.file_state_get("notes/a.md").unwrap(),
            Some(("deadbeef".to_string(), 1234))
        );
        // Upsert in place.
        s.file_state_set("notes/a.md", "note", "a", "feedface", 5678)
            .unwrap();
        assert_eq!(
            s.file_state_get("notes/a.md").unwrap(),
            Some(("feedface".to_string(), 5678))
        );
        s.file_state_delete("notes/a.md").unwrap();
        assert!(s.file_state_get("notes/a.md").unwrap().is_none());
    }

    #[test]
    fn wipe_derived_clears_index_but_preserves_oplog_and_meta() {
        let s = mem();
        ins(&s, "a note");
        s.file_state_set("notes/x.md", "note", "x", "h", 0).unwrap();
        s.log_op("remember", Some("d")).unwrap();
        s.meta_set("embedder_signature", "local:m:1").unwrap();
        s.record_import("imports/d.md", "d.md", "t", 2, "ih")
            .unwrap();
        s.insert_edge("a", "links_to", Some("b"), "b", "wikilink")
            .unwrap();

        s.wipe_derived().unwrap();

        // All derived tables emptied (notes/imports rebuild from files+sidecars)...
        assert_eq!(count(&s, "SELECT count(*) FROM notes"), 0);
        assert_eq!(count(&s, "SELECT count(*) FROM notes_fts"), 0);
        assert_eq!(count(&s, "SELECT count(*) FROM sync"), 0);
        assert_eq!(count(&s, "SELECT count(*) FROM imports"), 0);
        assert_eq!(count(&s, "SELECT count(*) FROM edges"), 0);
        // ...but operation history and the embedder signature survive (not derivable).
        assert_eq!(count(&s, "SELECT count(*) FROM oplog"), 1);
        assert_eq!(
            s.meta_get("embedder_signature").unwrap().as_deref(),
            Some("local:m:1")
        );
    }

    #[test]
    fn edges_insert_dedup_order_and_delete() {
        let s = mem();
        s.insert_edge("a", "depends_on", Some("b"), "db-schema", "relation")
            .unwrap();
        s.insert_edge("a", "links_to", None, "api-rate-limit", "wikilink")
            .unwrap();
        // Identical edge collapses via the PK (INSERT OR IGNORE).
        s.insert_edge("a", "depends_on", Some("b"), "db-schema", "relation")
            .unwrap();
        let from = s.edges_from("a").unwrap();
        assert_eq!(from.len(), 2);
        // Ordered by (predicate, dst_raw, source): depends_on < links_to.
        assert_eq!(from[0].predicate, "depends_on");
        assert_eq!(from[0].dst_id.as_deref(), Some("b"));
        assert_eq!(from[1].predicate, "links_to");
        assert_eq!(from[1].dst_id, None); // dangling
        assert_eq!(from[1].dst_raw, "api-rate-limit");
        // Re-deriving a note's edges: drop then reinsert.
        s.delete_edges_from("a").unwrap();
        assert!(s.edges_from("a").unwrap().is_empty());
    }

    #[test]
    fn edges_to_returns_only_inbound_resolved_edges() {
        let s = mem();
        s.insert_edge("a", "links_to", Some("b"), "beta", "wikilink")
            .unwrap();
        s.insert_edge("c", "depends_on", Some("b"), "b-slug", "relation")
            .unwrap();
        s.insert_edge("a", "links_to", None, "ghost", "wikilink")
            .unwrap(); // dangling, not inbound to b
        let into_b = s.edges_to("b").unwrap();
        assert_eq!(into_b.len(), 2);
        // Ordered by (src_id, predicate, source): a < c.
        assert_eq!(into_b[0].src_id, "a");
        assert_eq!(into_b[1].src_id, "c");
        assert!(s.edges_to("zzz").unwrap().is_empty());
    }

    #[test]
    fn dangling_edge_sources_lists_distinct_sources_with_null_dst() {
        let s = mem();
        s.insert_edge("a", "links_to", None, "ghost", "wikilink")
            .unwrap();
        s.insert_edge("a", "depends_on", None, "spectre", "relation")
            .unwrap(); // same source, two dangling
        s.insert_edge("c", "links_to", None, "phantom", "wikilink")
            .unwrap();
        s.insert_edge("d", "links_to", Some("a"), "a-slug", "wikilink")
            .unwrap(); // resolved -> not a source
        assert_eq!(s.dangling_edge_sources().unwrap(), vec!["a", "c"]);
    }

    #[test]
    fn dangle_edges_to_renulls_inbound_keeps_raw_and_outbound() {
        let s = mem();
        // a -> b (resolved), c -> b (resolved), b -> d (outbound, unrelated).
        s.insert_edge("a", "links_to", Some("b"), "beta", "wikilink")
            .unwrap();
        s.insert_edge("c", "depends_on", Some("b"), "b-slug", "relation")
            .unwrap();
        s.insert_edge("b", "links_to", Some("d"), "delta", "wikilink")
            .unwrap();

        // Forgetting b re-dangles the two edges pointing AT it (returns the count).
        assert_eq!(s.dangle_edges_to("b").unwrap(), 2);

        let from_a = s.edges_from("a").unwrap();
        assert_eq!(from_a[0].dst_id, None); // re-dangled...
        assert_eq!(from_a[0].dst_raw, "beta"); // ...but the verbatim target survives
        let from_c = s.edges_from("c").unwrap();
        assert_eq!(from_c[0].dst_id, None);
        assert_eq!(from_c[0].dst_raw, "b-slug");
        // b's own OUTBOUND edge is untouched (only inbound re-dangle).
        assert_eq!(s.edges_from("b").unwrap()[0].dst_id.as_deref(), Some("d"));
    }

    #[test]
    fn delete_chunks_for_source_removes_only_that_sources_chunks() {
        let s = mem();
        s.upsert_note(
            "c1",
            "chunk a1",
            "",
            Some("imports/a.md"),
            Some("a.md"),
            "chunk",
            0,
            "t",
            &[0.0],
        )
        .unwrap();
        s.upsert_note(
            "c2",
            "chunk a2",
            "",
            Some("imports/a.md"),
            Some("a.md #2"),
            "chunk",
            0,
            "t",
            &[0.0],
        )
        .unwrap();
        s.upsert_note(
            "c3",
            "chunk b1",
            "",
            Some("imports/b.md"),
            Some("b.md"),
            "chunk",
            0,
            "t",
            &[0.0],
        )
        .unwrap();
        s.delete_chunks_for_source("imports/a.md").unwrap();
        assert!(s.get("c1").unwrap().is_none() && s.get("c2").unwrap().is_none());
        assert!(s.get("c3").unwrap().is_some()); // other source untouched
        assert_eq!(
            count(&s, "SELECT count(*) FROM notes"),
            count(&s, "SELECT count(*) FROM notes_fts"),
        );
    }

    #[test]
    fn created_iso_matches_created_at_format() {
        let s = mem();
        let id = ins(&s, "x");
        let row = s.get(&id).unwrap().unwrap();
        assert_eq!(row.created_iso, iso_utc(row.created_at));
        // Shape: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(row.created_iso.len(), 20);
        assert!(row.created_iso.ends_with('Z'));
        assert_eq!(row.created_iso.as_bytes()[10], b'T');
    }

    #[test]
    fn insert_mints_distinct_hex_ids() {
        let s = mem();
        let a = ins(&s, "a");
        let b = ins(&s, "b");
        assert_ne!(a, b); // each insert mints a fresh, collision-checked id
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(s.get(&a).unwrap().is_some() && s.get(&b).unwrap().is_some());
    }

    #[test]
    fn upsert_note_by_id_replaces_in_place_and_keeps_fts_synced() {
        let s = mem();
        s.upsert_note(
            "a1b2c3",
            "first body",
            "",
            None,
            None,
            "note",
            0,
            "1970-01-01T00:00:00Z",
            &[0.1],
        )
        .unwrap();
        // Re-upserting the same id replaces the row (no duplicate) and re-syncs FTS.
        s.upsert_note(
            "a1b2c3",
            "second body",
            "",
            None,
            None,
            "note",
            0,
            "1970-01-01T00:00:00Z",
            &[0.2],
        )
        .unwrap();
        assert_eq!(
            count(&s, "SELECT count(*) FROM notes WHERE id = 'a1b2c3'"),
            1
        );
        assert_eq!(s.get("a1b2c3").unwrap().unwrap().body, "second body");
        // FTS reflects the new body, not the old one.
        let hits = s.fts_search("\"second\"", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a1b2c3");
        assert!(s.fts_search("\"first\"", 10).unwrap().is_empty());
        assert_eq!(
            count(&s, "SELECT count(*) FROM notes"),
            count(&s, "SELECT count(*) FROM notes_fts"),
        );
    }

    #[test]
    fn reopen_tempfile_preserves_data_and_schema_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let id = {
            let s = Store::open(&path).unwrap();
            ins(&s, "persisted")
        };
        // Reopen: schema is CREATE IF NOT EXISTS, data survives.
        let s2 = Store::open(&path).unwrap();
        assert_eq!(s2.get(&id).unwrap().unwrap().body, "persisted");
    }

    #[test]
    fn create_makes_usable_db_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        Store::create(&path).unwrap();
        let s = Store::open(&path).unwrap();
        assert!(s.all().unwrap().is_empty());
    }

    /// Build a tiny code graph by hand: product defs `parse`/`helper`/`Config` plus
    /// one TEST symbol `parse_works`, a resolved call and an external one. Exercises
    /// the Tier-1 query methods and the is_test filter.
    fn seed_code_graph(s: &Store) {
        s.upsert_code_file("a.rs", "rust", "h1", 0).unwrap();
        // last arg = is_test
        s.insert_code_symbol(
            "sp",
            "a.rs",
            "parse",
            "function",
            1,
            Some("fn parse()"),
            false,
        )
        .unwrap();
        s.insert_code_symbol(
            "sh",
            "a.rs",
            "helper",
            "function",
            5,
            Some("fn helper()"),
            false,
        )
        .unwrap();
        s.insert_code_symbol(
            "sc",
            "a.rs",
            "Config",
            "class",
            9,
            Some("struct Config"),
            false,
        )
        .unwrap();
        s.insert_code_symbol(
            "st",
            "a.rs",
            "parse_works",
            "method",
            20,
            Some("fn parse_works()"),
            true,
        )
        .unwrap();
        // defines edges
        s.insert_code_edge("a.rs", "defines", Some("sp"), "parse", 1)
            .unwrap();
        s.insert_code_edge("a.rs", "defines", Some("sh"), "helper", 5)
            .unwrap();
        // parse uses helper (resolved) and collect (external/unresolved)
        s.insert_code_edge("sp", "uses", Some("sh"), "helper", 2)
            .unwrap();
        s.insert_code_edge("sp", "uses", None, "collect", 2)
            .unwrap();
    }

    #[test]
    fn code_symbols_like_matches_substring_case_insensitively() {
        let s = mem();
        seed_code_graph(&s);
        let names: Vec<String> = s
            .code_symbols_like("ELP", false)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["helper"]); // 'helper' contains 'elp', case-insensitive
                                           // a needle matching nothing is empty, and `%`/`_` are literal (escaped).
        assert!(s.code_symbols_like("%", false).unwrap().is_empty());
    }

    #[test]
    fn code_callees_of_filters_unresolved_by_default() {
        let s = mem();
        seed_code_graph(&s);
        // Default: resolved-only -> just the in-project `helper`, not `collect`.
        let resolved: Vec<String> = s
            .code_callees_of("parse", true)
            .unwrap()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(resolved, vec!["helper"]);
        // With unresolved included, the external `collect` shows up too.
        let all: Vec<(String, bool)> = s
            .code_callees_of("parse", false)
            .unwrap()
            .into_iter()
            .map(|(name, _, ok)| (name, ok))
            .collect();
        assert!(all.contains(&("helper".to_string(), true)));
        assert!(all.contains(&("collect".to_string(), false)));
    }

    #[test]
    fn code_list_optionally_filters_by_kind() {
        let s = mem();
        seed_code_graph(&s);
        // Default hides the test symbol -> 3 product defs.
        let all = s.code_list(None, false).unwrap();
        assert_eq!(all.len(), 3); // parse, helper, Config
        let classes: Vec<String> = s
            .code_list(Some("class"), false)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(classes, vec!["Config"]); // only the struct/class kind
    }

    #[test]
    fn code_hubs_ranks_by_degree_and_caps() {
        let s = mem();
        seed_code_graph(&s);
        // Degrees over the `uses` graph (resolved only): parse out=1 (->helper),
        // helper in=1 (<-parse), Config 0. Ties (parse/helper at 1) break on path,line
        // -> parse (line 1) before helper (line 5); Config (0) last.
        let ranked: Vec<(String, i64)> = s
            .code_hubs(None, false, None)
            .unwrap()
            .into_iter()
            .map(|(r, d)| (r.name, d))
            .collect();
        assert_eq!(
            ranked,
            vec![
                ("parse".to_string(), 1),
                ("helper".to_string(), 1),
                ("Config".to_string(), 0),
            ]
        );
        // The cap keeps only the top-N hubs.
        let top1: Vec<String> = s
            .code_hubs(None, false, Some(1))
            .unwrap()
            .into_iter()
            .map(|(r, _)| r.name)
            .collect();
        assert_eq!(top1, vec!["parse"]);
        // Same kind / test filters as code_list: functions only, no test symbols.
        let funcs: Vec<String> = s
            .code_hubs(Some("function"), false, None)
            .unwrap()
            .into_iter()
            .map(|(r, _)| r.name)
            .collect();
        assert_eq!(funcs, vec!["parse", "helper"]); // Config (class) and parse_works (test) excluded
    }

    #[test]
    fn code_queries_hide_test_symbols_unless_included() {
        let s = mem();
        seed_code_graph(&s);
        // --list default: the test symbol parse_works is hidden.
        assert!(s
            .code_list(None, false)
            .unwrap()
            .iter()
            .all(|r| r.name != "parse_works"));
        // include_tests=true brings it back (4 symbols total).
        assert_eq!(s.code_list(None, true).unwrap().len(), 4);
        // --like default hides it; with tests it appears.
        assert!(s.code_symbols_like("parse", false).unwrap().len() == 1); // just `parse`
        assert_eq!(s.code_symbols_like("parse", true).unwrap().len(), 2); // + parse_works
                                                                          // --query by exact name respects the filter too.
        assert!(s
            .code_symbols_by_name("parse_works", false)
            .unwrap()
            .is_empty());
        assert_eq!(
            s.code_symbols_by_name("parse_works", true).unwrap().len(),
            1
        );
    }
}
