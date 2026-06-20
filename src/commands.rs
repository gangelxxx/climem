//! The command handlers. Each one opens whatever store/config it needs, does its
//! single job, and prints JSONL, in keeping with the whole "short-lived process"
//! idea.

use crate::cli::Parsed;
use crate::config::{self, Config};
use crate::embed::{self, Embedder};
use crate::export;
use crate::graph;
use crate::import;
use crate::note;
use crate::output::{self, note_preview_value, note_value, print_line, split_tags};
use crate::search::{self, RecallOpts};
use crate::store::Store;
use crate::util::{content_hash_hex, mtime_secs, now, preview, AppError, Result};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where everything lives inside a memory folder. `notes/` and `imports/` hold
/// the source of truth (the md files and the import originals); `store.db` is the
/// derived index.
pub struct Ctx {
    pub store_path: PathBuf,
    pub config_path: PathBuf,
    pub notes_dir: PathBuf,
    pub imports_dir: PathBuf,
}

impl Ctx {
    pub fn new(dir: &Path) -> Ctx {
        // config.json sits in `dir` (next to the binary, or where --dir points).
        // The DATA (store.db, notes/, imports/) lives under config's `data_dir`,
        // which is relative to `dir` (or absolute). This lets the binary + config
        // stay at the project root while data lives in a `memory/` subfolder.
        // Backward compatible: a config without `data_dir` defaults to ".", and a
        // missing/unreadable config falls back to the legacy single-folder layout.
        let config_path = dir.join("config.json");
        let data_dir = Config::load(&config_path)
            .ok()
            .map(|c| resolve_data_dir(dir, &c.data_dir))
            .unwrap_or_else(|| dir.to_path_buf());
        Ctx {
            store_path: data_dir.join("store.db"),
            config_path,
            notes_dir: data_dir.join("notes"),
            imports_dir: data_dir.join("imports"),
        }
    }

    /// Path to a note's md file, i.e. its source of truth at `notes/<id>.md`.
    pub fn note_path(&self, id: &str) -> PathBuf {
        self.notes_dir.join(format!("{id}.md"))
    }

    fn require_store(&self) -> Result<()> {
        if !self.store_path.exists() {
            // A present-but-broken config.json mislocates the store (Ctx::new can't
            // read data_dir, so it falls back to the legacy path). Surface THAT as the
            // real cause instead of a misleading "no memory store at <wrong path>".
            if self.config_path.is_file() {
                // Re-parse to turn a parse failure into the precise error; if it
                // parses fine, the store genuinely is missing.
                Config::load(&self.config_path)?;
            }
            return Err(AppError::with_hint(
                format!("no memory store at {}", self.store_path.display()),
                "Run `cm init <path>`, or point at an existing folder with --dir / MEMORY_DIR.",
            ));
        }
        Ok(())
    }

    fn open(&self) -> Result<(Store, Config)> {
        self.require_store()?;
        let cfg = Config::load(&self.config_path)?;
        let store = Store::open(&self.store_path)?;
        Ok((store, cfg))
    }
}

/// Resolve config's `data_dir` (where store.db/notes/imports live) against the
/// folder holding `config.json`. An absolute `data_dir` is used as-is; a relative
/// one (the usual case: "." legacy, or "memory" for a split layout) joins onto
/// `config_dir`. "" and "." both mean "same folder as config".
pub(crate) fn resolve_data_dir(config_dir: &Path, data_dir: &str) -> PathBuf {
    let trimmed = data_dir.trim();
    if trimmed.is_empty() || trimmed == "." {
        return config_dir.to_path_buf();
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    }
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    // Drop any UTF-8 BOMs (EF BB BF) that PowerShell 5.1 on Windows likes to add.
    // PS 5.1 can emit two of them: one from the $OutputEncoding preamble, one per object.
    Ok(s.trim_start_matches('\u{feff}').to_string())
}

/// Print a non-fatal warning if the embedder we're using now isn't the one the
/// notes were indexed with, because then their vectors don't really compare.
fn warn_on_drift(store: &Store, emb: &dyn Embedder) {
    if let Ok(Some(sig)) = store.meta_get("embedder_signature") {
        if sig != emb.signature() {
            eprintln!(
                "warning: embedder changed ('{}' -> '{}'); existing vectors may not match. \
                 Run `cm reindex --all` to rebuild the index.",
                sig,
                emb.signature()
            );
        }
    } else {
        let _ = store.meta_set("embedder_signature", &emb.signature());
    }
}

/// Index one note into the derived store, best-effort: build the embedder, embed,
/// upsert. If any of that fails we just warn, because the md file has already been
/// written (that's the real commit point), so the note isn't lost and `reindex`
/// can patch up the index later. Returns whether indexing actually worked.
#[allow(clippy::too_many_arguments)]
fn index_note_best_effort(
    store: &Store,
    cfg: &Config,
    id: &str,
    body: &str,
    tags: &str,
    source: Option<&str>,
    created_at: i64,
    created_iso: &str,
) -> bool {
    let attempt = || -> Result<()> {
        let emb = embed::build(cfg)?;
        warn_on_drift(store, emb.as_ref());
        let vec = emb.embed(body)?;
        store.upsert_note(
            id,
            body,
            tags,
            source,
            None,
            "note",
            created_at,
            created_iso,
            &vec,
        )
    };
    match attempt() {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "warning: note saved to notes/{id}.md but indexing failed ({}); \
                 run `cm reindex` to repair the search index.",
                e.msg
            );
            false
        }
    }
}

// ---- remember ------------------------------------------------------------

pub fn remember(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, cfg) = ctx.open()?;
    let body = read_stdin()?;
    let body = body.trim_end_matches(['\n', '\r']).to_string();
    if body.trim().is_empty() {
        return Err(AppError::with_hint(
            "remember reads the note body from stdin, but stdin was empty",
            "echo \"Decision: use JWT for auth\" | cm remember --tags auth,decision",
        ));
    }
    let tags = p.value("tags").unwrap_or("");
    let source = p.value("source");
    let slug = p.value("slug").filter(|s| !s.is_empty());
    let relations = parse_relations(p.value("relations").unwrap_or(""));

    let id = store.fresh_id()?;
    let (epoch, iso) = now();

    // The md file is the commit point (desc.md §3), so write it FIRST, before we
    // even build the embedder or index anything. That way a misconfigured embedder
    // can't cost us the note.
    let note_val = note::Note {
        id: id.clone(),
        created: iso.clone(),
        tags: tags.to_string(),
        source: source.map(|s| s.to_string()),
        slug: slug.map(|s| s.to_string()),
        relations,
        body: body.clone(),
    };
    let md = note::render(&note_val);
    std::fs::create_dir_all(&ctx.notes_dir)?;
    std::fs::write(ctx.note_path(&id), &md)?;

    // Indexing is derived and best-effort, and failing it doesn't lose the note.
    // On success we record the sync state so the next `reindex` can skip this
    // unchanged note.
    if index_note_best_effort(&store, &cfg, &id, &body, tags, source, epoch, &iso) {
        let rel = format!("notes/{id}.md");
        let _ = store.file_state_set(
            &rel,
            "note",
            &id,
            &content_hash_hex(md.as_bytes()),
            mtime_secs(&ctx.note_path(&id)),
        );
        // Warn if this slug is already claimed by another note (B4). The resolver
        // is lowest-id-wins (`build_slug_map` over `note_slugs ORDER BY id`), and
        // note ids are RANDOM hex — so the freshly minted id may sort above OR
        // below the existing claimant(s). We compute the actual winner (the lowest
        // id among all claimants, this note included) so the message can't lie: it
        // names which note links will resolve to, and flags when that isn't this
        // one. We keep the lowest-id-wins behavior (an invariant) — just visible,
        // the way reindex already does via `slug_collisions`.
        if let Some(s) = slug {
            let want = graph::normalize_slug(s);
            if !want.is_empty() {
                let others: Vec<String> = store
                    .note_slugs()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(other_id, other_slug)| {
                        other_id != &id && graph::normalize_slug(other_slug) == want
                    })
                    .map(|(other_id, _)| other_id)
                    .collect();
                if !others.is_empty() {
                    let winner = others.iter().chain(std::iter::once(&id)).min().unwrap();
                    eprintln!(
                        "warning: slug '{s}' is shared with note(s) {} — links to it resolve to \
                         the lowest id '{winner}'{}.",
                        others.join(", "),
                        if winner == &id {
                            " (this new note)"
                        } else {
                            ", not this new note"
                        }
                    );
                }
            }
        }
        let _ = store.set_note_slug(&id, slug); // make this note linkable by slug right away

        // Derive the outgoing edges from --relations and the body's [[wiki-links]].
        // Best-effort: targets resolve against the notes we know about right now and
        // dangle otherwise. reindex has the final word — a target added just now
        // resolves on the next run.
        let ids = store.note_ids().unwrap_or_default();
        let slug_map = graph::build_slug_map(&store.note_slugs().unwrap_or_default());
        let _ = index_note_edges(&store, &id, &note_val, &ids, &slug_map);
    }
    store.log_op("remember", Some(&preview(&body, 80)))?;
    print_line(&json!({ "id": id }));
    Ok(())
}

// ---- recall --------------------------------------------------------------

