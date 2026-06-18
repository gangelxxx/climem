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

    // ---- derived-wipe (reindex --all) ------------------------------------

    /// Wipe the purely-derived tables (notes, notes_fts, sync, imports, edges) for
    /// a full rebuild. Everything in here can be reconstructed from the files:
    /// notes/*.md (plus their frontmatter relations and body `[[links]]`) and the
    /// imports/ originals with their `.meta.json` sidecars. We deliberately leave
    /// `oplog` (operation history) and `meta` (the embedder signature) alone, since
    /// those can't be.
    pub fn wipe_derived(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM notes_fts; DELETE FROM notes; DELETE FROM sync;
             DELETE FROM imports; DELETE FROM edges;",
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
             WHERE name IN ('notes','notes_fts','oplog','imports','meta','sync','edges')",
        );
        assert_eq!(n, 7);
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
}