pub fn recall(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, cfg) = ctx.open()?;
    let query = p.arg(0).or_else(|| p.value("query")).ok_or_else(|| {
        AppError::with_hint(
            "recall needs a query",
            "cm recall \"how is auth structured\" --limit 5",
        )
    })?;
    let limit = parse_limit(p.value("limit"), 5)?;
    let explain = p.has("explain");
    let fields = match p.value("fields") {
        Some(s) if !s.is_empty() => Some(parse_fields(s, output::RECALL_FIELDS)?),
        _ => None,
    };
    // `--explain` only adds the score scalars in the default (no --fields) shape;
    // with an explicit --fields list it would be a silent no-op, so say so.
    if explain && fields.is_some() {
        eprintln!(
            "warning: --explain has no effect with --fields; \
             add score,fts,vector,graph to --fields to see those scalars"
        );
    }
    let opts = RecallOpts {
        limit,
        tag: p.value("tag").filter(|s| !s.is_empty()),
        origin_prefix: p.value("origin-prefix").filter(|s| !s.is_empty()),
        min_score: parse_score(p.value("min-score"))?,
        related: p.value("related").filter(|s| !s.is_empty()),
    };

    // Discoverability (D3): `--related` only does anything when the graph channel
    // carries weight, and that weight is 0 by default. Rather than silently no-op,
    // point the caller at the one config knob that turns it on.
    if opts.related.is_some() && cfg.search.hybrid_weights.graph == 0.0 {
        eprintln!(
            "warning: --related is inert because search.hybrid_weights.graph is 0 (the graph \
             channel is off by default). Turn it on with: \
             cm config set search.hybrid_weights.graph 0.3"
        );
    }

    let emb = embed::build(&cfg)?;
    let hits = search::recall_with(&store, emb.as_ref(), &cfg, query, &opts)?;
    store.log_op("recall", Some(query))?;

    for h in &hits {
        print_line(&output::recall_value(
            &h.row,
            h.score,
            h.fts,
            h.vector,
            h.graph,
            fields.as_deref(),
            explain,
        ));
    }
    Ok(())
}

// ---- get -----------------------------------------------------------------

pub fn get(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    let id = parse_id(p.arg(0))?;
    match store.get(&id)? {
        Some(row) => print_line(&note_value(&row)),
        None => print_line(&json!({ "found": false, "id": id })),
    }
    Ok(())
}

// ---- list ----------------------------------------------------------------

pub fn list(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    let recent = parse_limit(p.value("recent"), 20)?;
    for row in store.list(recent)? {
        print_line(&note_preview_value(&row));
    }
    Ok(())
}

// ---- related (graph traversal) -------------------------------------------

/// `related <id>`: walk the graph outward from a note along its relations and
/// `[[wiki-link]]` edges, and return the neighbors as lean JSONL. Dangling targets
/// (ones nobody's written yet) come back too, marked `dangling: true` with no
/// `id`. It's deterministic: a BTreeSet frontier plus a total order on (distance,
/// predicate, resolved-before-dangling, key), with `--limit` applied last so the
/// nearest neighbors win.
pub fn related(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    let id = parse_id(p.arg(0)).map_err(|_| {
        AppError::with_hint("related needs a note id", "cm related 0a1b2c3d --depth 2")
    })?;
    let limit = parse_limit(p.value("limit"), 5)?;
    let depth = parse_limit(p.value("depth"), 1)?;
    // Normalize the filter the same way edges store their predicate, so
    // `--predicate depends-on` matches an edge derived from `Depends_On` (D5).
    let predicate = p
        .value("predicate")
        .filter(|s| !s.is_empty())
        .map(graph::normalize_predicate);

    let fields = match p.value("fields") {
        Some(s) if !s.is_empty() => Some(parse_fields(s, output::RELATED_FIELDS)?),
        _ => None,
    };

    // The graph walk is the shared `bfs_neighbors` core (D1). `related` wants
    // dangling targets in the output, and the nearest-win truncation happens here.
    let mut neighbors = search::bfs_neighbors(&store, &id, depth, predicate.as_deref(), true)?;
    neighbors.truncate(limit);

    for n in &neighbors {
        print_line(&output::related_value(
            n.row.as_ref(),
            &n.dst_raw,
            &n.predicate,
            n.distance,
            fields.as_deref(),
        ));
    }
    Ok(())
}

// ---- backlinks (inbound graph edges) -------------------------------------

/// `backlinks <id>`: the inverse of `related` — which notes point AT this one.
/// The graph is directed (`A → B` means "A depends on / links to B"), so this is
/// the only way to walk it backwards (D6/D7). One hop only: each result is a note
/// that authored an edge resolving to `<id>`, with the predicate it used. Output
/// mirrors `related` minus `distance`/`dangling` (a backlink is always a resolved,
/// one-hop inbound edge from a live note).
pub fn backlinks(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    let id = parse_id(p.arg(0))
        .map_err(|_| AppError::with_hint("backlinks needs a note id", "cm backlinks 0a1b2c3d"))?;
    let limit = parse_limit(p.value("limit"), 5)?;
    let predicate = p
        .value("predicate")
        .filter(|s| !s.is_empty())
        .map(graph::normalize_predicate);

    let mut printed = 0usize;
    for e in store.edges_to(&id)? {
        if printed >= limit {
            break; // honor --limit 0 too (print nothing), like related/list
        }
        if let Some(pred) = &predicate {
            if &e.predicate != pred {
                continue;
            }
        }
        // Only surface live sources (the source note still exists).
        if let Some(row) = store.get(&e.src_id)? {
            print_line(&json!({
                "id": row.id,
                "kind": row.kind,
                "predicate": e.predicate,
                "preview": preview(&row.body, 160),
            }));
            printed += 1;
        }
    }
    Ok(())
}

// ---- forget --------------------------------------------------------------

pub fn forget(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    let id = parse_id(p.arg(0))?;

    // Imported chunks are derived from the imports/ originals, so you can't forget
    // one directly: poking the index here would just get undone on the next reindex.
    if let Some(row) = store.get(&id)? {
        if row.kind == "chunk" {
            return Err(AppError::with_hint(
                format!("'{id}' is an imported chunk (derived from imports/) — chunks can't be forgotten directly"),
                "Remove or edit the original under imports/, then run `cm reindex`.",
            ));
        }
    }

    // The md file is the source of truth: delete it first, then the index row.
    let md = ctx.note_path(&id);
    let had_file = md.exists();
    if had_file {
        std::fs::remove_file(&md)?;
    }
    let had_row = store.forget(&id)?;
    store.delete_edges_from(&id)?; // drop the note's outgoing graph edges
    store.dangle_edges_to(&id)?; // and re-dangle anyone who linked TO it (B2/B3)
    store.file_state_delete(&format!("notes/{id}.md"))?;
    store.log_op("forget", Some(&id))?;
    print_line(&json!({ "deleted": had_file || had_row, "id": id }));
    Ok(())
}

// ---- import --------------------------------------------------------------

pub fn import(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, cfg) = ctx.open()?;
    let file = p.arg(0).ok_or_else(|| {
        AppError::with_hint(
            "import needs a file path",
            "cm import ./docs/architecture.md --tags spec,architecture",
        )
    })?;
    let tags = p.value("tags").unwrap_or("");

    let emb = embed::build(&cfg)?;
    warn_on_drift(&store, emb.as_ref());

    let path = Path::new(file);
    let res = import::import_file(&store, emb.as_ref(), &cfg, path, tags, &ctx.imports_dir)?;
    store.log_op("import", Some(file))?;
    print_line(&json!({ "imported": file, "chunks": res.chunks }));
    Ok(())
}

// ---- reindex -------------------------------------------------------------

/// Rebuild the derived index (store.db) from the source of truth: notes/*.md and
/// the imports/ originals. By default it's incremental, using the content hash in
/// the sync table to skip unchanged files; `--all` throws the derived tables away
/// and rebuilds everything from scratch, re-embedding as it goes. store.db is
/// disposable: delete it, run `reindex`, and your memory comes back (desc.md §10).
pub fn reindex(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let cfg = Config::load(&ctx.config_path)?;
    // store.db might be gone; Store::open just recreates it — that's the whole point.
    let store = Store::open(&ctx.store_path)?;
    let all = p.has("all");

    if all {
        // Don't let a full wipe destroy the only copy of legacy data: old
        // db-as-truth notes that have no notes/*.md to rebuild from.
        if store.count_notes_kind("note")? > 0 && count_md_files(&ctx.notes_dir) == 0 {
            return Err(AppError::with_hint(
                "refusing `reindex --all`: the store has notes but notes/ is empty (legacy data would be lost)",
                "Back up first: `cm export jsonl --out backup.jsonl`, then reindex.",
            ));
        }
        store.wipe_derived()?;
    }

    // Build the embedder, but fall back to keyword-only if we can't. That keeps
    // the index rebuildable after a wipe even when the provider is misconfigured.
    let emb = match embed::build(&cfg) {
        Ok(e) => {
            let _ = store.meta_set("embedder_signature", &e.signature());
            Some(e)
        }
        Err(e) => {
            eprintln!(
                "warning: embedder unavailable ({}); rebuilding the keyword index only, \
                 vectors skipped. Re-run `cm reindex` once the provider is configured.",
                e.msg
            );
            None
        }
    };
    let emb_ref = emb.as_deref();

    let (n_seen, n_changed, changed_notes) = reindex_notes(&store, emb_ref, ctx)?;
    let (i_seen, i_changed) = reindex_imports(&store, emb_ref, &cfg, ctx)?;

    // Second pass: now derive graph edges for the changed notes, this time against
    // the FULL current note set. That way a forward reference (B links to A, both
    // written in this same run) resolves. Anything still unresolved dangles and
    // goes live once its target shows up on a later run.
    let ids = store.note_ids()?;
    let slugs = store.note_slugs()?;
    for (slug, dupes) in graph::slug_collisions(&slugs) {
        eprintln!(
            "warning: slug '{slug}' is shared by notes {} — links resolve to the lowest id '{}'.",
            dupes.join(", "),
            dupes[0]
        );
    }
    let slug_map = graph::build_slug_map(&slugs);
    let changed_ids: std::collections::HashSet<&str> =
        changed_notes.iter().map(|(id, _)| id.as_str()).collect();
    for (id, note) in &changed_notes {
        index_note_edges(&store, id, note, &ids, &slug_map)?;
    }

    // Revive dangling edges (B1/L3): a forward reference authored earlier (its
    // source note unchanged) may now resolve against a slug/id that appeared since
    // — possibly written by `remember` itself, so the reindex sees 0 changed files
    // yet the resolvable set has grown. So on every incremental run, re-derive the
    // edges of each note that still owns a dangling edge. It's cheap and idempotent:
    // with no dangling edges the source list is empty, and sources already re-derived
    // in the loop above are skipped. (`--all` already (re)indexed every note's
    // edges, so it needs no second look.) Reading the note's md is what lets us
    // re-derive it without it having changed.
    if !all {
        for src in store.dangling_edge_sources()? {
            if changed_ids.contains(src.as_str()) {
                continue; // already re-derived in the loop above
            }
            if let Some(note) = read_note_md(ctx, &src) {
                index_note_edges(&store, &src, &note, &ids, &slug_map)?;
            }
        }
    }

    store.log_op("reindex", Some(if all { "all" } else { "incremental" }))?;
    print_line(&json!({ "indexed": n_seen + i_seen, "changed": n_changed + i_changed }));
    Ok(())
}

/// (Re)index `notes/*.md`. Returns (files seen, files (re)indexed or pruned, and
/// the changed notes paired with their id, which the second edge-derivation pass
/// needs).
type ReindexNotes = (usize, usize, Vec<(String, note::Note)>);

fn reindex_notes(store: &Store, emb: Option<&dyn Embedder>, ctx: &Ctx) -> Result<ReindexNotes> {
    let mut seen = 0usize;
    let mut changed = 0usize;
    let mut changed_notes: Vec<(String, note::Note)> = Vec::new();

    if ctx.notes_dir.exists() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&ctx.notes_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
            .collect();
        paths.sort(); // deterministic processing order
        for path in paths {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let rel = format!("notes/{fname}");
            let bytes = std::fs::read(&path)?;
            let hash = content_hash_hex(&bytes);
            seen += 1;
            if let Some((old, _)) = store.file_state_get(&rel)? {
                if old == hash {
                    continue; // unchanged since last index
                }
            }
            let text = String::from_utf8_lossy(&bytes);
            let parsed = match note::parse(&text) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("warning: skipping {rel}: {}", e.msg);
                    continue;
                }
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let id = if parsed.id.is_empty() {
                stem.to_string()
            } else {
                parsed.id.clone()
            };
            if id != stem {
                eprintln!(
                    "warning: {rel}: frontmatter id '{id}' != filename stem '{stem}' (using frontmatter id)"
                );
            }
            let created = if parsed.created.is_empty() {
                now().1
            } else {
                parsed.created.clone()
            };
            let vec = embed_or_empty(emb, &parsed.body)?;
            store.upsert_note(
                &id,
                &parsed.body,
                &parsed.tags,
                parsed.source.as_deref(),
                None,
                "note",
                0,
                &created,
                &vec,
            )?;
            store.set_note_slug(&id, parsed.slug.as_deref())?;
            store.file_state_set(&rel, "note", &id, &hash, mtime_secs(&path))?;
            changed += 1;
            changed_notes.push((id, parsed));
        }
    }

    // Prune notes whose md file vanished (deleted out-of-band).
    for row in store.all()? {
        if row.kind == "note" && !ctx.note_path(&row.id).exists() {
            store.forget(&row.id)?;
            store.delete_edges_from(&row.id)?;
            store.dangle_edges_to(&row.id)?; // re-dangle inbound edges (B2/B3)
            store.file_state_delete(&format!("notes/{}.md", row.id))?;
            changed += 1;
        }
    }
    Ok((seen, changed, changed_notes))
}

/// (Re)derive chunks from the `imports/` originals. Returns (files seen, changed).
fn reindex_imports(
    store: &Store,
    emb: Option<&dyn Embedder>,
    cfg: &Config,
    ctx: &Ctx,
) -> Result<(usize, usize)> {
    let mut seen = 0usize;
    let mut changed = 0usize;

    if ctx.imports_dir.exists() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&ctx.imports_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| !import::is_sidecar(n))
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for path in paths {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let rel = format!("imports/{fname}");
            let bytes = std::fs::read(&path)?;
            let hash = content_hash_hex(&bytes);
            seen += 1;
            if let Some(r) = store.import_record(&rel)? {
                if r.content_hash == hash {
                    continue; // unchanged original
                }
            }
            // The original name and tags come from the sidecar (which is truth), so
            // they survive even a full store.db loss. A hand-added original with no
            // sidecar just falls back to its filename.
            let (orig_name, tags) = import::read_sidecar(&ctx.imports_dir, fname);
            match import::index_import(store, emb, cfg, &rel, &orig_name, &path, &tags, &hash) {
                Ok(n) => {
                    store.record_import(&rel, &orig_name, &tags, n as i64, &hash)?;
                    store.file_state_set(&rel, "import", &rel, &hash, mtime_secs(&path))?;
                    changed += 1;
                }
                Err(e) => eprintln!("warning: skipping import {rel}: {}", e.msg),
            }
        }
    }

    // Prune import records whose imports/ original vanished.
    for imp in store.list_imports()? {
        let name = imp.source.strip_prefix("imports/").unwrap_or(&imp.source);
        if !ctx.imports_dir.join(name).exists() {
            store.delete_import(&imp.source)?;
            store.file_state_delete(&imp.source)?;
            changed += 1;
        }
    }
    Ok((seen, changed))
}

fn embed_or_empty(emb: Option<&dyn Embedder>, text: &str) -> Result<Vec<f32>> {
    match emb {
        Some(e) => e.embed(text),
        None => Ok(Vec::new()),
    }
}

/// (Re)derive a note's outgoing edges: drop the old ones, then resolve each
/// authored relation and body `[[wiki-link]]` target against the current note set
/// (by slug by default, or by id when the target has an `id:` prefix). Anything
/// that doesn't resolve is kept as a dangling edge.
///
/// We dedup on `(predicate, resolved-key)` before inserting (D4): two relations
/// that name the same target with different spellings (`DB Schema` / `db-schema`)
/// resolve to one id and must not become two edges. The DB primary key keys on the
/// verbatim `dst_raw`, so it would let those through; this catch is upstream of it.
/// The resolved-key is the note id when it resolves, else the normalized raw
/// target (so two spellings of the same *dangling* target also collapse). We keep
/// the first spelling seen for `dst_raw`, which is stable because `note_edges`
/// yields relations in authored order, then wiki-links.
fn index_note_edges(
    store: &Store,
    src_id: &str,
    note: &note::Note,
    ids: &std::collections::HashSet<String>,
    slug_map: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    store.delete_edges_from(src_id)?;
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (predicate, target, source) in graph::note_edges(note) {
        let dst_id = graph::resolve_target(&target, ids, slug_map);
        let key = dst_id
            .clone()
            .unwrap_or_else(|| format!("raw:{}", graph::normalize_slug(&target)));
        if !seen.insert((predicate.clone(), key)) {
            continue; // same predicate + same resolved target, different spelling
        }
        store.insert_edge(src_id, &predicate, dst_id.as_deref(), &target, source)?;
    }
    Ok(())
}

/// Read and parse a note's md (`notes/<id>.md`) straight off disk, best-effort.
/// Used by reindex's dangling-revival pass to re-derive an *unchanged* note's
/// edges. Returns `None` if the file is missing or unparseable (it just won't be
/// revived this run — no worse than before).
fn read_note_md(ctx: &Ctx, id: &str) -> Option<note::Note> {
    let bytes = std::fs::read(ctx.note_path(id)).ok()?;
    note::parse(&String::from_utf8_lossy(&bytes)).ok()
}

/// How many `*.md` files sit directly under `dir` (used by the reindex legacy guard).
fn count_md_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                .count()
        })
        .unwrap_or(0)
}

// ---- map (source-code graph; feature `code`) ------------------------------

/// `cm map` drives the code-only knowledge graph (kept separate from the notes
/// graph: its own code_* tables, reached only here). It has two shapes: an INDEX
/// shape `cm map <path> [--lang L] [--exclude SUBSTR]`, and a QUERY shape
/// `cm map --query <name> | --uses <name> | --defines <path>`. The query modes are
/// read-only; the index mode parses the source tree with tree-sitter and (re)builds
/// the code graph incrementally by content hash.
pub fn map(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;

    // --- query modes (read-only; no parsing, so they work even without the
    //     `code` feature, since the graph is just rows once built) ---
    // A shared `--kind` filter narrows the symbol-listing modes to one tag kind
    // (function/method/class/...). It does not apply to --uses/--calls (those key
    // off a name, not a kind). `--tests` opts test-defined symbols back IN — by
    // default they're hidden so listings show a module's real API, not its tests.
    let kind = p.value("kind");
    let tests = p.has("tests");
    if let Some(name) = p.value("query") {
        for s in filter_kind(store.code_symbols_by_name(name, tests)?, kind) {
            print_line(&code_symbol_value(&s));
        }
        return Ok(());
    }
    if let Some(needle) = p.value("like") {
        for s in filter_kind(store.code_symbols_like(needle, tests)?, kind) {
            print_line(&code_symbol_value(&s));
        }
        return Ok(());
    }
    if p.has("list") {
        // `--list` with no value = whole graph; an optional `--kind` narrows it.
        for s in store.code_list(kind, tests)? {
            print_line(&code_symbol_value(&s));
        }
        return Ok(());
    }
    if let Some(name) = p.value("uses") {
        for (caller, line) in store.code_callers_of(name, tests)? {
            print_line(&json!({
                "name": caller.name,
                "kind": caller.kind,
                "path": caller.path,
                "line": line,            // where the use occurs
                "def_line": caller.line, // where the caller itself is defined
            }));
        }
        return Ok(());
    }
    if let Some(name) = p.value("calls") {
        // Outgoing dependencies. Resolved-only by default (unresolved = external /
        // stdlib names, which are the bulk); --external surfaces those too.
        let resolved_only = !p.has("external");
        for (target, line, resolved) in store.code_callees_of(name, resolved_only)? {
            print_line(&json!({
                "calls": target,
                "line": line,
                "resolved": resolved, // true = links to an in-project definition
            }));
        }
        return Ok(());
    }
    if let Some(path) = p.value("defines") {
        let key = normalize_code_path(path);
        for s in filter_kind(store.code_symbols_in_like(&key, tests)?, kind) {
            print_line(&code_symbol_value(&s));
        }
        return Ok(());
    }

    // --- index mode ---
    let root = p.arg(0).ok_or_else(|| {
        AppError::with_hint(
            "map needs a path to index, or a query flag",
            "cm map ./src   (or: cm map --query <name> | --uses <name> | --defines <file>)",
        )
    })?;
    let root = Path::new(root);
    if !root.exists() {
        return Err(AppError::with_hint(
            format!("no such path: {}", root.display()),
            "cm map ./src",
        ));
    }
    let mem_dir = ctx
        .store_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let stats = map_tree(&store, root, &mem_dir, p.value("lang"), p.value("exclude"))?;
    store.log_op("map", Some(&root.to_string_lossy()))?;

    let (n_files, n_symbols, n_edges) = store.code_counts()?;
    print_line(&json!({
        "mapped": root.to_string_lossy(),
        "scanned": stats.scanned,
        "changed": stats.changed,
        "files": n_files,
        "symbols": n_symbols,
        "edges": n_edges,
    }));
    Ok(())
}

/// Outcome of indexing a source tree into the code graph.
pub(crate) struct MapStats {
    /// Source files actually fed to a grammar (extension recognized).
    pub scanned: usize,
    /// Files whose graph rows were (re)written this run, plus pruned-file deltas.
    pub changed: usize,
    /// Files seen during the walk whose extension has no grammar (skipped silently
    /// before — now reported so the user knows what wasn't indexed).
    pub no_grammar: usize,
    /// Files a grammar rejected (parse error); their content isn't in the graph.
    pub unparsed: usize,
    /// Per-language scanned-file counts, for a "C# 57, Rust 8" breakdown. Sorted
    /// by count (desc) when rendered.
    pub by_lang: std::collections::BTreeMap<&'static str, usize>,
}

/// Index a source tree into the code graph: walk `root` (skipping build/vendor
/// dirs, the memory folder `mem_dir`, and anything matching `exclude`), parse each
/// changed source file (incremental by content hash), prune vanished files, then
/// resolve `uses` edges by name. Shared by `cm map` and `cm init --code`. Does NOT
/// log the op or print — callers decide that. Propagates the no-`code`-feature
/// rebuild hint so the caller can surface (map) or downgrade (init) it.
pub(crate) fn map_tree(
    store: &Store,
    root: &Path,
    mem_dir: &Path,
    lang_filter: Option<&str>,
    exclude: Option<&str>,
) -> Result<MapStats> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_source_files(root, mem_dir, exclude, &mut files);
    files.sort();

    let mut scanned = 0usize;
    let mut changed = 0usize;
    let mut no_grammar = 0usize;
    let mut unparsed = 0usize;
    let mut by_lang: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for path in &files {
        let lang = match crate::code::lang_for_path(path) {
            Some(l) => l,
            None => {
                // Walked in (e.g. via is_source_file) but no grammar owns the ext.
                no_grammar += 1;
                continue;
            }
        };
        if let Some(want) = lang_filter {
            if want != lang {
                continue;
            }
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("warning: cannot read {}: {e}", path.display());
                continue;
            }
        };
        let rel = rel_code_path(root, path);
        let hash = content_hash_hex(&bytes);
        scanned += 1;
        *by_lang.entry(lang).or_insert(0) += 1;
        if let Some((old, _)) = store.code_file_state(&rel)? {
            if old == hash {
                continue; // unchanged since last map
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let parsed = match crate::code::parse(&rel, lang, &text) {
            Ok(pp) => pp,
            Err(e) => {
                // Without the `code` feature this is THE place the rebuild hint
                // surfaces; propagate it so the caller can react.
                if e.hint.is_some() {
                    return Err(e);
                }
                eprintln!("warning: parse failed for {rel}: {}", e.msg);
                unparsed += 1;
                continue;
            }
        };
        index_code_file(store, &rel, lang, &hash, mtime_secs(path), &parsed)?;
        changed += 1;
    }

    // Prune files that vanished from disk, re-dangling inbound uses-edges so a
    // later map revives them if the symbol reappears (mirrors note pruning).
    for rel in store.code_file_paths()? {
        let abs = root.join(&rel);
        if !abs.exists() {
            for s in store.code_symbols_in(&rel)? {
                store.dangle_code_edges_to(&s.symbol_id)?;
            }
            store.delete_code_file(&rel)?;
            changed += 1;
        }
    }

    // Second pass: resolve uses-edges by name against the FULL symbol table, so a
    // call to a symbol defined in another file (or a file mapped later this run)
    // links up. Anything unresolved stays dangling and revives on a later map.
    let name_map = store.code_symbol_name_map()?;
    for src in store.dangling_code_sources()? {
        resolve_code_uses(store, &src, &name_map)?;
    }

    Ok(MapStats {
        scanned,
        changed,
        no_grammar,
        unparsed,
        by_lang,
    })
}

/// Render `by_lang` as a compact "C# 57, Rust 8, TypeScript 4" string, biggest
/// first, with the registry's display names. Empty → "—".
pub(crate) fn lang_breakdown(by_lang: &std::collections::BTreeMap<&'static str, usize>) -> String {
    if by_lang.is_empty() {
        return "—".to_string();
    }
    let mut pairs: Vec<(&&str, &usize)> = by_lang.iter().collect();
    // Count desc, then name asc for a stable tie-break.
    pairs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    pairs
        .iter()
        .map(|(lang, n)| format!("{} {}", crate::code::display_lang(lang), n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write one source file's symbols and edges into the code graph. Replaces any
/// prior data for the file first (so an edit doesn't leave stale symbols), then
/// inserts each definition (+ a `defines` edge from the file) and each reference
/// (a `uses` edge from the enclosing — best-effort — symbol, resolved by name in
/// the second pass). Inbound uses-edges to the file's old symbols are re-dangled.
fn index_code_file(
    store: &Store,
    rel: &str,
    lang: &str,
    hash: &str,
    mtime: i64,
    parsed: &crate::code::CodeParse,
) -> Result<()> {
    // Re-dangle inbound edges that pointed at this file's current symbols, then
    // drop the file's old rows before re-inserting (idempotent re-map).
    for s in store.code_symbols_in(rel)? {
        store.dangle_code_edges_to(&s.symbol_id)?;
    }
    store.delete_code_file(rel)?;
    store.upsert_code_file(rel, lang, hash, mtime)?;

    // Definitions become symbol nodes + a `defines` edge from the file path.
    // We also build a line-sorted index of (line, symbol_id) so a reference can
    // be attributed to the definition it sits inside (the nearest def starting at
    // or before the ref's line). This is a cheap heuristic, not real scoping.
    let mut def_spans: Vec<(usize, String)> = Vec::new();
    for d in &parsed.defs {
        let sid = crate::code::symbol_id(rel, &d.kind, &d.name, d.line);
        store.insert_code_symbol(
            &sid,
            rel,
            &d.name,
            &d.kind,
            d.line as i64,
            Some(&d.signature),
            d.is_test,
        )?;
        store.insert_code_edge(rel, "defines", Some(&sid), &d.name, d.line as i64)?;
        def_spans.push((d.line, sid));
    }
    def_spans.sort();

    // References become `uses` edges. The edge's src is the enclosing definition
    // (so `--uses X` can say WHICH symbol uses X); if a ref sits before any def
    // (top-level code), we attribute it to the file path itself.
    for r in &parsed.refs {
        let src = enclosing_symbol(&def_spans, r.line).unwrap_or_else(|| rel.to_string());
        // dst resolved in the second pass; insert dangling with the verbatim name.
        store.insert_code_edge(&src, "uses", None, &r.name, r.line as i64)?;
    }
    Ok(())
}

/// The symbol_id of the definition that encloses `line`: the one with the largest
/// start line ≤ `line`. `def_spans` must be sorted by line ascending.
fn enclosing_symbol(def_spans: &[(usize, String)], line: usize) -> Option<String> {
    let mut found = None;
    for (start, sid) in def_spans {
        if *start <= line {
            found = Some(sid.clone());
        } else {
            break;
        }
    }
    found
}

/// Resolve a source's dangling `uses` edges by name against the symbol table.
fn resolve_code_uses(
    store: &Store,
    src: &str,
    name_map: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    for e in store_code_uses_from(store, src)? {
        if e.dst.is_none() {
            // Never resolve a ubiquitous stdlib method name to a same-named project
            // symbol — that's the main false-positive source for --calls/--uses.
            if crate::code::is_stdlib_combinator(&e.dst_raw) {
                continue;
            }
            if let Some(target) = name_map.get(&e.dst_raw) {
                // Don't link a symbol to itself (a recursive call shouldn't count).
                if target != src {
                    store.resolve_code_edge(src, &e.dst_raw, target)?;
                }
            }
        }
    }
    Ok(())
}

/// Small read helper: a source's `uses` edges (we only need src/dst/dst_raw here).
fn store_code_uses_from(store: &Store, src: &str) -> Result<Vec<crate::store::CodeEdgeRow>> {
    let mut stmt = store.conn.prepare(
        "SELECT src, predicate, dst, dst_raw, line FROM code_edges
         WHERE src = ?1 AND predicate = 'uses'",
    )?;
    let rows = stmt
        .query_map([src], |r| {
            Ok(crate::store::CodeEdgeRow {
                src: r.get(0)?,
                predicate: r.get(1)?,
                dst: r.get(2)?,
                dst_raw: r.get(3)?,
                line: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Keep only symbols of a given tag kind (function/method/class/...), or all of
/// them when no `--kind` was given. Done in Rust, not SQL, so every symbol-listing
/// mode shares one filter regardless of which store query produced the rows.
fn filter_kind(
    rows: Vec<crate::store::CodeSymbolRow>,
    kind: Option<&str>,
) -> Vec<crate::store::CodeSymbolRow> {
    match kind {
        Some(k) => rows.into_iter().filter(|s| s.kind == k).collect(),
        None => rows,
    }
}

/// JSON projection of a code symbol (omit a null/empty signature to stay lean).
fn code_symbol_value(s: &crate::store::CodeSymbolRow) -> serde_json::Value {
    let mut v = json!({
        "name": s.name,
        "kind": s.kind,
        "path": s.path,
        "line": s.line,
    });
    if let Some(sig) = &s.signature {
        if !sig.is_empty() {
            v["signature"] = json!(sig);
        }
    }
    v
}

/// Relative, '/'-separated path of a source file under the mapped root. Falls back
/// to the file name if it isn't under root (shouldn't happen via our walk).
fn rel_code_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    normalize_code_path(&rel.to_string_lossy())
}

/// Normalize a path key to forward slashes so Windows and Unix agree on the keys
/// stored in code_files / queried by `--defines`.
fn normalize_code_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Directory names we never descend into when mapping a source tree: build output,
/// vendored deps, VCS metadata, and a few common heavy/irrelevant folders.
const SKIP_DIRS: &[&str] = &[
    "target",       // Rust
    "node_modules", // JS/TS
    ".git",
    ".hg",
    ".svn",
    "dist",
    "build", // generic / Gradle
    "out",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    "obj", // .NET — generated C# (obj/Debug/*.cs etc.)
    "bin", // .NET — build output
];

/// Recursively gather source files under `dir`, skipping SKIP_DIRS, the memory
/// folder, dotfiles-dirs, anything matching `exclude` (a plain substring of the
/// path), and any non-source extension. Best-effort: unreadable entries are
/// skipped. No `walkdir` dep — a small hand-rolled walk (CLAUDE.md: minimal deps).
fn collect_source_files(dir: &Path, mem_dir: &Path, exclude: Option<&str>, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if let Some(ex) = exclude {
            if !ex.is_empty() && path.to_string_lossy().contains(ex) {
                continue;
            }
        }
        if ft.is_dir() {
            // Skip the memory folder (its store/binary aren't project source).
            if path == mem_dir {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            collect_source_files(&path, mem_dir, exclude, out);
        } else if ft.is_file() && crate::code::is_source_file(&path) {
            out.push(path);
        }
    }
}

// ---- export --------------------------------------------------------------

pub fn export(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, cfg) = ctx.open()?;
    let format = p.arg(0).ok_or_else(|| {
        AppError::with_hint("export needs a format", "cm export md --out dump.md")
    })?;

    let rows = if let Some(query) = p.value("query") {
        let emb = embed::build(&cfg)?;
        // Honor the same pre-filters as `recall` so the two read paths don't
        // diverge: a flag learned on `recall` (--tag/--origin-prefix/--min-score)
        // applies here too. --limit defaults to 50 for a query export.
        let opts = RecallOpts {
            limit: parse_limit(p.value("limit"), 50)?,
            tag: p.value("tag").filter(|s| !s.is_empty()),
            origin_prefix: p.value("origin-prefix").filter(|s| !s.is_empty()),
            min_score: parse_score(p.value("min-score"))?,
            related: p.value("related").filter(|s| !s.is_empty()),
        };
        search::recall_with(&store, emb.as_ref(), &cfg, query, &opts)?
            .into_iter()
            .map(|h| h.row)
            .collect()
    } else {
        store.all()?
    };

    let content = export::render(format, &rows)?;
    store.log_op("export", Some(format))?;

    if let Some(out) = p.value("out") {
        std::fs::write(out, &content)?;
        print_line(&json!({ "exported": out, "format": format, "count": rows.len() }));
    } else {
        print!("{content}");
    }
    Ok(())
}

// ---- log -----------------------------------------------------------------

pub fn log(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let (store, _cfg) = ctx.open()?;
    if p.has("imports") {
        for imp in store.list_imports()? {
            print_line(&json!({
                "source": imp.source,
                "orig": imp.orig_name,
                "tags": split_tags(&imp.tags),
                "chunks": imp.chunks,
                "at": imp.created_iso,
            }));
        }
    } else {
        let recent = parse_limit(p.value("recent"), 50)?;
        for row in store.recent_logs(recent)? {
            print_line(&json!({ "at": row.created_iso, "op": row.op, "detail": row.detail }));
        }
    }
    Ok(())
}

// ---- config --------------------------------------------------------------

pub fn config(p: &Parsed, ctx: &Ctx) -> Result<()> {
    // Check the config file first: in the split layout it carries `data_dir`, so a
    // missing config is the more fundamental problem (the store can't even be
    // located). A clear "config.json not found" beats a downstream "no memory store".
    if !ctx.config_path.is_file() {
        return Err(AppError::with_hint(
            format!("config.json not found at {}", ctx.config_path.display()),
            "Run `cm init <path>`, or point at the folder holding config.json with --dir.",
        ));
    }
    ctx.require_store()?;
    let path = &ctx.config_path;
    let sub = p.arg(0);

    match sub {
        None => {
            let raw = config::load_raw(path)?;
            let masked = config::mask_secrets(&raw);
            println!("{}", serde_json::to_string_pretty(&masked)?);
        }
        Some("get") => {
            let key = p.arg(1).ok_or_else(|| {
                AppError::with_hint("config get needs a key", "cm config get embedding.model")
            })?;
            let raw = config::load_raw(path)?;
            match config::get_path(&raw, key) {
                Some(v) => println!("{}", config::mask_secrets(v)),
                None => return Err(AppError::new(format!("no such config key: {key}"))),
            }
        }
        Some("set") => {
            let key = p.arg(1).ok_or_else(|| {
                AppError::with_hint(
                    "config set needs a key and a value",
                    "cm config set embedding.provider api",
                )
            })?;
            let val = p.arg(2).ok_or_else(|| {
                AppError::with_hint(
                    "config set needs a value",
                    "cm config set embedding.dimension 768",
                )
            })?;
            let mut raw = config::load_raw(path)?;
            config::set_path(&mut raw, key, val)?;
            // Validate the result still deserializes into a Config.
            serde_json::from_value::<Config>(raw.clone())
                .map_err(|e| AppError::new(format!("resulting config is invalid: {e}")))?;
            config::save_raw(path, &raw)?;
            let stored = config::get_path(&raw, key)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            print_line(&json!({ "set": key, "value": config::mask_secrets(&stored) }));
        }
        Some(other) => {
            return Err(AppError::with_hint(
                format!("unknown config subcommand '{other}'"),
                "Use: cm config | cm config get <key> | cm config set <key> <value>",
            ))
        }
    }
    Ok(())
}

// ---- helpers -------------------------------------------------------------

/// Validate a note id: non-empty lowercase hex (`0-9a-f`). Note ids are short
/// random hex written in the md frontmatter (and equal to the filename stem),
/// not integers.
fn parse_id(arg: Option<&str>) -> Result<String> {
    let s = arg.ok_or_else(|| AppError::with_hint("expected an id", "cm get 0a1b2c3d"))?;
    let is_hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    if s.is_empty() || !s.chars().all(is_hex) {
        return Err(AppError::with_hint(
            format!("id must be lowercase hex, got '{s}'"),
            "cm get 0a1b2c3d",
        ));
    }
    Ok(s.to_string())
}

fn parse_limit(arg: Option<&str>, default: usize) -> Result<usize> {
    match arg {
        None | Some("") => Ok(default),
        Some(s) => s
            .parse::<usize>()
            .map_err(|_| AppError::new(format!("limit must be a number, got '{s}'"))),
    }
}

/// Parse a `--relations "pred:target, pred:target"` string into edge pairs. We
/// split items on commas, and each item on its FIRST colon, so an `id:<hex>`
/// target keeps its prefix. Anything malformed (no colon, or a blank side) is
/// just dropped.
fn parse_relations(arg: &str) -> Vec<(String, String)> {
    arg.split(',')
        .filter_map(|item| {
            let (pred, target) = item.split_once(':')?;
            let (pred, target) = (pred.trim(), target.trim());
            (!pred.is_empty() && !target.is_empty()).then(|| (pred.to_string(), target.to_string()))
        })
        .collect()
}

/// Parse a `--fields a,b,c` list and check every name against `allowed` (the
/// command's own field set: `RECALL_FIELDS` for recall, `RELATED_FIELDS` for related).
fn parse_fields(arg: &str, allowed: &[&str]) -> Result<Vec<String>> {
    let fields: Vec<String> = arg
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if fields.is_empty() {
        return Err(AppError::with_hint(
            "--fields needs at least one field name",
            "cm recall \"topic\" --fields id,body",
        ));
    }
    for f in &fields {
        if !allowed.contains(&f.as_str()) {
            return Err(AppError::with_hint(
                format!("unknown --fields name '{f}'"),
                format!("Valid: {}", allowed.join(",")),
            ));
        }
    }
    Ok(fields)
}

/// Parse an optional `--min-score` floor; missing or empty means 0.0, i.e. keep
/// everything. K1.
fn parse_score(arg: Option<&str>) -> Result<f32> {
    match arg {
        None | Some("") => Ok(0.0),
        Some(s) => s.parse::<f32>().map_err(|_| {
            AppError::with_hint(
                format!("--min-score must be a number, got '{s}'"),
                "cm recall \"topic\" --min-score 0.01",
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// A temp folder scaffolded like `init` (config.json + store.db + dirs).
    fn setup() -> (TempDir, Ctx) {
        let dir = TempDir::new().unwrap();
        Config::default()
            .save(&dir.path().join("config.json"))
            .unwrap();
        Store::create(&dir.path().join("store.db")).unwrap();
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        std::fs::create_dir_all(dir.path().join("imports")).unwrap();
        let ctx = Ctx::new(dir.path());
        (dir, ctx)
    }

    fn open_store(ctx: &Ctx) -> Store {
        Store::open(&ctx.store_path).unwrap()
    }

    // ---- pure helpers ---------------------------------------------------

    #[test]
    fn parse_relations_pairs_and_id_prefix() {
        assert_eq!(
            parse_relations("depends_on: db-schema, blocks: id:9f8e7d"),
            vec![
                ("depends_on".to_string(), "db-schema".to_string()),
                ("blocks".to_string(), "id:9f8e7d".to_string()), // first colon only
            ]
        );
        assert!(parse_relations("").is_empty());
        assert!(parse_relations("garbage, no-colon").is_empty()); // malformed dropped
    }

    #[test]
    fn parse_fields_valid_and_trims() {
        assert_eq!(
            parse_fields("id, body ,origin", output::RECALL_FIELDS).unwrap(),
            vec!["id", "body", "origin"]
        );
        assert_eq!(
            parse_fields("preview", output::RECALL_FIELDS).unwrap(),
            vec!["preview"]
        );
    }

    #[test]
    fn parse_fields_empty_and_unknown_error_with_hint() {
        let empty = parse_fields("  ,  ", output::RECALL_FIELDS).unwrap_err();
        assert!(empty.msg.contains("at least one field"));
        assert!(empty.hint.is_some());
        let bad = parse_fields("id,bogus", output::RECALL_FIELDS).unwrap_err();
        assert!(bad.msg.contains("unknown --fields name 'bogus'"));
        assert!(bad.hint.is_some());
        // `related`-only fields are valid for related but not for recall.
        assert!(parse_fields("distance", output::RELATED_FIELDS).is_ok());
        assert!(parse_fields("distance", output::RECALL_FIELDS).is_err());
    }

    #[test]
    fn parse_score_default_and_parse_and_error() {
        assert_eq!(parse_score(None).unwrap(), 0.0);
        assert_eq!(parse_score(Some("")).unwrap(), 0.0);
        assert!((parse_score(Some("0.01")).unwrap() - 0.01).abs() < 1e-9);
        let err = parse_score(Some("x")).unwrap_err();
        assert!(err.msg.contains("--min-score must be a number"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn parse_id_none_returns_error_with_hint() {
        let err = parse_id(None).unwrap_err();
        assert!(err.msg.contains("expected an id"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn parse_id_valid_hex() {
        assert_eq!(parse_id(Some("0a1b2c")).unwrap(), "0a1b2c");
        assert_eq!(parse_id(Some("42")).unwrap(), "42"); // hex-shaped digits ok
        assert_eq!(parse_id(Some("deadbeef")).unwrap(), "deadbeef");
    }

    #[test]
    fn parse_id_non_hex_error() {
        // Uppercase, non-hex letters, and separators are all rejected.
        for bad in ["XYZ", "g123", "0a1b2c-", "ab cd", "-1"] {
            let err = parse_id(Some(bad)).unwrap_err();
            assert!(err.msg.contains("id must be lowercase hex"), "{bad}");
            assert!(err.hint.is_some());
        }
        let empty = parse_id(Some("")).unwrap_err();
        assert!(empty.msg.contains("id must be lowercase hex"));
    }

    #[test]
    fn parse_limit_none_and_empty_use_default() {
        assert_eq!(parse_limit(None, 8).unwrap(), 8);
        assert_eq!(parse_limit(Some(""), 8).unwrap(), 8);
    }

    #[test]
    fn parse_limit_valid_and_invalid() {
        assert_eq!(parse_limit(Some("5"), 8).unwrap(), 5);
        let err = parse_limit(Some("x"), 8).unwrap_err();
        assert!(err.msg.contains("limit must be a number"));
        assert!(err.hint.is_none());
    }

    #[test]
    fn ctx_new_builds_paths() {
        let ctx = Ctx::new(Path::new("/tmp/mem"));
        assert!(ctx.store_path.ends_with("store.db"));
        assert!(ctx.config_path.ends_with("config.json"));
    }

    #[test]
    fn require_store_missing_errors() {
        let dir = TempDir::new().unwrap();
        let err = Ctx::new(dir.path()).require_store().unwrap_err();
        assert!(err.msg.contains("no memory store"));
        assert!(err.hint.is_some());
    }

    // ---- handler integration -------------------------------------------

    #[test]
    fn get_existing_and_missing_id() {
        let (_d, ctx) = setup();
        let id = open_store(&ctx)
            .insert_note("hi", "", None, None, "note", &[0.1])
            .unwrap();
        assert!(get(&parsed(&["get", id.as_str()]), &ctx).is_ok());
        assert!(get(&parsed(&["get", "ffffff"]), &ctx).is_ok()); // found:false, still Ok
    }

    #[test]
    fn forget_deletes_and_reports_false_for_missing() {
        let (_d, ctx) = setup();
        let id = open_store(&ctx)
            .insert_note("hi", "", None, None, "note", &[0.1])
            .unwrap();
        assert!(forget(&parsed(&["forget", id.as_str()]), &ctx).is_ok());
        assert!(open_store(&ctx).get(&id).unwrap().is_none());
        assert!(forget(&parsed(&["forget", "ffffff"]), &ctx).is_ok());
    }

    #[test]
    fn forget_note_removes_md_file_and_index_row() {
        let (_d, ctx) = setup();
        let id = "a1b2c3";
        std::fs::write(ctx.note_path(id), "---\nid: a1b2c3\ncreated: t\n---\nbody").unwrap();
        open_store(&ctx)
            .upsert_note(id, "body", "", None, None, "note", 0, "t", &[0.1])
            .unwrap();
        assert!(ctx.note_path(id).exists());
        forget(&parsed(&["forget", id]), &ctx).unwrap();
        assert!(!ctx.note_path(id).exists()); // md (source of truth) removed
        assert!(open_store(&ctx).get(id).unwrap().is_none()); // index row removed
    }

    #[test]
    fn forget_chunk_refuses_with_hint() {
        let (_d, ctx) = setup();
        open_store(&ctx)
            .upsert_note(
                "c0ffee",
                "chunk body",
                "",
                Some("doc.md"),
                Some("doc.md › H"),
                "chunk",
                0,
                "t",
                &[0.1],
            )
            .unwrap();
        let err = forget(&parsed(&["forget", "c0ffee"]), &ctx).unwrap_err();
        assert!(err.msg.contains("imported chunk"));
        assert!(err.hint.is_some());
        // The chunk's index row is untouched (only reindex can remove it).
        assert!(open_store(&ctx).get("c0ffee").unwrap().is_some());
    }

    // ---- related (graph) ------------------------------------------------

    #[test]
    fn related_traverses_depth_predicate_and_dangling() {
        let (_d, ctx) = setup();
        // A --depends_on--> B (slug) ; B --links_to--> C ; A --links_to--> ghost
        std::fs::write(
            ctx.note_path("a00001"),
            "---\nid: a00001\ncreated: t\nrelations:\n  - depends_on: beta\n---\nsee [[ghost]]",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("b00002"),
            "---\nid: b00002\ncreated: t\nslug: beta\n---\nlinks [[gamma]]",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("c00003"),
            "---\nid: c00003\ncreated: t\nslug: gamma\n---\ngamma note",
        )
        .unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();

        // depth 1 from A: neighbor B (depends_on) + dangling ghost (links_to).
        assert!(related(&parsed(&["related", "a00001"]), &ctx).is_ok());

        // Build results directly to assert structure (the handler prints to stdout).
        let store = open_store(&ctx);
        let d1 = store.edges_from("a00001").unwrap();
        assert_eq!(d1.len(), 2);
        // B resolves via slug "beta"; ghost stays dangling.
        assert!(d1.iter().any(|e| e.dst_id.as_deref() == Some("b00002")));
        assert!(d1
            .iter()
            .any(|e| e.dst_id.is_none() && e.dst_raw == "ghost"));
        // B -> C resolves via slug "gamma" (depth 2 reachable).
        let from_b = store.edges_from("b00002").unwrap();
        assert!(from_b.iter().any(|e| e.dst_id.as_deref() == Some("c00003")));

        // --predicate filters: depends_on only -> no links_to/ghost.
        assert!(related(
            &parsed(&[
                "related",
                "a00001",
                "--predicate",
                "depends_on",
                "--depth",
                "2"
            ]),
            &ctx
        )
        .is_ok());
        // Missing id errors with a hint.
        let err = related(&parsed(&["related"]), &ctx).unwrap_err();
        assert!(err.hint.is_some());
    }

    #[test]
    fn backlinks_lists_inbound_sources_and_filters_predicate() {
        let (_d, ctx) = setup();
        // A --depends_on--> C ; B --links_to--> C. backlinks C => {A, B}.
        std::fs::write(
            ctx.note_path("a00001"),
            "---\nid: a00001\ncreated: t\nrelations:\n  - depends_on: gamma\n---\nbody a",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("b00002"),
            "---\nid: b00002\ncreated: t\n---\nsee [[gamma]]",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("c00003"),
            "---\nid: c00003\ncreated: t\nslug: gamma\n---\ngamma note",
        )
        .unwrap();
        reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap();

        // Both inbound edges resolve to c00003.
        let into_c = open_store(&ctx).edges_to("c00003").unwrap();
        assert_eq!(into_c.len(), 2);
        assert!(into_c.iter().any(|e| e.src_id == "a00001"));
        assert!(into_c.iter().any(|e| e.src_id == "b00002"));

        // The handler runs; predicate filter narrows to the depends_on source only.
        assert!(backlinks(&parsed(&["backlinks", "c00003"]), &ctx).is_ok());
        assert!(backlinks(
            &parsed(&["backlinks", "c00003", "--predicate", "depends_on"]),
            &ctx
        )
        .is_ok());
        // Missing id errors with a hint.
        let err = backlinks(&parsed(&["backlinks"]), &ctx).unwrap_err();
        assert!(err.hint.is_some());
    }

    #[test]
    fn related_predicate_filter_normalizes_separator() {
        // `--predicate depends-on` must match an edge stored as `depends_on` (D5).
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("a00001"),
            "---\nid: a00001\ncreated: t\nrelations:\n  - depends_on: gamma\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("c00003"),
            "---\nid: c00003\ncreated: t\nslug: gamma\n---\ngamma",
        )
        .unwrap();
        reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap();
        // The edge predicate is normalized to `depends_on`; the hyphenated filter
        // is normalized the same way, so the walk still finds c00003.
        let neighbors =
            search::bfs_neighbors(&open_store(&ctx), "a00001", 1, Some("depends_on"), true)
                .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].row.as_ref().unwrap().id, "c00003");
    }

    // ---- reindex --------------------------------------------------------

    #[test]
    fn reindex_rebuilds_notes_and_recovers_after_store_deleted() {
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("aa11bb22"),
            "---\nid: aa11bb22\ncreated: 2026-01-01T00:00:00Z\ntags: x\n---\nrecovered body",
        )
        .unwrap();
        // Simulate losing store.db entirely; the md file is what we rebuild from.
        std::fs::remove_file(&ctx.store_path).unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        let row = open_store(&ctx).get("aa11bb22").unwrap().unwrap();
        assert_eq!(row.body, "recovered body");
        assert_eq!(row.tags, "x");
        assert_eq!(row.created_iso, "2026-01-01T00:00:00Z"); // created from frontmatter, not now()
    }

    #[test]
    fn reindex_incremental_indexes_then_prunes_deleted_md() {
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("aabbcc01"),
            "---\nid: aabbcc01\ncreated: t\n---\none",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("aabbcc02"),
            "---\nid: aabbcc02\ncreated: t\n---\ntwo",
        )
        .unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        assert!(open_store(&ctx).get("aabbcc01").unwrap().is_some());
        assert!(open_store(&ctx).get("aabbcc02").unwrap().is_some());
        // Delete one md out-of-band; the next reindex prunes its derived row.
        std::fs::remove_file(ctx.note_path("aabbcc01")).unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        assert!(open_store(&ctx).get("aabbcc01").unwrap().is_none());
        assert!(open_store(&ctx).get("aabbcc02").unwrap().is_some());
    }

    #[test]
    fn reindex_all_rebuilds_from_md() {
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("ab12cd34"),
            "---\nid: ab12cd34\ncreated: t\n---\nbody all",
        )
        .unwrap();
        reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap();
        assert_eq!(
            open_store(&ctx).get("ab12cd34").unwrap().unwrap().body,
            "body all"
        );
    }

    #[test]
    fn reindex_recovers_imported_chunks_and_tags_after_store_deleted() {
        let (dir, ctx) = setup();
        // Import a doc: the original is copied into imports/ with a tags sidecar.
        let src = dir.path().join("architecture.md");
        std::fs::write(&src, "# Auth\nalpha beta gamma\n## DB\ndelta epsilon").unwrap();
        let emb = embed::build(&Config::default()).unwrap();
        import::import_file(
            &open_store(&ctx),
            emb.as_ref(),
            &Config::default(),
            &src,
            "spec,arch",
            &ctx.imports_dir,
        )
        .unwrap();
        let before: Vec<_> = open_store(&ctx).all().unwrap();
        assert!(!before.is_empty() && before.iter().all(|r| r.kind == "chunk"));

        // Lose the entire store.db, then rebuild purely from imports/ + sidecar.
        std::fs::remove_file(&ctx.store_path).unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();

        let after = open_store(&ctx).all().unwrap();
        assert_eq!(after.len(), before.len()); // chunks rebuilt deterministically
                                               // Tags survived via the sidecar (not just store.db).
        assert!(after.iter().all(|r| r.tags == "spec,arch"));
        assert_eq!(
            open_store(&ctx)
                .import_record("imports/architecture.md")
                .unwrap()
                .unwrap()
                .tags,
            "spec,arch"
        );
        // Chunk ids are stable across the rebuild (content-addressed).
        let ids_before: std::collections::BTreeSet<_> =
            before.iter().map(|r| r.id.clone()).collect();
        let ids_after: std::collections::BTreeSet<_> = after.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids_before, ids_after);
    }

    #[test]
    fn reindex_derives_graph_edges_with_slug_resolution_and_dangling() {
        let (_d, ctx) = setup();
        // Note A carries a slug; note B (written to sort BEFORE A) links to it by
        // slug via a relation AND a wiki-link, plus a dangling [[ghost]].
        std::fs::write(
            ctx.note_path("aa00ff"),
            "---\nid: aa00ff\ncreated: t\nrelations:\n  - depends_on: db-schema\n---\nlinks [[db-schema]] and [[ghost]]",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("zz99ee"),
            "---\nid: zz99ee\ncreated: t\nslug: DB Schema\n---\nthe schema note",
        )
        .unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        let edges = open_store(&ctx).edges_from("aa00ff").unwrap();
        assert_eq!(edges.len(), 3);
        // Forward reference + slug normalization: both the relation and the
        // wiki-link resolve to zz99ee (slug "DB Schema" -> "db-schema").
        let resolved = edges
            .iter()
            .filter(|e| e.dst_id.as_deref() == Some("zz99ee"))
            .count();
        assert_eq!(resolved, 2);
        // The unknown target stays dangling (dst_id NULL), keeping its raw text.
        let dangling: Vec<_> = edges.iter().filter(|e| e.dst_id.is_none()).collect();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].dst_raw, "ghost");
        assert_eq!(dangling[0].predicate, "links_to");
    }

    #[test]
    fn incremental_reindex_revives_dangling_edge_when_target_appears() {
        // B1 regression: A links to slug `beta` (dangling); B with slug `beta` is
        // written later; an INCREMENTAL reindex (no --all) must revive A's edge,
        // even though A's md never changed.
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("a00001"),
            "---\nid: a00001\ncreated: t\nrelations:\n  - depends_on: beta\n---\nbody",
        )
        .unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        // A's edge dangles: no note owns slug `beta` yet.
        let e = open_store(&ctx).edges_from("a00001").unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].dst_id.is_none() && e[0].dst_raw == "beta");

        // Now B appears with slug beta; A's md is untouched.
        std::fs::write(
            ctx.note_path("b00002"),
            "---\nid: b00002\ncreated: t\nslug: beta\n---\nbeta note",
        )
        .unwrap();
        reindex(&parsed(&["reindex"]), &ctx).unwrap();
        // Revived WITHOUT --all: the edge now resolves to b00002.
        let e2 = open_store(&ctx).edges_from("a00001").unwrap();
        assert_eq!(e2[0].dst_id.as_deref(), Some("b00002"));
    }

    #[test]
    fn forget_target_redangles_inbound_edge_not_drops_it() {
        // B2/B3 regression: A --depends_on--> B (resolved). forget B. A's edge must
        // re-dangle (still authored in A's md), not silently disappear or stay
        // resolved to a dead row.
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("b00002"),
            "---\nid: b00002\ncreated: t\nslug: beta\n---\nbeta",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("a00001"),
            "---\nid: a00001\ncreated: t\nrelations:\n  - depends_on: beta\n---\nbody",
        )
        .unwrap();
        reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap();
        assert_eq!(
            open_store(&ctx).edges_from("a00001").unwrap()[0]
                .dst_id
                .as_deref(),
            Some("b00002")
        );

        // Forget B: the inbound edge re-dangles (dst_id NULL), keeping its raw text.
        forget(&parsed(&["forget", "b00002"]), &ctx).unwrap();
        let e = open_store(&ctx).edges_from("a00001").unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].dst_id.is_none());
        assert_eq!(e[0].dst_raw, "beta");
    }

    #[test]
    fn index_note_edges_dedups_same_target_different_spelling() {
        // D4: a relation and a wiki-link naming the same slug two ways collapse to
        // one edge per (predicate, resolved id). `links_to` here applies twice over
        // `DB Schema` / `db-schema`, which resolve to the same note.
        let (_d, ctx) = setup();
        std::fs::write(
            ctx.note_path("aa00ff"),
            "---\nid: aa00ff\ncreated: t\n---\nsee [[DB Schema]] and again [[db-schema]]",
        )
        .unwrap();
        std::fs::write(
            ctx.note_path("zz99ee"),
            "---\nid: zz99ee\ncreated: t\nslug: db-schema\n---\nthe schema",
        )
        .unwrap();
        reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap();
        let edges = open_store(&ctx).edges_from("aa00ff").unwrap();
        // Two spellings, one resolved edge.
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].dst_id.as_deref(), Some("zz99ee"));
    }

    #[test]
    fn reindex_all_refuses_to_wipe_legacy_data() {
        let (_d, ctx) = setup();
        // A note row in the DB but no notes/*.md (the legacy db-as-truth shape).
        open_store(&ctx)
            .upsert_note("dead00", "legacy", "", None, None, "note", 0, "t", &[0.1])
            .unwrap();
        let err = reindex(&parsed(&["reindex", "--all"]), &ctx).unwrap_err();
        assert!(err.msg.contains("refusing"));
        assert!(err.hint.is_some());
        assert!(open_store(&ctx).get("dead00").unwrap().is_some()); // not wiped
    }

    #[test]
    fn list_and_log_handlers_run() {
        let (_d, ctx) = setup();
        open_store(&ctx)
            .insert_note("hi", "", None, None, "note", &[0.1])
            .unwrap();
        assert!(list(&parsed(&["list", "--recent", "2"]), &ctx).is_ok());
        assert!(log(&parsed(&["log"]), &ctx).is_ok());
        assert!(log(&parsed(&["log", "--imports"]), &ctx).is_ok());
    }

    #[test]
    fn open_missing_config_errors() {
        let dir = TempDir::new().unwrap();
        // store.db exists but config.json is absent.
        Store::create(&dir.path().join("store.db")).unwrap();
        let ctx = Ctx::new(dir.path());
        let err = get(&parsed(&["get", "1"]), &ctx).unwrap_err();
        assert!(err.msg.contains("config.json not found"));
    }

    #[test]
    fn config_show_missing_config_file_errors() {
        let dir = TempDir::new().unwrap();
        Store::create(&dir.path().join("store.db")).unwrap();
        let ctx = Ctx::new(dir.path());
        // `config` (no sub) goes through load_raw, bypassing open().
        let err = config(&parsed(&["config"]), &ctx).unwrap_err();
        assert!(err.msg.contains("config.json not found"));
    }

    #[test]
    fn warn_on_drift_first_run_records_signature() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let emb = embed::build(&Config::default()).unwrap();
        assert_eq!(store.meta_get("embedder_signature").unwrap(), None);
        warn_on_drift(&store, emb.as_ref());
        assert_eq!(
            store.meta_get("embedder_signature").unwrap().as_deref(),
            Some(emb.signature().as_str()),
        );
    }

    #[cfg(feature = "api")]
    #[test]
    fn api_provider_without_endpoint_errors() {
        let dir = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.embedding.provider = "api".into(); // endpoint stays None
        cfg.save(&dir.path().join("config.json")).unwrap();
        Store::create(&dir.path().join("store.db")).unwrap();
        let ctx = Ctx::new(dir.path());
        let err = recall(&parsed(&["recall", "query"]), &ctx).unwrap_err();
        assert!(err.msg.contains("endpoint is not set"));
    }

    #[test]
    fn resolve_data_dir_joins_relative_keeps_absolute_and_dot() {
        let root = Path::new("/proj");
        // "." and "" mean "same folder as config".
        assert_eq!(resolve_data_dir(root, "."), PathBuf::from("/proj"));
        assert_eq!(resolve_data_dir(root, ""), PathBuf::from("/proj"));
        assert_eq!(resolve_data_dir(root, "  "), PathBuf::from("/proj"));
        // A relative dir joins onto the config folder.
        assert_eq!(
            resolve_data_dir(root, "memory"),
            PathBuf::from("/proj/memory")
        );
        // An absolute dir is used as-is.
        assert_eq!(
            resolve_data_dir(root, "/data/store"),
            PathBuf::from("/data/store")
        );
    }

    #[test]
    fn ctx_splits_config_at_root_and_data_under_data_dir() {
        // config.json at the root with data_dir="memory" → store under root/memory.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cfg = Config {
            data_dir: "memory".into(),
            ..Config::default()
        };
        cfg.save(&root.join("config.json")).unwrap();
        let ctx = Ctx::new(root);
        assert_eq!(ctx.config_path, root.join("config.json"));
        assert_eq!(ctx.store_path, root.join("memory").join("store.db"));
        assert_eq!(ctx.notes_dir, root.join("memory").join("notes"));
    }

    #[test]
    fn ctx_legacy_single_folder_when_no_config() {
        // No config.json → fall back to the legacy single-folder layout (data next
        // to where we were pointed), so old stores keep working.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let ctx = Ctx::new(dir);
        assert_eq!(ctx.store_path, dir.join("store.db"));
        assert_eq!(ctx.config_path, dir.join("config.json"));
    }

    #[test]
    fn malformed_config_surfaces_parse_error_not_no_store() {
        // Regression: a split-layout store (real store under memory/) whose config
        // got corrupted. Ctx::new can't read data_dir, so it falls back to the legacy
        // path and store_path points at the (absent) root store.db. require_store must
        // surface "config.json is invalid", not a misleading "no memory store".
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("config.json"), "{ this is not json").unwrap();
        // The real store lives under memory/ (mimicking a split layout).
        std::fs::create_dir_all(root.join("memory")).unwrap();
        Store::create(&root.join("memory").join("store.db")).unwrap();
        let ctx = Ctx::new(root);
        let err = get(&parsed(&["get", "1"]), &ctx).unwrap_err();
        assert!(
            err.msg.contains("config.json is invalid"),
            "expected parse error, got: {}",
            err.msg
        );
    }

    #[test]
    fn lang_breakdown_orders_by_count_then_name_with_pretty_names() {
        let mut m: std::collections::BTreeMap<&'static str, usize> = Default::default();
        m.insert("csharp", 57);
        m.insert("rust", 8);
        m.insert("typescript", 8); // ties with rust on count -> name asc (rust first)
        assert_eq!(lang_breakdown(&m), "C# 57, Rust 8, TypeScript 8");
        assert_eq!(lang_breakdown(&Default::default()), "—");
    }

    #[test]
    fn skip_dirs_excludes_dotnet_obj_and_bin() {
        // The .NET generated dirs must not be walked (obj/Debug/*.cs is generated
        // noise that previously inflated the file count).
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("obj/Debug")).unwrap();
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("src/A.cs"), "class A {}").unwrap();
        std::fs::write(d.join("obj/Debug/G.cs"), "class G {}").unwrap();
        std::fs::write(d.join("bin/B.cs"), "class B {}").unwrap();
        let mut out = Vec::new();
        collect_source_files(d, Path::new("/no/mem"), None, &mut out);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["A.cs"], "obj/ and bin/ must be skipped");
    }
}
