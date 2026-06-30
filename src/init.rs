//! `init`: scaffold a self-contained, portable memory folder (desc.md §5). If the
//! target directory already has `.md` files in it, we offer to import them all in
//! one pass (y/n) and then optionally delete the originals.

use crate::cli::Parsed;
use crate::commands;
use crate::config::Config;
use crate::embed;
use crate::help;
use crate::import;
use crate::output::print_line;
use crate::store::Store;
use crate::util::{AppError, Result};
use serde_json::json;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn run(p: &Parsed) -> Result<()> {
    // No target given → default to the current working directory. Dropping the
    // binary at a project root and running a bare `cm init` should "just work":
    // it lands a `memory/` folder in the project root regardless of where the
    // binary itself sits. An explicit path still overrides (e.g. `cm init ./docs`).
    let cwd = std::env::current_dir()?;
    let target = resolve_target(p, &cwd);
    let target = target.as_path();

    // The layout splits in two:
    //   * ROOT (`target`, where the user dropped the binary): cm(.exe) + config.json
    //     — small, committable, easy to find.
    //   * DATA (`target/<name>`, default `memory/`): store.db + notes/ + imports/ +
    //     models/ — the derived index and the md source of truth.
    // config.json's `data_dir` records the link (`<name>`), so the binary at the
    // root finds its data without a --dir flag.
    let name = p.value("name").unwrap_or("memory");
    let data_folder = target.join(name);
    let config_path = target.join("config.json");

    // `init` is idempotent: it brings the project up to a fully-initialized state and
    // touches only what is missing or stale (desc.md §5: never clobber an existing
    // store). Probe what's already on disk so the rest of `run` can re-create just the
    // gaps. A FULLY-complete layout short-circuits below; a partial one is repaired.
    let pre = LayoutState::probe(target, &data_folder, &config_path);

    // The config is the one place where re-running could destroy user edits (secrets'
    // env-var names, tuned weights). If a config.json already exists, ADOPT it verbatim
    // — never overwrite — and warn that the embedding flags don't retro-apply. Only a
    // fresh init builds a config from defaults + flags.
    let cfg = if pre.config {
        if ["model", "provider", "endpoint", "dimension"]
            .iter()
            .any(|f| p.value(f).is_some())
        {
            eprintln!(
                "      note: config.json already exists — keeping it; --model/--provider/--endpoint/--dimension ignored"
            );
        }
        // A broken config.json is fatal here: we won't silently replace what might be a
        // user's tuned file, and proceeding with defaults would mask the corruption.
        Config::load(&config_path)?
    } else {
        // Start from the defaults, then layer on any flag overrides. `data_dir = <name>`
        // is what points the root config at the data subfolder.
        let mut cfg = Config::new_named(name);
        cfg.data_dir = name.to_string();
        if let Some(m) = p.value("model") {
            cfg.embedding.model = m.to_string();
        }
        if let Some(pr) = p.value("provider") {
            cfg.embedding.provider = pr.to_string();
        }
        if let Some(ep) = p.value("endpoint") {
            cfg.embedding.endpoint = Some(ep.to_string());
        }
        if let Some(dim) = p.value("dimension") {
            cfg.embedding.dimension = dim
                .parse()
                .map_err(|_| AppError::new(format!("--dimension must be a number, got '{dim}'")))?;
        }
        cfg
    };

    // Fully-initialized already → nothing to do. This is the new, smarter guard: not
    // "the data folder exists" (which an empty/partial folder also satisfies), but
    // "every artifact init produces is present and current". A partial layout falls
    // through to the repair path below. The exe-display is needed to judge whether the
    // wired pointer is current, so compute it the same way the wiring step will.
    let exe_name = if cfg!(windows) { "cm.exe" } else { "cm" };
    let exe_dest = target.join(exe_name);
    let exe_display = exe_command_display(target, &exe_dest);
    // An explicit `--docs <paths>` is a request to import THOSE docs now, even into an
    // otherwise-complete store. Don't short-circuit then — fall through so the import
    // step runs (it's idempotent enough; the user named the paths deliberately). The
    // empty-value filter mirrors the import step, so `--docs ""`/`--docs ,` (i.e. "not
    // really given") still allow the no-op short-circuit.
    let explicit_docs = p.value("docs").is_some_and(docs_arg_has_entry);
    if !explicit_docs && pre.is_complete(target, &exe_display) {
        println!(
            "{}",
            json!({
                "status": "already_exists",
                "path": data_folder.to_string_lossy(),
                "note": "Fully initialized — nothing to do. Pass --docs <paths> to import more, delete the folder to start over, or `cm reindex` to rebuild the index.",
            })
        );
        return Ok(());
    }

    // Narrate the run as numbered steps on stderr (data/JSONL stays on stdout). The
    // user dropped a binary and ran `cm init`; show them what each phase did.
    let do_code = !p.has("no-code");
    let total_steps = if do_code { 4 } else { 3 };
    let store_path = data_folder.join("store.db");
    // `repairing` distinguishes a fresh init from filling gaps in a partial layout, so
    // the narration and final status read truthfully ("repaired" vs "created").
    let repairing = pre.any_present();
    eprintln!(
        "cm init → {} (data in {}/){}",
        target.to_string_lossy(),
        name,
        if repairing {
            " — completing existing layout"
        } else {
            ""
        }
    );

    // [1/N] Scaffold — create only what's MISSING (idempotent). config.json goes at the
    // ROOT (next to the binary); the data dirs go under `data_folder`. notes/ and
    // imports/ are the source of truth; store.db is the derived, rebuildable index.
    // Each piece is guarded by `pre` so a re-run never overwrites a good store.db or a
    // user-edited config.json — it only fills the holes.
    step(1, total_steps, "Creating memory folder");
    std::fs::create_dir_all(data_folder.join("notes"))?;
    std::fs::create_dir_all(data_folder.join("imports"))?;
    std::fs::create_dir_all(data_folder.join("models"))?;
    if !pre.config {
        cfg.save(&config_path)?;
    }

    // `Store::create` is itself idempotent (CREATE TABLE IF NOT EXISTS), but skip it
    // when the store already exists so we never touch a good index's mtime.
    if !pre.store {
        Store::create(&store_path)?;
    }

    // REPAIR a lost index: if we just (re)created store.db but the md TRUTH is still on
    // disk (notes/ or imports/ non-empty), rebuild the document index from it. Without
    // this, a repair after `store.db` was deleted would leave `recall` empty until the
    // user manually ran `cm reindex` — the store is derived, so init should restore it
    // automatically. We only do this on a genuine repair (store was missing AND truth
    // exists); a fresh init has nothing to rebuild, and a present store is left alone.
    // Best-effort: a failure here never aborts the scaffold (the store is still valid,
    // just empty, and `cm reindex` remains the fallback). Code indexing is handled
    // separately in step [4/N]; this is documents only.
    if !pre.store && has_md_truth(&data_folder) {
        reindex_documents(target, &data_folder);
    }

    // A .gitignore INSIDE the data folder: commit the TRUTH (notes/ + imports/) but
    // ignore the derived index and re-downloadable weights. Write it only if absent so
    // a user's tweaks to it survive a repair run.
    if !pre.data_gitignore {
        let gitignore = "# Derived index — rebuild from md with `cm reindex`.\n\
                         store.db\nstore.db-wal\nstore.db-shm\n\
                         # Embedding weights (re-downloadable).\nmodels/\n";
        let _ = std::fs::write(data_folder.join(".gitignore"), gitignore);
    }

    // Make sure a copy of the binary sits at the ROOT (so `cm` is findable next to
    // its config). Usually it's already there — the user dropped it — but when init
    // runs via `cargo run`/elsewhere, copy it in. Skip if the running exe already IS
    // the root copy (compare canonicalized paths so a relative target like `.` still
    // matches the absolute current_exe()). `exe_dest`/`exe_display` were computed up
    // top (the completeness check needed them); reuse them here.
    let self_exe = std::env::current_exe()?;
    if !same_file(&self_exe, &exe_dest) {
        if let Err(e) = std::fs::copy(&self_exe, &exe_dest) {
            // Not fatal: the store still works, we just couldn't place the binary.
            eprintln!("warning: could not copy the binary to the project root: {e}");
        }
    }

    // The binary now lives at the project ROOT, which has no .gitignore coverage of
    // our doing. It's large (tens of MB) and rebuildable, so make sure git won't
    // sweep it into a commit: ensure the root .gitignore ignores cm/cm.exe. We append
    // (idempotently) rather than overwrite, so a project's existing .gitignore is
    // preserved. config.json beside it is small and meant to be committed — untouched.
    // Capture what we did to it for the rollback manifest (restore/delete/leave).
    let gi_state = ensure_binary_ignored(target);

    // Record the embedder signature (so we can spot provider/dimension drift
    // later) and log the init. If the provider can't be built yet (say, api with
    // no endpoint) we just skip the signature; the store is still perfectly valid.
    if let Ok(store) = Store::open(&store_path) {
        if let Ok(emb) = embed::build(&cfg) {
            let _ = store.meta_set("embedder_signature", &emb.signature());
        }
        let _ = store.log_op("init", Some(&data_folder.to_string_lossy()));
    }
    eprintln!(
        "      root: config.json + cm{}; data: {}/ (store.db, notes/, imports/)",
        if cfg!(windows) { ".exe" } else { "" },
        name,
    );

    // [2/N] Pull in the project's docs. With --docs <paths> the user names exactly
    // which dirs/files to import (no prompt). Otherwise we auto-scan: gather the .md
    // tree (skipping the agent's root entry-point files like README/CLAUDE — those
    // are wired, not absorbed), show the doc folders we found, and ask one y/N.
    step(2, total_steps, "Collecting documentation (.md)");
    let doc_stats = import_existing_md(target, &data_folder, &cfg, &store_path, p.value("docs"));

    // [3/N] Wire the agent's existing instruction files (CLAUDE.md, AGENTS.md, …) to
    // the store: append a pointer telling the model to reach for these docs via `cm
    // recall` instead of reading them whole. Idempotent — re-running won't duplicate.
    step(
        3,
        total_steps,
        "Wiring agent instruction files (CLAUDE.md etc.)",
    );
    let wire = wire_entry_points(target, &exe_display);
    let mut created_agents_md = false;
    if !wire.wired.is_empty() {
        eprintln!("      wired: {}", wire.wired.join(", "));
    } else if wire.any_existed {
        // Files exist and already carry a current block — nothing to do.
        eprintln!("      pointer already present");
    } else {
        // No instruction file at all → create AGENTS.md so a model has an entry point.
        match create_agents_md(target, &exe_display) {
            Some(name) => {
                // Record the creation so deinit removes it authoritatively (even if
                // the user later edits the file past the header heuristic).
                created_agents_md = true;
                eprintln!("      no instruction files found — created {name}");
            }
            None => eprintln!("      no instruction files found at root"),
        }
    }
    // Drop the standalone full guide alongside the (short) wired pointers, so any
    // agent — whatever instruction file it reads — has a complete, self-sufficient
    // manual it can open as a file, with no `cm help` round-trip. Tracked for rollback.
    let created_cm_guide = write_cm_guide(target, &exe_display);

    // Sanity-check the wiring we just wrote: the pointers tell the agent to run the
    // binary from the project root, so make sure it's actually there. If the self-copy
    // above failed, the instructions would point at a binary that isn't present — say
    // so loudly rather than leave a broken pointer. (This is exactly what `cm doctor`'s
    // `agent_wiring` check re-verifies later.)
    if !exe_dest.is_file() {
        eprintln!(
            "      warning: the instruction files point at `{exe_display}`, but no cm binary \
             is there — place cm at {} so the pointers resolve.",
            target.to_string_lossy()
        );
    }

    // [4/N] Index the project's source tree into the code graph (default on), so a
    // bare `cm init` gives an immediately-queryable map. `--no-code` skips it.
    // Best-effort: the scaffold already succeeded, so a missing `code` feature or a
    // parse hiccup must NOT fail init.
    let mapped = if do_code {
        step(4, total_steps, "Indexing source code");
        map_source_tree(target, &data_folder, &store_path)
    } else {
        None
    };

    let mut out = json!({
        // "repaired" when we filled gaps in a partial layout, "created" for a fresh
        // one — so a caller can tell a first init from a top-up.
        "status": if repairing { "repaired" } else { "created" },
        "created": data_folder.to_string_lossy(),
        "exe": exe_dest.to_string_lossy(),
        "store": store_path.to_string_lossy(),
        "config": config_path.to_string_lossy(),
        "notes": data_folder.join("notes").to_string_lossy(),
        "imports": data_folder.join("imports").to_string_lossy(),
        "provider": cfg.embedding.provider,
        "offline": cfg.embedding.provider == "local",
    });
    if let Some(m) = &mapped {
        out["code"] = json!({ "files": m.files, "symbols": m.symbols, "edges": m.edges });
    }
    println!("{out}");

    // Final human summary on stderr: what landed where, in one glance.
    print_summary(&data_folder, &doc_stats, mapped.as_ref());

    // Write the rollback manifest LAST and best-effort: a failure here must never
    // block init's success output. It records exactly what init changed (which docs
    // came from where, how the root .gitignore was touched, whether AGENTS.md was
    // created) so `deinit` can undo init precisely. It's internal data, so it goes to
    // disk only — never to stdout.
    //
    // On a REPAIR (a manifest already exists from the first init) we MERGE rather than
    // overwrite, so the original rollback truth survives. The pre-init `.gitignore`
    // snapshot and the "init created this file" flags are authoritative from the FIRST
    // run — a repair re-deriving them would be wrong (e.g. it now sees the .gitignore
    // it itself created and would conclude "leave it"). We keep those, OR-in anything
    // this run additionally created, and extend the doc records with newly-imported docs.
    let manifest = match read_manifest_opt(&data_folder) {
        Some(mut prev) => {
            prev.version = 1;
            prev.data_dir = name.to_string();
            prev.created_agents_md |= created_agents_md;
            prev.created_cm_guide |= created_cm_guide;
            // Only adopt this run's gitignore state if the first run recorded none
            // (e.g. an older manifest predating the field, or a repair after a manual
            // gitignore deletion that this run re-created).
            if !prev.root_gitignore.modified && gi_state.modified {
                prev.root_gitignore = gi_state;
            }
            prev.docs.extend(doc_stats.records);
            prev
        }
        None => InitManifest {
            version: 1,
            data_dir: name.to_string(),
            root_gitignore: gi_state,
            created_agents_md,
            created_cm_guide,
            docs: doc_stats.records,
        },
    };
    write_manifest(&data_folder, &manifest);

    // Print the ready-to-paste pointer (desc.md §8).
    eprintln!("\n— Pointer for system prompt / CLAUDE.md —\n");
    eprintln!("{}", help::pointer(&exe_display));

    Ok(())
}

/// Serialize the rollback manifest into `<data_folder>/.init-manifest.json`.
/// Best-effort: a serialize/write error is a non-fatal stderr warning — init has
/// already succeeded, and `deinit` falls back to the `imports/` sidecars when the
/// manifest is missing.
fn write_manifest(data_folder: &Path, manifest: &InitManifest) {
    let path = data_folder.join(MANIFEST_NAME);
    match serde_json::to_string_pretty(manifest) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("      rollback manifest not written ({e}) — deinit will be approximate");
            }
        }
        Err(e) => eprintln!("      rollback manifest could not be serialized: {e}"),
    }
}

/// Read the rollback manifest from a data folder if it exists and parses, else `None`.
/// Used by a repair run to MERGE into the first init's manifest instead of clobbering
/// its rollback truth (see `run`). A missing or garbled manifest is treated as "none" —
/// the repair then writes a fresh one, the same as a first init would.
fn read_manifest_opt(data_folder: &Path) -> Option<InitManifest> {
    let text = std::fs::read_to_string(data_folder.join(MANIFEST_NAME)).ok()?;
    serde_json::from_str(&text).ok()
}

/// A snapshot of which init artifacts already exist on disk, taken BEFORE init touches
/// anything. It drives the idempotent repair path: each scaffold piece is created only
/// when its flag is false, and `is_complete` decides whether there's anything to do at
/// all. Probing is pure filesystem reads — no mutation — so it's safe to call up front.
struct LayoutState {
    /// `config.json` at the project root.
    config: bool,
    /// `store.db` in the data folder.
    store: bool,
    /// All three data subdirs (`notes/`, `imports/`, `models/`) present.
    dirs: bool,
    /// The data folder's own `.gitignore` (ignores the derived index + weights).
    data_gitignore: bool,
    /// The cm binary sits at the project root.
    exe: bool,
    /// The root `.gitignore` already ignores the binary (our marker, or the user's
    /// own `cm.exe` rule) — i.e. `ensure_binary_ignored` would be a no-op.
    root_ignored: bool,
    /// `CM_GUIDE.md` at the root.
    guide: bool,
}

impl LayoutState {
    /// Read the current on-disk state of every init artifact. Pure (filesystem reads
    /// only); call it once before the scaffold so the repair path knows the gaps.
    fn probe(target: &Path, data_folder: &Path, config_path: &Path) -> LayoutState {
        let exe_name = if cfg!(windows) { "cm.exe" } else { "cm" };
        let root_gi = std::fs::read_to_string(target.join(".gitignore")).unwrap_or_default();
        LayoutState {
            config: config_path.is_file(),
            store: data_folder.join("store.db").is_file(),
            dirs: data_folder.join("notes").is_dir()
                && data_folder.join("imports").is_dir()
                && data_folder.join("models").is_dir(),
            data_gitignore: data_folder.join(".gitignore").is_file(),
            exe: target.join(exe_name).is_file(),
            root_ignored: root_gi.contains(GITIGNORE_MARKER)
                || root_gi.lines().any(|l| l.trim() == "cm.exe"),
            guide: target.join(CM_GUIDE_NAME).is_file(),
        }
    }

    /// True if a PRIOR init left MEMORY DATA behind — i.e. this run is completing a
    /// partial layout, not a fresh one. Used only to phrase the narration ("completing
    /// existing layout") and the final status ("repaired" vs "created").
    ///
    /// Deliberately ignores `exe`, `root_ignored`, AND `config`. The usual fresh flow
    /// is "drop the binary, then `cm init`", so the binary (and a user's pre-existing
    /// root `.gitignore` rule) being present isn't a prior init. And `deinit`
    /// intentionally LEAVES `config.json` at the root, so a lone config is the
    /// after-state of a full uninstall — re-`init`-ing onto it is a FRESH setup
    /// ("created"), not a repair (the config is still adopted, never overwritten; it
    /// just doesn't, by itself, mark the run as a repair). Only the data-folder
    /// artifacts init authors — store / dirs / data-.gitignore / guide — count as
    /// "memory data already here".
    fn any_present(&self) -> bool {
        self.store || self.dirs || self.data_gitignore || self.guide
    }

    /// True when there is nothing left for init to do: the full scaffold is present,
    /// the binary is placed and git-ignored, the guide is written, and at least one
    /// agent instruction file carries a CURRENT pointer block. We check the wired
    /// pointer last (it's the only part needing a file read against `exe_display`) so a
    /// stale pointer (re-init under a new path) correctly reports "incomplete" and gets
    /// refreshed. When this is true, `run` prints `already_exists` and stops.
    fn is_complete(&self, target: &Path, exe_display: &str) -> bool {
        self.config
            && self.store
            && self.dirs
            && self.data_gitignore
            && self.exe
            && self.root_ignored
            && self.guide
            && instruction_pointer_current(target, exe_display)
    }
}

/// True if some agent instruction file under `target` already carries the CURRENT cm
/// pointer block (byte-identical to what wiring would write for `exe_display`). This is
/// the last gate of `LayoutState::is_complete`: it ensures a re-init under a new exe
/// path (stale pointer) is treated as incomplete and re-wired, not skipped. Mirrors the
/// matching `wire_entry_points` does, but read-only.
fn instruction_pointer_current(target: &Path, exe_display: &str) -> bool {
    let block = entry_point_block(exe_display);
    ENTRY_POINT_NAMES.iter().any(|rel| {
        let path = rel
            .split('/')
            .fold(target.to_path_buf(), |p, seg| p.join(seg));
        std::fs::read_to_string(&path)
            .map(|text| text.contains(&block))
            .unwrap_or(false)
    })
}

/// Read-only wiring/scaffold health for `cm doctor`. Reports the agent-facing wiring
/// (does the cm binary sit where the instruction files point, and do those files carry
/// a CURRENT pointer block) plus the cosmetic scaffold extras (gitignore rules, guide).
/// It reuses the SAME `LayoutState` probe `init` heals from, so `doctor` reports exactly
/// what `init` would complete — without duplicating the check or running init's repair.
/// `data_folder` is the resolved data dir (config's `data_dir`).
pub(crate) struct LayoutStatus {
    /// How the wired pointers spell the binary as a command (e.g. `.\cm.exe`).
    pub exe_display: String,
    /// The cm binary sits at the project root, where the pointers tell the agent to
    /// run it. The crux of the "instructions point at a binary that isn't there" check.
    pub binary_present: bool,
    /// The root `.gitignore` ignores the (large, rebuildable) binary.
    pub root_ignored: bool,
    /// The data folder's own `.gitignore` is present.
    pub data_gitignore: bool,
    /// `CM_GUIDE.md` is at the root.
    pub guide_present: bool,
    /// Entry-point instruction files that exist at all (CLAUDE.md/AGENTS.md/…).
    pub instruction_files: Vec<&'static str>,
    /// …of those, the ones carrying a CURRENT cm pointer block (byte-identical to what
    /// `init` would wire for `exe_display`). A present file NOT in here is unwired/stale.
    pub wired_files: Vec<&'static str>,
}

/// Probe the project layout rooted at `root` (config + binary) with data under
/// `data_folder`, the read-only mirror of `init`'s wiring decision.
pub(crate) fn layout_status(root: &Path, data_folder: &Path) -> LayoutStatus {
    let config_path = root.join("config.json");
    let pre = LayoutState::probe(root, data_folder, &config_path);
    let exe_name = if cfg!(windows) { "cm.exe" } else { "cm" };
    let exe_display = exe_command_display(root, &root.join(exe_name));
    let block = entry_point_block(&exe_display);
    let mut instruction_files = Vec::new();
    let mut wired_files = Vec::new();
    for rel in ENTRY_POINT_NAMES {
        let path = rel
            .split('/')
            .fold(root.to_path_buf(), |p, seg| p.join(seg));
        if !path.is_file() {
            continue;
        }
        instruction_files.push(*rel);
        if std::fs::read_to_string(&path)
            .map(|t| t.contains(&block))
            .unwrap_or(false)
        {
            wired_files.push(*rel);
        }
    }
    LayoutStatus {
        exe_display,
        binary_present: pre.exe,
        root_ignored: pre.root_ignored,
        data_gitignore: pre.data_gitignore,
        guide_present: pre.guide,
        instruction_files,
        wired_files,
    }
}

/// Emit one numbered step header to stderr: `[2/4] Collecting documentation`.
fn step(n: usize, total: usize, label: &str) {
    eprintln!("\n[{n}/{total}] {label}…");
}

/// True if `a` and `b` name the same file on disk. We canonicalize both (resolving
/// `.`/relative components and symlinks) so a relative `exe_dest` like `./cm.exe`
/// matches the absolute path from `current_exe()`. If a path can't be canonicalized
/// (e.g. `exe_dest` doesn't exist yet — the common case on a fresh init), it clearly
/// isn't the running exe, so we return false and let the copy proceed.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Marker line so `ensure_binary_ignored` can tell whether it already added its
/// entries to a .gitignore (idempotent across re-inits). `pub(crate)` so `deinit`'s
/// manifest-less fallback can locate and strip exactly this block.
pub(crate) const GITIGNORE_MARKER: &str = "# cm binary (large, rebuildable) — added by `cm init`";

/// Filename of the rollback manifest `init` drops in the DATA folder. It is the
/// snapshot `deinit` reads to undo init *exactly*: which docs were imported (and
/// from where), whether the root `.gitignore` was changed (and its pre-init bytes),
/// and whether an `AGENTS.md` was created. Dot-prefixed so it sorts/hides like the
/// other bookkeeping; lives inside the data folder, so `remove_dir_all` of that
/// folder also disposes of it.
pub(crate) const MANIFEST_NAME: &str = ".init-manifest.json";

/// The snapshot `init` writes so `deinit` is an exact rollback (not a heuristic).
/// Serialized to `<data_folder>/.init-manifest.json`. Best-effort: a write failure
/// never fails init, and `deinit` tolerates a missing/old/garbled manifest by
/// falling back to the `imports/` sidecars + the `GITIGNORE_MARKER` block.
#[derive(serde::Serialize, serde::Deserialize, Default, Debug)]
pub(crate) struct InitManifest {
    /// Schema version; bump if the shape changes. `deinit` reads tolerantly.
    pub version: u32,
    /// The data-folder name init used (echo of `config.data_dir`), e.g. `memory`.
    pub data_dir: String,
    /// State of the ROOT `.gitignore` *before* init touched it.
    pub root_gitignore: GitignoreState,
    /// True if init CREATED an `AGENTS.md` (no instruction file existed). Lets
    /// `deinit` remove it authoritatively even if the user later edited it.
    pub created_agents_md: bool,
    /// True if init CREATED the standalone `CM_GUIDE.md` (none existed). Lets `deinit`
    /// remove exactly the file we wrote, never a user's own. Defaults to false so an
    /// older manifest (no such field) deserializes cleanly and deinit just skips it.
    #[serde(default)]
    pub created_cm_guide: bool,
    /// One record per doc init actually imported (the delete-originals prompt may
    /// have removed the source — see `DocRecord::deleted`).
    pub docs: Vec<DocRecord>,
}

/// Pre-init state of the project-root `.gitignore`, so `deinit` can choose between
/// leaving it, restoring its exact bytes, or deleting it (when init created it).
#[derive(serde::Serialize, serde::Deserialize, Default, Debug)]
pub(crate) struct GitignoreState {
    /// True iff init APPENDED its cm-ignore block (or created the file). When false
    /// (the user already ignored `cm.exe`, or our marker was present), deinit leaves
    /// the file untouched.
    pub modified: bool,
    /// True iff a root `.gitignore` existed BEFORE init. With `modified == true`:
    /// `existed` → restore `original`; `!existed` → init created it, so delete it.
    pub existed: bool,
    /// Exact pre-init bytes of the root `.gitignore` (empty when `!existed`). Only
    /// meaningful when `modified == true`.
    pub original: String,
}

/// One imported doc, recorded so `deinit` can put it back where it came from.
#[derive(serde::Serialize, serde::Deserialize, Default, Debug)]
pub(crate) struct DocRecord {
    /// The doc's original source path, RELATIVE to the project target when it sits
    /// under it (so `deinit` from a different cwd still restores correctly), else the
    /// raw/absolute path. Uses forward slashes. Resolved against `target` by deinit.
    pub orig_path: String,
    /// The copy's filename under `imports/` (`ImportResult::import_name`).
    pub import_copy: String,
    /// True iff init deleted the original from disk (the delete-originals prompt).
    pub deleted: bool,
}

/// Make sure the project root's `.gitignore` ignores the cm binary, so a ~tens-of-MB
/// rebuildable exe placed at the root isn't swept into a commit. Idempotent: if our
/// marker is already present we do nothing; otherwise we APPEND our block (creating
/// the file if absent), never rewriting the user's existing rules. config.json beside
/// the binary is intentionally NOT ignored — it's small and meant to be committed.
fn ensure_binary_ignored(target: &Path) -> GitignoreState {
    let path = target.join(".gitignore");
    // Snapshot the pre-init state BEFORE we read/write, so the manifest can restore
    // the file byte-for-byte (or know to delete one we create).
    let existed = path.is_file();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(GITIGNORE_MARKER) || existing.lines().any(|l| l.trim() == "cm.exe") {
        // Already covered (by us, or by the user's own rule) — we change nothing, so
        // deinit must leave it alone.
        return GitignoreState {
            modified: false,
            existed,
            original: existing,
        };
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let block = format!("{sep}{GITIGNORE_MARKER}\ncm\ncm.exe\n");
    match std::fs::write(&path, format!("{existing}{block}")) {
        // We appended our block (or created the file): deinit restores `original`
        // (or deletes the file if it didn't exist before).
        Ok(()) => GitignoreState {
            modified: true,
            existed,
            original: existing,
        },
        Err(e) => {
            eprintln!("      could not update .gitignore (binary will not be ignored): {e}");
            // Write failed → the file is unchanged → deinit must not touch it.
            GitignoreState {
                modified: false,
                existed,
                original: existing,
            }
        }
    }
}

/// What `import_existing_md` did, for the final summary. `imported` is the count of
/// .md files absorbed (each may be many chunks); `chunks` the total chunk count.
#[derive(Default)]
struct DocStats {
    imported: usize,
    chunks: usize,
    deleted: usize,
    /// One record per imported doc, in import order, for the rollback manifest. The
    /// index lines up with `imported_paths` so the delete loop can flip `deleted`.
    records: Vec<DocRecord>,
}

/// One-glance summary on stderr after all steps: docs absorbed, code indexed (with a
/// per-language breakdown and anything skipped), and where the folder lives.
fn print_summary(folder: &Path, docs: &DocStats, code: Option<&MapSummary>) {
    eprintln!("\n── Done ──");
    if docs.imported > 0 {
        eprint!(
            "  Memory: {} chunk(s) from {} doc(s)",
            docs.chunks, docs.imported
        );
        if docs.deleted > 0 {
            eprint!("; originals deleted: {}", docs.deleted);
        }
        eprintln!();
    } else {
        eprintln!("  Memory: no docs imported");
    }
    match code {
        Some(c) => {
            eprintln!(
                "  Code: {} symbol(s), {} file(s) [{}]",
                c.symbols, c.files, c.langs
            );
            if c.no_grammar > 0 || c.unparsed > 0 {
                let mut skipped = Vec::new();
                if c.no_grammar > 0 {
                    skipped.push(format!("{} no grammar", c.no_grammar));
                }
                if c.unparsed > 0 {
                    skipped.push(format!("{} failed to parse", c.unparsed));
                }
                eprintln!("       skipped: {}", skipped.join(", "));
            }
        }
        None => eprintln!("  Code: indexing skipped"),
    }
    eprintln!("  Folder: {}", folder.to_string_lossy());
}

/// Pick init's target directory: the first positional if given, else `cwd` (the
/// process working directory). The default is what makes a bare `cm init` work —
/// it scaffolds `memory/` in the project root the command was run from, no matter
/// where the binary itself lives. Pure (takes `cwd` explicitly) so it's testable
/// without mutating the process CWD.
fn resolve_target(p: &Parsed, cwd: &Path) -> PathBuf {
    match p.arg(0) {
        Some(t) => PathBuf::from(t),
        None => cwd.to_path_buf(),
    }
}

/// Code-indexing result folded into init's summary: the graph totals plus the
/// per-language breakdown and what the walk skipped, so the step can report
/// "302 symbols, 57 files [C# 57], skipped: 12 no grammar".
struct MapSummary {
    files: i64,
    symbols: i64,
    edges: i64,
    langs: String,
    no_grammar: usize,
    unparsed: usize,
}

/// Index the project's source tree (rooted at `target`) into the code graph, as
/// part of `init`. Best-effort: any failure (no `code` feature, unreadable store,
/// parse error) is reported to stderr and yields `None`, never aborting the init
/// that already succeeded. Reuses `commands::map_tree`, the same core `cm map`
/// runs; the memory `folder` is passed as `mem_dir` so the walk skips it (its
/// store/binary aren't project source).
fn map_source_tree(target: &Path, folder: &Path, store_path: &Path) -> Option<MapSummary> {
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("      skipping: could not open store: {}", e.msg);
            return None;
        }
    };
    // Show a live progress bar while the tree is parsed (it can be hundreds of
    // files). Only when stderr is a real terminal — a redirected/piped stderr (CI,
    // logs) would otherwise fill with `\r`-spam, so there we stay silent and let the
    // step's final summary line speak. The bar is the only `progress` here; passing
    // `None` would index just as correctly but appear hung.
    let bar: Option<Box<dyn Fn(usize, usize)>> = if std::io::stderr().is_terminal() {
        Some(Box::new(progress_bar))
    } else {
        None
    };
    let progress = bar.as_deref();
    match commands::map_tree(&store, target, folder, None, None, progress) {
        Ok(stats) => {
            // Erase the bar's line so the summary that follows starts clean.
            if progress.is_some() {
                clear_progress_line();
            }
            let _ = store.log_op("map", Some(&target.to_string_lossy()));
            let (files, symbols, edges) = store.code_counts().unwrap_or((0, 0, 0));
            let langs = commands::lang_breakdown(&stats.by_lang);
            eprintln!("      {symbols} symbol(s) in {files} file(s) [{langs}]");
            Some(MapSummary {
                files,
                symbols,
                edges,
                langs,
                no_grammar: stats.no_grammar,
                unparsed: stats.unparsed,
            })
        }
        Err(e) => {
            // Erase any partial bar line before the error so it reads cleanly.
            if progress.is_some() {
                clear_progress_line();
            }
            // The headline case: binary built without the `code` feature. Surface
            // the rebuild hint, but keep the freshly-scaffolded store intact.
            eprintln!("      skipping: code indexing unavailable: {}", e.msg);
            if let Some(hint) = &e.hint {
                eprintln!("      {hint}");
            }
            None
        }
    }
}

/// True if the data folder still holds md SOURCE OF TRUTH — at least one `.md` under
/// `notes/` or `imports/`. Used by the repair path to decide whether a freshly
/// (re)created store.db has documents to rebuild from. A fresh init (empty folders)
/// returns false, so we don't run a pointless reindex.
fn has_md_truth(data_folder: &Path) -> bool {
    let has_md_in = |dir: PathBuf| -> bool {
        std::fs::read_dir(dir).ok().is_some_and(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.file_type().is_ok_and(|t| t.is_file()) && is_md(&e.path()))
        })
    };
    has_md_in(data_folder.join("notes")) || has_md_in(data_folder.join("imports"))
}

/// Rebuild the DOCUMENT index (notes/ + imports/) from the md truth after init had to
/// (re)create an empty store.db — so `recall` works immediately on a repair instead of
/// silently returning nothing until the user runs `cm reindex` by hand. Reuses the very
/// same `commands::reindex` the `cm reindex` command runs, via a `Ctx` rooted at the
/// project target (it reads `config.json` there to resolve the data dir). Best-effort:
/// any error is a non-fatal stderr warning — the store is valid (just empty), and
/// `cm reindex` stays the manual fallback. This handles DOCUMENTS only; the code graph
/// is (re)built separately in step [4/N].
fn reindex_documents(target: &Path, data_folder: &Path) {
    eprintln!("      store.db was missing — rebuilding the document index from md…");
    // An empty Parsed → no `--all`, so reindex runs incrementally (full rebuild here
    // anyway, since the store is empty and every md file reads as new).
    let empty = Parsed::parse(&[]);
    let ctx = commands::Ctx::new(target);
    match commands::reindex(&empty, &ctx) {
        Ok(()) => {} // reindex already printed its own {"indexed","changed"} JSON line
        Err(e) => eprintln!(
            "      could not rebuild the document index ({}); run `cm reindex` to fix. [{}]",
            e.msg,
            data_folder.to_string_lossy()
        ),
    }
}

/// Width of the ASCII progress bar's fill area (characters between the brackets).
const PROGRESS_BAR_WIDTH: usize = 18;

/// Build the progress-bar text for `done`/`total`: `      [######------------]  47%  142/300`.
/// Pure (no I/O) so the layout is unit-testable. `filled` is the floor of the ratio
/// so the bar only shows full at completion; `pct` likewise floors (49/100 reads as
/// 49%, never a premature 50). Returns the empty string for `total == 0` (no source
/// files) — the caller then draws nothing. No leading `\r` / trailing escape: those
/// are the I/O concern of `progress_bar`.
fn render_bar(done: usize, total: usize) -> String {
    if total == 0 {
        return String::new();
    }
    // Clamp so a `done > total` slip can't overflow the fill or exceed 100%.
    let done = done.min(total);
    let filled = done * PROGRESS_BAR_WIDTH / total;
    let pct = done * 100 / total;
    let bar: String = std::iter::repeat_n('#', filled)
        .chain(std::iter::repeat_n('-', PROGRESS_BAR_WIDTH - filled))
        .collect();
    format!("      [{bar}] {pct:>3}%  {done}/{total}")
}

/// Draw the in-place indexing progress bar to stderr: `[######------] 47% 142/300`,
/// rewritten on the same line via a leading `\r`. Called once per file by
/// `map_tree`; only wired in when stderr is a terminal (so non-TTY runs never see
/// `\r`-spam — `map_source_tree` decides that). Keeps data/JSONL on stdout untouched
/// (conventions.md: narration is stderr-only).
fn progress_bar(done: usize, total: usize) {
    use std::io::Write;
    let line = render_bar(done, total);
    if line.is_empty() {
        return;
    }
    // `\r` returns to column 0; no trailing newline, so the next call overwrites.
    // `\x1b[K` clears any leftover from a previously longer line.
    let mut err = std::io::stderr();
    let _ = write!(err, "\r{line}\x1b[K");
    let _ = err.flush();
}

/// Erase the progress bar's line (return to column 0 and clear to end-of-line) so
/// whatever prints next — the summary or an error — starts on a clean line. Paired
/// with `progress_bar`; only called when the bar was actually drawn.
fn clear_progress_line() {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[K");
    let _ = err.flush();
}

/// Pull the project's docs into the store. Two modes:
///   * `docs_arg = Some("a,b,c")` — import exactly those paths (each a dir, walked
///     recursively for .md, or a single .md file). No scan, no prompt: the user
///     said precisely what they want.
///   * `docs_arg = None` — auto-scan the target tree for .md, but SKIP the agent's
///     root entry-point files (README/CLAUDE/AGENTS/…), which are wired in place,
///     not absorbed. We then list the doc FOLDERS we found and ask one y/N.
///
/// Nothing here is fatal: errors go to stderr as warnings, and an import that
/// failed is never deleted. If stdin isn't readable (piped or non-interactive
/// use) we just quietly skip the whole thing.
fn import_existing_md(
    target: &Path,
    folder: &Path,
    cfg: &Config,
    store_path: &Path,
    docs_arg: Option<&str>,
) -> DocStats {
    let mut stats = DocStats::default();
    // An empty/whitespace --docs value (e.g. `--docs ""`, `--docs`, `--docs ,`) is
    // treated as "not given" so we fall through to the auto-scan instead of silently
    // importing nothing and skipping the scan+prompt.
    let docs_arg = docs_arg.filter(|a| docs_arg_has_entry(a));
    let md_files = match docs_arg {
        // Explicit paths: take them verbatim (still skipping the memory folder, in
        // case the user points at the project root). No confirmation prompt.
        Some(arg) => {
            let files = collect_docs_from_args(arg, folder);
            eprintln!("      --docs selected {} .md", files.len());
            files
        }
        // Auto-scan: gather the tree, then confirm by folder.
        None => {
            // Recurse the docs tree, but never descend into the memory folder we
            // just laid out under `target` (when target is `./`, `folder` lives
            // inside it): its notes/ + imports/ originals aren't user docs to
            // re-absorb. Root entry-point files are skipped (they get wired).
            let found = collect_md_files(target, folder);
            if found.is_empty() {
                eprintln!("      no .md files found — skipping import");
                return stats;
            }
            eprintln!("      found {} .md in folders:", found.len());
            for (dir, count) in doc_folder_summary(target, &found) {
                eprintln!("        {dir}  ({count} .md)");
            }
            if !prompt_yes_no("      Import from these folders? [y/N]: ") {
                eprintln!("      import declined");
                return stats;
            }
            found
        }
    };
    if md_files.is_empty() {
        return stats;
    }

    let emb = match embed::build(cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("      import skipped (embedder): {}", e.msg);
            return stats;
        }
    };
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("      import skipped (store): {}", e.msg);
            return stats;
        }
    };
    let imports_dir = folder.join("imports");
    let mut imported_paths: Vec<&PathBuf> = Vec::new();

    for path in &md_files {
        match import::import_file(&store, emb.as_ref(), cfg, path, "", &imports_dir) {
            Ok(res) => {
                imported_paths.push(path);
                // Record provenance for the rollback manifest, in the same order as
                // `imported_paths` (the delete loop flips `deleted` by index). We
                // store the original path relative to `target` when possible, so a
                // later `deinit` (perhaps from a different cwd) restores it correctly.
                stats.records.push(DocRecord {
                    orig_path: rel_or_raw(target, path),
                    import_copy: res.import_name,
                    deleted: false,
                });
                stats.imported += 1;
                stats.chunks += res.chunks;
                print_line(&json!({
                    "imported": path.to_string_lossy(),
                    "chunks": res.chunks,
                }));
            }
            Err(e) => {
                eprintln!("      could not import {}: {}", path.display(), e.msg);
            }
        }
    }
    let _ = store.log_op(
        "init-import",
        Some(&format!("{} files", imported_paths.len())),
    );
    eprintln!(
        "      imported {} doc(s), {} chunk(s)",
        stats.imported, stats.chunks
    );

    if imported_paths.is_empty() {
        return stats;
    }

    // Be explicit about what "delete originals" touches: ONLY the imported .md
    // (copies are safe in imports/), never source code. The earlier confusing
    // prompt made a user fear `init` was deleting their .cs files.
    eprintln!(
        "      Imported .md copied to {}/imports/ (source of truth).",
        folder.to_string_lossy()
    );
    if prompt_yes_no(&format!(
        "      Delete originals of these {} .md? (source code is NOT touched) [y/N]: ",
        imported_paths.len()
    )) {
        for (i, path) in imported_paths.iter().enumerate() {
            if let Err(e) = std::fs::remove_file(path) {
                eprintln!("      could not delete {}: {e}", path.display());
            } else {
                stats.deleted += 1;
                // Mark the matching manifest record (same index as imported_paths)
                // so deinit knows it must restore this original, not just leave it.
                if let Some(rec) = stats.records.get_mut(i) {
                    rec.deleted = true;
                }
            }
        }
        eprintln!("      originals deleted: {}", stats.deleted);
    }
    stats
}

/// File names we treat as an agent's "entry-point" instruction docs — the ones a
/// model reads at the start of a session. We append a pointer to each so the model
/// learns to pull project docs through `cm recall` instead of reading them whole.
/// Matched case-insensitively by the path's tail (so `.github/copilot-instructions.md`
/// matches a nested file too). Keep in sync with help::HELP / README.
pub(crate) const ENTRY_POINT_NAMES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "AGENT.md",
    "GEMINI.md",
    ".cursorrules",
    ".github/copilot-instructions.md",
];

/// Markers bracketing the block we append, so a re-run can detect "already wired"
/// and skip it (idempotent), and a human can find/remove it by hand. `deinit`
/// strips exactly this region back out (init.rs::unwire_entry_points).
pub(crate) const WIRE_BEGIN: &str = "<!-- BEGIN cm memory pointer -->";
pub(crate) const WIRE_END: &str = "<!-- END cm memory pointer -->";

/// Outcome of wiring entry-point files. `wired` lists the short names we touched
/// (appended or refreshed). `any_existed` is true if ANY entry-point file was
/// present at all — even one already carrying a current block (so the caller
/// doesn't mistake an idempotent re-run for "no instruction files here" and create
/// a redundant AGENTS.md).
struct WireResult {
    wired: Vec<String>,
    any_existed: bool,
}

/// Append (or refresh) a "use cm, not the raw doc" pointer in each entry-point file
/// found under `target`. Best-effort and idempotent:
///   * no block yet     -> append it after the existing content;
///   * block already there, identical -> leave the file untouched (no output);
///   * block already there but stale (different exe path, e.g. a re-init under a new
///     `--name`) -> replace just the bracketed block in place.
///
/// We never create these files here, only edit ones that already exist (creating a
/// fresh `AGENTS.md` when none exist is the caller's job); any I/O error is a
/// non-fatal stderr warning.
fn wire_entry_points(target: &Path, exe_display: &str) -> WireResult {
    let block = entry_point_block(exe_display);
    let mut wired: Vec<String> = Vec::new();
    let mut any_existed = false;
    for rel in ENTRY_POINT_NAMES {
        // Split on '/' so a nested name like `.github/copilot-instructions.md`
        // resolves under `target` on any platform (PathBuf joins per-component).
        let path = rel
            .split('/')
            .fold(target.to_path_buf(), |p, seg| p.join(seg));
        if !path.is_file() {
            continue;
        }
        any_existed = true;
        let existing = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("      could not read {}: {e}", path.display());
                continue;
            }
        };

        let updated = match replace_block(&existing, &block) {
            // A block is present. `None` means it's already current -> skip silently.
            Some(replaced) => replaced,
            None if existing.contains(WIRE_BEGIN) => continue,
            // No block yet: append after the content, separated by a blank line
            // (one extra '\n' if the file doesn't already end in a newline).
            None => {
                let sep = if existing.is_empty() || existing.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{existing}{sep}{block}\n")
            }
        };

        match std::fs::write(&path, updated) {
            Ok(()) => {
                print_line(&json!({ "wired": path.to_string_lossy() }));
                // Show the short name (CLAUDE.md), not the full path, in the summary.
                wired.push((*rel).to_string());
            }
            Err(e) => eprintln!("      could not write {}: {e}", path.display()),
        }
    }
    WireResult { wired, any_existed }
}

/// Create a fresh `AGENTS.md` at the target root with a short header and the cm
/// pointer block, for the case where a project has NO agent-instruction file at
/// all. Gives a model something to read at session start that points it at `cm`.
/// Best-effort: a write failure is a non-fatal warning. Returns the file's display
/// name on success so the step can report it. Does nothing (returns None) if an
/// AGENTS.md somehow already exists — we never clobber.
fn create_agents_md(target: &Path, exe_display: &str) -> Option<String> {
    let path = target.join("AGENTS.md");
    if path.exists() {
        return None; // never overwrite
    }
    let block = entry_point_block(exe_display);
    let contents = format!("{AGENTS_HEADER}\n{block}\n");
    match std::fs::write(&path, contents) {
        Ok(()) => {
            // Report as a wired entry point, but flag that we created it (vs edited
            // an existing one). Distinct key from init's final {"created":<folder>}.
            print_line(&json!({ "wired": path.to_string_lossy(), "new_file": true }));
            Some("AGENTS.md".to_string())
        }
        Err(e) => {
            eprintln!("      could not create AGENTS.md: {e}");
            None
        }
    }
}

/// The header `create_agents_md` puts above the pointer block. Kept as a constant so
/// `deinit` can recognize a file that is ONLY this header + (stripped) block and
/// remove it wholesale — i.e. round-trip a cm-created AGENTS.md back to nothing.
pub(crate) const AGENTS_HEADER: &str = "# AGENTS\n\n\
    Instructions for AI agents in this project.\n";

/// Filename of the standalone, full usage guide `init` drops at the project ROOT.
/// The short pointer block wired into the instruction files links here for the
/// complete contract, so an agent never has to spend a turn on `cm help`: the same
/// knowledge is a plain file it can read like any other doc. `deinit` removes it iff
/// init created it (tracked in the manifest's `created_cm_guide`).
pub(crate) const CM_GUIDE_NAME: &str = "CM_GUIDE.md";

/// Render the standalone `CM_GUIDE.md`: a self-contained, agent-agnostic manual for
/// `cm` with the exact JSON each command returns. Written so even a small model can
/// follow it on the first read — concrete `command → output` pairs, the read/write
/// loop, and an explicit "when map vs grep". `{exe}` is substituted with the binary's
/// display path so copy-paste works verbatim in this project.
fn cm_guide(exe: &str) -> String {
    format!(
        "# Using `{exe}` — project memory & code map\n\
         \n\
         `{exe}` is this project's memory tool: a small command-line program that stores\n\
         notes and documents on disk and searches them, and also holds a structural map of\n\
         the source code. The project's real documentation lives inside it — the\n\
         instruction files (CLAUDE.md / AGENTS.md / …) only point here.\n\
         \n\
         **You (the coding agent) are the intended user.** This guide is everything you\n\
         need; you do not have to run `{exe} help` to get started.\n\
         \n\
         ## The two rules that matter most\n\
         \n\
         1. **Every command prints JSONL** — one JSON object per line on stdout. Parse it;\n\
         do not guess. Errors and hints go to stderr with one correct example to copy.\n\
         2. **Text you save comes from STDIN, never an argument.** Pipe it in:\n\
         `echo \"a fact\" | {exe} remember`  ✅   ·   `{exe} remember \"a fact\"`  ❌ (ignored).\n\
         \n\
         ## Memory: the everyday read/write loop\n\
         \n\
         **Recall before you answer.** Search notes and imported docs by topic:\n\
         ```\n\
         {exe} recall \"how does authentication work\" --limit 5\n\
         → {{\"id\":\"0a1b2c3d\",\"kind\":\"note\",\"body\":\"Auth uses JWT; refresh lasts 30 days.\"}}\n\
         ```\n\
         Each line is one match, best first. A short note prints its whole `body`; a\n\
         long one comes back as `preview`+`chars` (a budgeted summary) — read that\n\
         first, and `{exe} get <id>` only when you need the rest. Useful flags:\n\
         `--limit N` (how many), `--full` (whole bodies, no preview),\n\
         `--budget C` (cap each body at C chars), `--tag T` (only that tag).\n\
         \n\
         **Remember after you decide.** Save a fact for next time (body on stdin):\n\
         ```\n\
         echo \"Decided: store config as JSON next to the binary.\" | {exe} remember --tags decision\n\
         → {{\"id\":\"7f3e9a01\"}}\n\
         ```\n\
         The id names the note's file (`notes/7f3e9a01.md`). Get one back in full with\n\
         `{exe} get 7f3e9a01`; list the newest with `{exe} list`.\n\
         \n\
         **Anchor a note to the code it documents.** When a note is about specific code,\n\
         name the symbols with `--code-refs`:\n\
         ```\n\
         echo \"Auth issues a JWT; refresh lasts 30 days.\" | {exe} remember --code-refs \"validate_token\"\n\
         ```\n\
         (or inline in the body: `… see [[code:validate_token]] …` — same effect).\n\
         On `recall` each anchor is resolved live against the code map: it comes back with its\n\
         current `path`/`line` (`resolved:true`), or `resolved:false` if that symbol is gone —\n\
         so the note tells you whether the code it describes still exists. Anchor by NAME\n\
         (survives line edits), not by line number. The mirror direction — which notes document\n\
         a symbol — is `{exe} backlinks --symbol <name>`.\n\
         \n\
         ## Code map: structural questions about the source\n\
         \n\
         A SEPARATE graph of the code (it never mixes into recall): which symbols exist,\n\
         where each is defined, and which uses which. Indexed automatically at `{exe} init`;\n\
         after you edit code, refresh it (incremental): `{exe} map <path>`.\n\
         \n\
         Prefer it over grep for \"where / who / what\" questions about a symbol — it knows\n\
         symbol boundaries, so it never matches a name inside a string, a comment, or a\n\
         longer word, and it tells a definition apart from a use. Each mode prints one JSON\n\
         object per line:\n\
         \n\
         | Question | Command | Output fields |\n\
         |---|---|---|\n\
         | Where is X defined? | `{exe} map --query X` | `name, kind, path, line, signature` |\n\
         | Any symbol like …? | `{exe} map --like part` | `name, kind, path, line, signature` |\n\
         | What does file F define? | `{exe} map --defines F` | `name, kind, path, line, signature` |\n\
         | Who calls / uses X? | `{exe} map --uses X` | `name, kind, path, line, def_line` |\n\
         | What does X depend on? | `{exe} map --calls X` | `calls, line, resolved` |\n\
         | What are the key symbols? | `{exe} map --list` | `name, kind, path, line, signature, degree` |\n\
         \n\
         Worked examples with real output:\n\
         ```\n\
         {exe} map --query upsert_note\n\
         → {{\"name\":\"upsert_note\",\"kind\":\"method\",\"path\":\"store.rs\",\"line\":268,\"signature\":\"pub fn upsert_note(\"}}\n\
         \n\
         {exe} map --uses recall_with        # who calls it, attributed to the caller fn\n\
         → {{\"name\":\"run\",\"kind\":\"function\",\"path\":\"commands.rs\",\"line\":142,\"def_line\":120}}\n\
         \n\
         {exe} map --calls recall_with       # its in-project dependencies\n\
         → {{\"calls\":\"fts_search\",\"line\":196,\"resolved\":true}}\n\
         ```\n\
         `{exe} map --list` ranks symbols by how connected they are (`degree`) and shows\n\
         the top 30 hubs — the architecture skeleton; `--all` lists every symbol, `--limit N`\n\
         resizes the cap. For symbol listings, `--kind function|method|class|…` narrows by\n\
         kind, and `--tests` opts test code back in (hidden by default).\n\
         \n\
         **When to use grep instead:** free text (a string, an error message, a TODO, a\n\
         config key); very common/overloaded names (`new`, `get`, `run` — the map merges\n\
         same-named symbols and may misattribute); or code you just edited but haven't\n\
         re-mapped. Rule of thumb: *unique-ish name + structural question → `{exe} map`;\n\
         common name, free text, or just-edited code → grep.*\n\
         \n\
         ## Feedback: help improve `{exe}`\n\
         \n\
         You are the agent who actually USES this tool, so you see its rough edges before\n\
         anyone else. When `{exe}` is missing a command or flag, returns output that's\n\
         awkward to act on, or behaves surprisingly, record it — the text comes from STDIN,\n\
         exactly like `remember`:\n\
         ```\n\
         echo \"cm map --uses misses trait impls; a --kind filter would help\" | {exe} feedback\n\
         → {{\"id\":\"7f3e9a01\"}}\n\
         ```\n\
         It is saved into THIS project's memory as a note tagged `cm-feedback` (stamped\n\
         with the cm version), so it never gets lost. Add `--tags <area>` to mark what it's\n\
         about (e.g. `--tags map`). Whoever maintains cm reviews it later with\n\
         `{exe} feedback --list` (or `{exe} recall \"<topic>\" --tag cm-feedback`). Short,\n\
         concrete notes about what would have helped are exactly what makes the tool\n\
         better — don't hold back.\n\
         \n\
         ## Where things live\n\
         \n\
         At the project root: `{exe}` (the binary) and `config.json` (settings). The data —\n\
         `notes/` and `imports/` (the real memory) plus `store.db` (a rebuildable search\n\
         index) — sits in the data folder named by `config.json`. You don't manage these by\n\
         hand; use the commands above.\n\
         \n\
         ## Full command reference\n\
         \n\
         This file covers the everyday loops. For every command, flag, and output field —\n\
         including `import`, `export`, `related`/`backlinks` (the note graph), `reindex`,\n\
         `config`, and `deinit` — run `{exe} help`. It prints the complete contract that\n\
         ships inside the binary, always in sync with this version.\n",
        exe = exe,
    )
}

/// Write `CM_GUIDE.md` at the project root, unless a file by that name already exists
/// (never clobber a user's). Returns true iff init created it, so the manifest can
/// record it and `deinit` remove exactly the file we made. Best-effort: a write
/// failure is a non-fatal stderr warning (the scaffold already succeeded).
fn write_cm_guide(target: &Path, exe_display: &str) -> bool {
    let path = target.join(CM_GUIDE_NAME);
    if path.exists() {
        eprintln!("      {CM_GUIDE_NAME} already present — left as is");
        return false;
    }
    match std::fs::write(&path, cm_guide(exe_display)) {
        Ok(()) => {
            print_line(&json!({ "guide": path.to_string_lossy(), "new_file": true }));
            true
        }
        Err(e) => {
            eprintln!("      could not write {CM_GUIDE_NAME}: {e}");
            false
        }
    }
}

/// If `text` already carries a marker-bracketed block AND it differs from `block`,
/// return `text` with that region swapped for `block` (stale pointer self-heals on
/// a re-init under a new path). Returns `None` when there's no block, or when the
/// existing one is already identical (nothing to do). Only the FIRST block is
/// touched; a malformed region (BEGIN without a following END) is left alone.
fn replace_block(text: &str, block: &str) -> Option<String> {
    let begin = text.find(WIRE_BEGIN)?;
    // END must come after BEGIN; include the marker itself in the cut.
    let end_rel = text[begin..].find(WIRE_END)?;
    let end = begin + end_rel + WIRE_END.len();
    if text[begin..end] == *block {
        return None; // already current
    }
    Some(format!("{}{}{}", &text[..begin], block, &text[end..]))
}

/// The reverse of wiring: strip the marker-bracketed block (and the blank line we
/// used to separate it from prior content) back out of each entry-point file under
/// `target`. Reused by `deinit` to leave the docs as the user had them. Returns the
/// number of files actually changed. Best-effort: a file without the block, or one
/// we can't read/write, is just skipped (write errors warn on stderr).
pub(crate) fn unwire_entry_points(target: &Path) -> usize {
    let mut changed = 0;
    for rel in ENTRY_POINT_NAMES {
        let path = rel
            .split('/')
            .fold(target.to_path_buf(), |p, seg| p.join(seg));
        if !path.is_file() {
            continue;
        }
        let existing = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: could not read {}: {e}", path.display());
                continue;
            }
        };
        let Some(stripped) = strip_block(&existing) else {
            continue; // no block here — nothing to undo
        };
        // If what's left is exactly the header cm wrote when it CREATED this file
        // (an AGENTS.md born from `init` with no prior instruction file), remove the
        // file wholesale so deinit round-trips back to nothing. A user's own file
        // (any other residual content) is preserved, just block-stripped.
        if stripped.trim_end() == AGENTS_HEADER.trim_end() {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    changed += 1;
                    print_line(&json!({ "removed": path.to_string_lossy() }));
                }
                Err(e) => eprintln!("warning: could not remove {}: {e}", path.display()),
            }
            continue;
        }
        match std::fs::write(&path, stripped) {
            Ok(()) => {
                changed += 1;
                print_line(&json!({ "unwired": path.to_string_lossy() }));
            }
            Err(e) => eprintln!("warning: could not update {}: {e}", path.display()),
        }
    }
    changed
}

/// Remove the first marker-bracketed block from `text`, plus the single blank-line
/// separator that wiring inserted before it (so a once-wired file round-trips back
/// to its original bytes). Returns `None` if there's no well-formed block. Mirrors
/// the cut in `replace_block`, then trims one leading "\n\n"/"\n" the append added.
fn strip_block(text: &str) -> Option<String> {
    let begin = text.find(WIRE_BEGIN)?;
    let end_rel = text[begin..].find(WIRE_END)?;
    let end = begin + end_rel + WIRE_END.len();
    let mut before = text[..begin].to_string();
    let after = &text[end..];
    // `wire_entry_points` appended "\n" + block + "\n" (with a "\n\n" separator when
    // the file didn't end in a newline). Undo that: drop the trailing newline(s) we
    // added before the block, and a single trailing newline after it.
    while before.ends_with('\n') {
        before.pop();
    }
    let after = after.strip_prefix('\n').unwrap_or(after);
    // If there was real content before the block, restore its terminating newline.
    let mut out = before;
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(after);
    Some(out)
}

/// The marker-bracketed pointer block appended to an entry-point file. Instructs
/// the agent to reach for project docs via `cm recall` rather than reading them
/// directly. Designed to be self-sufficient for ANY coding agent (Claude Code,
/// Codex, Cursor, Copilot, Kimi, …): it shows not just the flag NAMES but the exact
/// JSON each call returns, so a model can use `cm` correctly on the first try without
/// a separate `cm help` round-trip. The full contract lives in the generated
/// `CM_GUIDE.md` (linked at the end), not in `cm help`, so even a simple model can
/// read it as a file.
fn entry_point_block(exe_display: &str) -> String {
    format!(
        "{begin}\n\
         ## Project memory & code map — use `{exe}` (read this first)\n\n\
         This project's docs, notes, and a structural map of the source live INSIDE\n\
         `{exe}`, not in these instruction files. `{exe}` is a tiny command-line tool:\n\
         each call prints one JSON object per line (JSONL) — read that JSON, don't guess.\n\
         Why it exists: it returns the exact slice you need in one call, so you neither\n\
         re-read whole docs nor grep blindly through the code.\n\n\
         ### MEMORY — facts, decisions, docs\n\
         - BEFORE answering a project question, recall first:\n\
         `{exe} recall \"<topic>\"` → `{{\"id\":\"0a1b2c3d\",\"kind\":\"note\",\"body\":\"…\"}}` per match.\n\
         A long match comes back as `preview`+`chars` instead of `body`; `{exe} get <id>` for the full text.\n\
         - AFTER a decision worth keeping, save it (the text comes from STDIN, never an arg):\n\
         `echo \"<the fact>\" | {exe} remember` → `{{\"id\":\"0a1b2c3d\"}}`.\n\
         - Documenting specific code? Anchor the note to it: `… | {exe} remember --code-refs \"sym,sym\"`.\n\
         `recall` then returns `code_refs` resolved against the code map (`resolved:false` = that symbol is gone).\n\
         - Don't re-read large docs in full — pull the relevant slice via `recall`.\n\n\
         ### CODE MAP — structural questions about the source (beats grep)\n\
         Sources are indexed at `{exe} init`; re-index after edits — `{exe} map <path>`.\n\
         For \"where/who/what\" questions about symbols, prefer this over grep: it knows\n\
         symbol boundaries (never matches a name in a string or comment) and answers in\n\
         one call. Example with its real output:\n\
         `{exe} map --query <name>` (where a symbol is defined) →\n\
         `{{\"name\":\"…\",\"kind\":\"method\",\"path\":\"store.rs\",\"line\":268,\"signature\":\"…\"}}`\n\
         Modes (each prints one JSON object per line):\n\
         - `{exe} map --query <name>`   where defined  → `{{name,kind,path,line,signature}}`\n\
         - `{exe} map --like <part>`    fuzzy by name  → `{{name,kind,path,line,signature}}`\n\
         - `{exe} map --defines <file>` a file's API   → `{{name,kind,path,line,signature}}`\n\
         - `{exe} map --uses <name>`    who calls it   → `{{name,kind,path,line,def_line}}`\n\
         - `{exe} map --calls <name>`   what it uses   → `{{calls,line,resolved}}`\n\
         - `{exe} map --list`           top hub symbols → `{{name,kind,path,line,signature,degree}}` (--all = every symbol)\n\
         Reliable for unique names; for common names (new/get/run), arbitrary text, or\n\
         just-edited code — use grep instead.\n\n\
         ### FEEDBACK — help make `{exe}` better\n\
         You are the agent who actually uses this tool, so you hit its rough edges first.\n\
         If `{exe}` is missing something, gets in your way, or its output is awkward to act\n\
         on, record it (text via STDIN, like remember) — it's saved to this project's\n\
         memory for the maintainer, no need to leave your work:\n\
         `echo \"<what's missing / to improve>\" | {exe} feedback` → `{{\"id\":\"…\"}}`.\n\n\
         ### More\n\
         Full step-by-step contract with examples: open `CM_GUIDE.md` in this project\n\
         (or run `{exe} help`). Both are equivalent — the file needs no extra call.\n\
         {end}",
        begin = WIRE_BEGIN,
        end = WIRE_END,
        exe = exe_display,
    )
}

/// Recursively collect the Markdown files under `dir` (sorted), skipping `exclude`
/// (the folder we just scaffolded) and ANY other climem memory folder we run into.
/// Without that, an `init` next to an earlier memory folder would re-absorb its
/// `imports/` copies (and a re-run would loop self-import). We also skip the
/// agent's ROOT entry-point files (README/CLAUDE/AGENTS/…) sitting directly in
/// `dir`: those get a pointer wired into them, not absorbed as docs (a deeper
/// `docs/README.md` is still collected — the skip is root-only). We match
/// `.md`/`.markdown` case-insensitively, the exact same set `import` treats as
/// Markdown, so a `README.MD` can't get skipped here while import would happily
/// have taken it. No `walkdir` dependency: a small hand-rolled walk keeps the
/// dependency tree minimal (CLAUDE.md).
fn collect_md_files(dir: &Path, exclude: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    walk_md(dir, Some(dir), exclude, &mut paths);
    paths.sort();
    paths
}

fn is_md(p: &Path) -> bool {
    p.extension()
        .and_then(|x| x.to_str())
        .map(|x| {
            let x = x.to_lowercase();
            x == "md" || x == "markdown"
        })
        .unwrap_or(false)
}

/// True if a `--docs` value names at least one non-empty path. `""`, `"  "`, `","`,
/// `" , "` → false (treat as "not given" → fall through to the auto-scan).
fn docs_arg_has_entry(arg: &str) -> bool {
    arg.split(',').any(|e| !e.trim().is_empty())
}

/// Resolve `--docs a,b,c` into a sorted, de-duplicated list of .md files. Each
/// comma-separated entry is a path that may be a directory (walked recursively for
/// .md, with NO root-entry-point skip — the user asked for it explicitly) or a
/// single file (taken as-is, regardless of extension: an explicit path is trusted).
/// The memory folder `exclude` is still pruned in case a named dir contains it.
/// Empty entries and missing paths are skipped with a stderr warning.
fn collect_docs_from_args(arg: &str, exclude: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw in arg.split(',') {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        let path = Path::new(p);
        if path.is_dir() {
            // Walk for .md with NO root-entry-point skip (skip_root=None): an
            // explicit dir is a deliberate choice, so don't second-guess its README.
            walk_md(path, None, exclude, &mut paths);
        } else if path.is_file() {
            paths.push(path.to_path_buf());
        } else {
            eprintln!("warning: --docs: path not found, skipped: {p}");
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Summarize collected .md files by the folder they live in, for the confirmation
/// prompt. Returns `(display_dir, count)` pairs sorted by folder. `display_dir` is
/// the path relative to `root` ("." for files at the root itself), so the user
/// sees `docs (3 .md)` rather than an absolute path.
fn doc_folder_summary(root: &Path, files: &[PathBuf]) -> Vec<(String, usize)> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let parent = f.parent().unwrap_or(root);
        let rel = parent.strip_prefix(root).unwrap_or(parent);
        let display = if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.to_string_lossy().replace('\\', "/")
        };
        *counts.entry(display).or_insert(0) += 1;
    }
    counts.into_iter().collect()
}

/// File names we treat as the project's "root entry points" — the docs a model
/// reads at session start, and which `init` WIRES (appends a pointer) rather than
/// absorbing into the store. Skipped by the auto-scan when they sit directly at
/// the target root. Covers README plus every agent-instruction name we wire
/// (`ENTRY_POINT_NAMES`, by their file-name tail). Matched case-insensitively.
fn is_root_entry_point(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower == "readme.md" || lower == "readme.markdown" {
        return true;
    }
    ENTRY_POINT_NAMES.iter().any(|ep| {
        ep.rsplit('/')
            .next()
            .is_some_and(|tail| tail.eq_ignore_ascii_case(name))
    })
}

/// True if `dir` looks like a climem memory DATA folder, so the doc-scan prunes it
/// wholesale and never re-ingests its `imports/` copies (or `notes/`) as user docs.
/// Recognizes both layouts. In the current SPLIT layout the data folder has
/// `store.db` plus the `imports/` init scaffolds, while config.json lives at the
/// PARENT root — so we must NOT require config.json here. The legacy single-folder
/// layout has `store.db` + `config.json` together. Requiring `imports/` (or
/// config.json) alongside `store.db` avoids matching a random folder that merely
/// happens to hold a file named store.db.
pub(crate) fn is_memory_folder(dir: &Path) -> bool {
    let has_store = dir.join("store.db").is_file();
    has_store && (dir.join("imports").is_dir() || dir.join("config.json").is_file())
}

/// One directory level of the recursive `.md` walk (depth-first): append this
/// level's `.md` files, then recurse into subdirectories. The freshly-scaffolded
/// `exclude` folder and any other memory folder are pruned wholesale; symlinked dirs
/// aren't followed (file_type() reflects the link, so is_dir() is false — guards
/// against loops / escaping the tree). Any unreadable entry is silently skipped.
///
/// `skip_root` controls the one behavioral difference between the auto-scan and the
/// explicit `--docs <dir>` walk: when `Some(root)`, agent entry-point files
/// (README/CLAUDE/…) sitting DIRECTLY in `root` are skipped (they get wired, not
/// imported); when `None`, nothing is skipped (an explicitly named folder is taken
/// whole, README included).
fn walk_md(dir: &Path, skip_root: Option<&Path>, exclude: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if path == exclude || is_memory_folder(&path) {
                continue;
            }
            walk_md(&path, skip_root, exclude, out);
        } else if ft.is_file() && is_md(&path) {
            // Skip root entry-point files only when this level IS the skip-root.
            if skip_root == Some(dir) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if is_root_entry_point(name) {
                        continue;
                    }
                }
            }
            out.push(path);
        }
    }
}

/// Print `question` to stderr and read one line back from stdin. Returns `true`
/// for a yes, and `false` for a no, EOF, or a read error, so piped/non-interactive
/// use safely declines. Shared with `deinit` (a destructive op needs the same
/// guard).
pub(crate) fn prompt_yes_no(question: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => is_yes(&line),
    }
}

/// True if `line` is a yes: `y`/`yes`/`д`/`да`, case-insensitive. We strip a
/// leading UTF-8 BOM first, because PowerShell pipes one in (see `read_stdin` /
/// conventions.md) and otherwise `\u{feff}y` would slip past as a no.
fn is_yes(line: &str) -> bool {
    let cleaned = line.trim_start_matches('\u{feff}').trim().to_lowercase();
    matches!(cleaned.as_str(), "y" | "yes" | "д" | "да")
}

fn display_path(p: &Path) -> String {
    // Show something tidy if we can, otherwise just the raw path.
    let pb: PathBuf = p.to_path_buf();
    pb.to_string_lossy().into_owned()
}

/// How to spell the cm binary as a runnable COMMAND in the wired pointers and guide.
/// When the binary sits under the project `target` (the usual case — init drops it at
/// the root), render it relative to `target` with an explicit `./` (`.\` on Windows)
/// prefix: short to read and a valid command from the project root, where a bare
/// `cm.exe` might not resolve without `.` on PATH. A binary outside `target` (unusual)
/// falls back to its full path. The separator matches the platform so copy-paste runs.
fn exe_command_display(target: &Path, exe_dest: &Path) -> String {
    match exe_dest.strip_prefix(target) {
        Ok(rel) => {
            let sep = std::path::MAIN_SEPARATOR;
            format!(".{sep}{}", rel.to_string_lossy())
        }
        Err(_) => display_path(exe_dest),
    }
}

/// Render an imported doc's source path for the manifest: relative to `target`
/// (forward-slashed) when it sits under it, else the raw path. Storing relative
/// lets a later `deinit` — possibly run from a different working directory —
/// resolve it back against its own `target` and restore the doc to the right spot.
fn rel_or_raw(target: &Path, p: &Path) -> String {
    let rel = p.strip_prefix(target).unwrap_or(p);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::TempDir;

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn is_yes_accepts_affirmatives_case_and_bom_insensitive() {
        for ok in [
            "y",
            "Y",
            "yes",
            "YES",
            "д",
            "да",
            "ДА",
            " y \n",
            "\u{feff}y",
        ] {
            assert!(is_yes(ok), "{ok:?} should be yes");
        }
        for no in ["", "n", "no", "нет", "x", "yep", "\u{feff}n", "ладно"] {
            assert!(!is_yes(no), "{no:?} should not be yes");
        }
    }

    #[test]
    fn render_bar_layout_endpoints_and_midpoint() {
        // Empty store / no source files: nothing to draw.
        assert_eq!(render_bar(0, 0), "");
        // Start: zero fill, 0%, count shown.
        assert_eq!(render_bar(0, 300), "      [------------------]   0%  0/300");
        // Completion: full fill, 100%.
        assert_eq!(
            render_bar(300, 300),
            "      [##################] 100%  300/300"
        );
        // Roughly half (142/300 = 47%): floored fill and percent.
        let mid = render_bar(142, 300);
        assert!(mid.contains(" 47%  142/300"), "{mid:?}");
        // 18-wide fill: 142/300*18 = 8 hashes.
        assert!(mid.contains("[########----------]"), "{mid:?}");
    }

    #[test]
    fn render_bar_clamps_overrun() {
        // A done>total slip must not overflow the fill or exceed 100%.
        assert_eq!(render_bar(5, 3), render_bar(3, 3));
        assert!(render_bar(5, 3).contains("100%"));
    }

    /// A path that can't exist inside the test dir, so nothing is excluded.
    fn no_exclude() -> PathBuf {
        Path::new("/__no_such_exclude__").to_path_buf()
    }

    #[test]
    fn collect_md_files_filters_sorts_and_ignores_non_md() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("b.md"), "x").unwrap();
        std::fs::write(d.join("a.md"), "x").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap(); // wrong extension
        std::fs::write(d.join("C.MD"), "x").unwrap(); // uppercase ext: accepted
        std::fs::write(d.join("doc.markdown"), "x").unwrap(); // .markdown: accepted
        std::fs::create_dir(d.join("sub.md")).unwrap(); // a dir named *.md: not a file
        let got: Vec<String> = collect_md_files(d, &no_exclude())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Only files, sorted; case-insensitive ext, .markdown included. The dir
        // named *.md isn't a file, and being empty it contributes nothing.
        assert_eq!(got, vec!["C.MD", "a.md", "b.md", "doc.markdown"]);
    }

    #[test]
    fn collect_md_files_recurses_into_subdirs() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("top.md"), "x").unwrap();
        let sub = d.join("guide");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("nested.md"), "x").unwrap();
        std::fs::write(sub.join("skip.txt"), "x").unwrap(); // wrong ext, even nested
        let deep = sub.join("deeper");
        std::fs::create_dir(&deep).unwrap();
        std::fs::write(deep.join("way-down.md"), "x").unwrap();
        // File names found, regardless of order — proves the recursion pulls in
        // nested .md files and skips the nested .txt.
        let mut names: Vec<String> = collect_md_files(d, &no_exclude())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["nested.md", "top.md", "way-down.md"]);

        // Sort order is by full path (deterministic): a parent dir's own files
        // come after its subdirectories' when those subdir names sort earlier.
        let paths = collect_md_files(d, &no_exclude());
        assert!(
            paths.windows(2).all(|w| w[0] <= w[1]),
            "must be sorted by path"
        );
    }

    #[test]
    fn collect_md_files_prunes_excluded_folder() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("doc.md"), "x").unwrap();
        // Simulate the scaffolded memory folder sitting inside the docs dir.
        let mem = d.join(".memory");
        std::fs::create_dir_all(mem.join("imports")).unwrap();
        std::fs::write(mem.join("imports").join("copy.md"), "x").unwrap();
        let got: Vec<String> = collect_md_files(d, &mem)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The memory folder's import copies are NOT re-absorbed.
        assert_eq!(got, vec!["doc.md"]);
    }

    #[test]
    fn collect_md_files_prunes_any_memory_folder_not_just_exclude() {
        // The bug a live test caught: `init` next to an EARLIER memory folder
        // (different name, so not `exclude`) re-absorbed its imports/ copies.
        // A memory folder is recognized by its store.db + config.json marker.
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("real.md"), "x").unwrap();

        // A pre-existing memory folder with a different name than `exclude`.
        let old = d.join(".oldmem");
        std::fs::create_dir_all(old.join("imports")).unwrap();
        std::fs::write(old.join("store.db"), "").unwrap(); // marker
        std::fs::write(old.join("config.json"), "{}").unwrap(); // marker
        std::fs::write(old.join("imports").join("absorbed.md"), "x").unwrap();

        // A look-alike that is NOT a memory folder (only config.json, no store.db):
        // its .md must still be collected.
        let notmem = d.join("docs");
        std::fs::create_dir_all(&notmem).unwrap();
        std::fs::write(notmem.join("config.json"), "{}").unwrap();
        std::fs::write(notmem.join("guide.md"), "x").unwrap();

        let exclude = d.join(".newmem"); // doesn't exist yet; irrelevant here
        let mut got: Vec<String> = collect_md_files(d, &exclude)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        // .oldmem pruned (absorbed.md gone); real.md and docs/guide.md kept.
        assert_eq!(got, vec!["guide.md", "real.md"]);
    }

    #[test]
    fn collect_md_files_prunes_split_layout_memory_folder() {
        // Regression: a SPLIT-layout data folder has store.db + notes/ + imports/ but
        // NO config.json inside it (config lives at the parent root). The pruner must
        // still recognize it, or its imports/ copies get re-absorbed as user docs.
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("real.md"), "x").unwrap();
        // A pre-existing split-layout data folder (store.db + imports/, no config.json).
        let mem = d.join("memory");
        std::fs::create_dir_all(mem.join("imports")).unwrap();
        std::fs::write(mem.join("store.db"), "").unwrap();
        std::fs::write(mem.join("imports").join("absorbed.md"), "x").unwrap();

        let got: Vec<String> = collect_md_files(d, &no_exclude())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // absorbed.md is NOT re-collected; only the genuine user doc remains.
        assert_eq!(got, vec!["real.md"]);
    }

    #[test]
    fn is_memory_folder_recognizes_both_layouts() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Split: store.db + imports/ (no config.json).
        let split = d.join("split");
        std::fs::create_dir_all(split.join("imports")).unwrap();
        std::fs::write(split.join("store.db"), "").unwrap();
        assert!(is_memory_folder(&split));
        // Legacy: store.db + config.json.
        let legacy = d.join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("store.db"), "").unwrap();
        std::fs::write(legacy.join("config.json"), "{}").unwrap();
        assert!(is_memory_folder(&legacy));
        // Not a memory folder: a stray store.db with neither imports/ nor config.json.
        let stray = d.join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("store.db"), "").unwrap();
        assert!(!is_memory_folder(&stray));
    }

    #[test]
    fn ensure_binary_ignored_appends_once_and_preserves_existing() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // No .gitignore yet → created with the cm-ignore block.
        ensure_binary_ignored(d);
        let gi = std::fs::read_to_string(d.join(".gitignore")).unwrap();
        assert!(gi.contains("cm.exe") && gi.contains("cm\n"));
        // Idempotent: a second call doesn't duplicate the block.
        ensure_binary_ignored(d);
        let gi2 = std::fs::read_to_string(d.join(".gitignore")).unwrap();
        assert_eq!(gi, gi2);
        assert_eq!(gi2.matches("cm.exe").count(), 1);

        // An existing .gitignore with the user's own rules is preserved + appended to.
        let d2 = tmp.path().join("p2");
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d2.join(".gitignore"), "/build\n").unwrap();
        ensure_binary_ignored(&d2);
        let gi3 = std::fs::read_to_string(d2.join(".gitignore")).unwrap();
        assert!(gi3.starts_with("/build\n")); // user rule kept
        assert!(gi3.contains("cm.exe"));

        // A user who already ignores cm.exe themselves → no-op.
        let d3 = tmp.path().join("p3");
        std::fs::create_dir_all(&d3).unwrap();
        std::fs::write(d3.join(".gitignore"), "cm.exe\n").unwrap();
        ensure_binary_ignored(&d3);
        let gi4 = std::fs::read_to_string(d3.join(".gitignore")).unwrap();
        assert_eq!(gi4, "cm.exe\n");
    }

    #[test]
    fn collect_md_files_missing_dir_is_empty() {
        assert!(collect_md_files(Path::new("/no/such/dir/here"), &no_exclude()).is_empty());
    }

    #[test]
    fn collect_md_files_skips_root_entry_points_but_keeps_nested() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Root entry-point files: wired, not absorbed → must be skipped here.
        std::fs::write(d.join("README.md"), "x").unwrap();
        std::fs::write(d.join("CLAUDE.md"), "x").unwrap();
        std::fs::write(d.join("AGENTS.md"), "x").unwrap();
        std::fs::write(d.join("readme.markdown"), "x").unwrap(); // case/ext-insensitive
                                                                 // A normal root doc that is NOT an entry point → kept.
        std::fs::write(d.join("notes.md"), "x").unwrap();
        // The SAME names one level down are real docs → kept (skip is root-only).
        let docs = d.join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("README.md"), "x").unwrap();
        std::fs::write(docs.join("guide.md"), "x").unwrap();

        let mut got: Vec<String> = collect_md_files(d, &no_exclude())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        // Root README/CLAUDE/AGENTS gone; notes.md kept; nested ones kept.
        assert_eq!(got, vec!["README.md", "guide.md", "notes.md"]);
    }

    #[test]
    fn is_root_entry_point_matches_readme_and_wired_names() {
        for yes in [
            "README.md",
            "readme.md",
            "README.markdown",
            "CLAUDE.md",
            "agents.md",
        ] {
            assert!(is_root_entry_point(yes), "{yes:?} should be an entry point");
        }
        for no in ["notes.md", "guide.md", "readme.txt", "claude.txt"] {
            assert!(!is_root_entry_point(no), "{no:?} should NOT be");
        }
    }

    #[test]
    fn collect_docs_from_args_takes_dirs_and_files_keeps_root_readme() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // An explicit dir: everything inside it is wanted, INCLUDING its README
        // (no root-entry-point skip — the user chose the folder).
        let manual = d.join("manual");
        std::fs::create_dir(&manual).unwrap();
        std::fs::write(manual.join("README.md"), "x").unwrap();
        std::fs::write(manual.join("ch1.md"), "x").unwrap();
        std::fs::write(manual.join("ignore.txt"), "x").unwrap(); // not md
                                                                 // A single explicit file elsewhere.
        std::fs::write(d.join("loose.md"), "x").unwrap();

        let arg = format!(
            "{}, {}",
            manual.to_string_lossy(),
            d.join("loose.md").to_string_lossy()
        );
        let mut got: Vec<String> = collect_docs_from_args(&arg, &no_exclude())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["README.md", "ch1.md", "loose.md"]);
    }

    #[test]
    fn collect_docs_from_args_skips_missing_and_empty_entries() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("real.md"), "x").unwrap();
        // Mix a real file, an empty entry, and a non-existent path.
        let arg = format!(
            " {} ,, /no/such/path.md",
            d.join("real.md").to_string_lossy()
        );
        let got = collect_docs_from_args(&arg, &no_exclude());
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("real.md"));
    }

    #[test]
    fn docs_arg_has_entry_detects_empty_values() {
        // These must be treated as "no --docs" → caller falls through to auto-scan.
        for empty in ["", "   ", ",", " , ", ",,,"] {
            assert!(!docs_arg_has_entry(empty), "{empty:?} should be empty");
        }
        // These carry a real path.
        for nonempty in ["docs", " docs ", "docs,notes/spec.md", ",docs,"] {
            assert!(
                docs_arg_has_entry(nonempty),
                "{nonempty:?} should have an entry"
            );
        }
    }

    #[test]
    fn doc_folder_summary_groups_by_relative_folder() {
        let root = Path::new("/proj");
        let files = [
            PathBuf::from("/proj/notes.md"),      // root → "."
            PathBuf::from("/proj/docs/a.md"),     // "docs"
            PathBuf::from("/proj/docs/b.md"),     // "docs"
            PathBuf::from("/proj/docs/sub/c.md"), // "docs/sub"
        ];
        let got = doc_folder_summary(root, &files);
        assert_eq!(
            got,
            vec![
                (".".to_string(), 1),
                ("docs".to_string(), 2),
                ("docs/sub".to_string(), 1),
            ]
        );
    }

    /// Build init args targeting `tmp`, returning the produced folder path too.
    fn run_init(tmp: &TempDir, extra: &[&str]) -> (Result<()>, PathBuf) {
        let mut args = vec!["init", tmp.path().to_str().unwrap()];
        args.extend_from_slice(extra);
        let name = extra
            .windows(2)
            .find(|w| w[0] == "--name")
            .map(|w| w[1])
            .unwrap_or("memory");
        (run(&parsed(&args)), tmp.path().join(name))
    }

    #[test]
    fn resolve_target_defaults_to_cwd_else_positional() {
        let cwd = Path::new("/fake/project/root");
        // No positional → the working directory (so a bare `cm init` lands
        // `memory/` in the project root the command was run from).
        assert_eq!(resolve_target(&parsed(&["init"]), cwd), cwd);
        // An explicit path still wins over the default.
        assert_eq!(
            resolve_target(&parsed(&["init", "./docs"]), cwd),
            PathBuf::from("./docs")
        );
    }

    /// With the `code` feature, `init --code` indexes the project tree into the
    /// code graph; a source file at the target should produce symbols.
    #[cfg(feature = "code")]
    #[test]
    fn init_code_indexes_source_tree() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn helper() {}\nfn main() { helper() }\n",
        )
        .unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--code"]);
        res.unwrap();
        let store = Store::open(&folder.join("store.db")).unwrap();
        // The source file was indexed; its symbols are in the code graph.
        let defs = store.code_symbols_by_name("helper", false).unwrap();
        assert!(!defs.is_empty(), "init --code should have indexed helper");
        assert_eq!(defs[0].path, "lib.rs");
        // ...and the memory folder itself was NOT indexed as project source.
        assert!(store.code_symbols_in("m/store.db").unwrap().is_empty());
    }

    /// `init --code` must NEVER fail the scaffold — without the feature it warns and
    /// carries on; with it, an empty/source-less tree just indexes nothing. Either
    /// way the store is created. (Feature-agnostic: holds in both build configs.)
    #[test]
    fn init_code_still_scaffolds_when_nothing_to_index() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--code"]);
        res.unwrap(); // init succeeded regardless of the `code` feature
        assert!(folder.join("store.db").exists());
    }

    #[test]
    fn init_creates_scaffold_local() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        // Split layout: config.json at the ROOT (next to the binary); data dirs in
        // the `m/` data folder.
        let root = tmp.path();
        assert!(root.join("config.json").exists());
        assert!(Config::load(&root.join("config.json")).is_ok());
        assert!(folder.join("store.db").exists());
        assert!(folder.join("models").is_dir());
        assert!(folder.join("notes").is_dir()); // source of truth: notes
        assert!(folder.join("imports").is_dir()); // source of truth: imports
                                                  // config records the link to its data folder.
        assert_eq!(
            Config::load(&root.join("config.json")).unwrap().data_dir,
            "m"
        );
        // The data folder's .gitignore ignores the derived index + weights, keeps the
        // md truth (notes/, imports/).
        let gi = std::fs::read_to_string(folder.join(".gitignore")).unwrap();
        assert!(gi.contains("store.db") && gi.contains("models/"));
        assert!(!gi.contains("notes/") && !gi.contains("imports/"));
    }

    #[test]
    fn init_writes_embedder_signature_for_local() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        let store = Store::open(&folder.join("store.db")).unwrap();
        assert_eq!(
            store.meta_get("embedder_signature").unwrap().as_deref(),
            Some("local:hash-ngram-v1:384"),
        );
        assert!(store
            .recent_logs(10)
            .unwrap()
            .iter()
            .any(|l| l.op == "init"));
    }

    #[test]
    fn init_repairs_partial_layout_keeping_user_files() {
        // A pre-existing but EMPTY data folder is no longer "already initialized": it's
        // partial, so init completes it (scaffolds the store + dirs) WITHOUT clobbering
        // anything the user put there. This replaces the old "empty folder → no-op".
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join("m");
        std::fs::create_dir_all(&folder).unwrap();
        let sentinel = folder.join("sentinel.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert!(sentinel.exists()); // user file untouched
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        // ...and the missing pieces were filled in.
        assert!(folder.join("store.db").exists());
        assert!(folder.join("notes").is_dir());
        assert!(tmp.path().join("config.json").exists());
    }

    #[test]
    fn init_completes_when_only_store_missing() {
        // Run init once to get a full layout, delete just the derived store.db, and
        // re-init: the store is re-created, the user-edited config.json is KEPT verbatim.
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        // Mutate the config so we can prove a repair adopts it rather than overwriting.
        let config_path = tmp.path().join("config.json");
        let mut cfg = Config::load(&config_path).unwrap();
        cfg.embedding.model = "user-tuned-model".to_string();
        cfg.save(&config_path).unwrap();
        std::fs::remove_file(folder.join("store.db")).unwrap();

        let (res2, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res2.unwrap();
        assert!(folder.join("store.db").exists()); // re-created
                                                   // The user's edited config survived the repair (not reset to defaults).
        assert_eq!(
            Config::load(&config_path).unwrap().embedding.model,
            "user-tuned-model"
        );
    }

    #[test]
    fn init_rebuilds_document_index_after_store_loss() {
        // The defect a live test on a real project caught: deleting the derived store.db
        // and re-running init re-created an EMPTY store and re-indexed only code, leaving
        // `recall` blank until a manual `cm reindex`. The repair must rebuild the doc
        // index from the md truth in imports/ automatically.
        let tmp = TempDir::new().unwrap();
        // Install with one doc so imports/ holds md truth.
        let docs = tmp.path().join("d");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("spec.md"), "# Spec\nbody about widgets").unwrap();
        let docs_arg = docs.to_str().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code", "--docs", docs_arg]);
        res.unwrap();
        let chunks_before = Store::open(&folder.join("store.db"))
            .unwrap()
            .count_notes_kind("chunk")
            .unwrap();
        assert!(chunks_before > 0, "doc imported as chunk(s)");

        // Lose the derived index, then repair via a bare re-init.
        std::fs::remove_file(folder.join("store.db")).unwrap();
        let (res2, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res2.unwrap();

        // The store is back AND repopulated from imports/ — no manual reindex needed.
        let chunks_after = Store::open(&folder.join("store.db"))
            .unwrap()
            .count_notes_kind("chunk")
            .unwrap();
        assert_eq!(
            chunks_after, chunks_before,
            "repair rebuilt the document index from md truth"
        );
    }

    #[test]
    fn init_fresh_install_does_not_reindex_documents() {
        // The repair-reindex must NOT fire on a fresh install (empty notes/ + imports/):
        // there's nothing to rebuild, so has_md_truth gates it off.
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join("m");
        // No docs imported → imports/ and notes/ are empty after init.
        let (res, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert!(!has_md_truth(&folder), "fresh install has no md truth");
    }

    #[test]
    fn init_with_docs_imports_into_complete_layout() {
        // A complete layout + an explicit --docs <paths> must NOT short-circuit: the
        // named docs are imported into the existing store (the fix for "--docs ignored
        // on a ready layout"). Prove it by importing into a layout that's already full.
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        // The first init imported nothing (no docs), so the store starts doc-free.
        let before = read_manifest(&folder).docs.len();
        assert_eq!(before, 0);

        // A doc to import, named explicitly via an absolute --docs path.
        let docs = tmp.path().join("extra");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("late.md"), "# Late\nadded after init").unwrap();
        let docs_arg = docs.to_str().unwrap();
        let (res2, _) = run_init(&tmp, &["--name", "m", "--no-code", "--docs", docs_arg]);
        res2.unwrap();

        // The doc was imported into the already-complete store: manifest grew and the
        // imports/ copy exists (rather than an already_exists no-op).
        let after = read_manifest(&folder);
        assert_eq!(
            after.docs.len(),
            1,
            "explicit --docs imported into a full layout"
        );
        assert_eq!(after.docs[0].import_copy, "late.md");
        assert!(folder.join("imports").join("late.md").exists());
    }

    #[test]
    fn init_reports_already_exists_when_fully_complete() {
        // A full init followed by a second identical init is a true no-op: the second
        // run detects a complete layout and returns the already_exists status. Use
        // --no-code so the layout is deterministically complete (the code index isn't
        // part of the completeness check).
        let tmp = TempDir::new().unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        // Second run must succeed and must NOT have re-created/duplicated anything; the
        // key signal is the wired pointer block still appears exactly once.
        let (res2, _) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res2.unwrap();
        let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert_eq!(agents.matches(WIRE_BEGIN).count(), 1);
    }

    #[test]
    fn layout_state_probe_and_completeness() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let data = target.join("m");
        let config = target.join("config.json");
        let exe = if cfg!(windows) { "cm.exe" } else { "cm" };

        // Nothing on disk → every flag false, not present, not complete.
        let empty = LayoutState::probe(target, &data, &config);
        assert!(!empty.any_present() && !empty.is_complete(target, "./cm"));

        // A LONE config.json (the after-state of `deinit`, which leaves it) is NOT a
        // prior init's data: any_present() must stay false, so re-init reads as a fresh
        // "created", not a "repaired".
        std::fs::write(&config, "{}").unwrap();
        let cfg_only = LayoutState::probe(target, &data, &config);
        assert!(cfg_only.config, "config probed");
        assert!(
            !cfg_only.any_present(),
            "a lone config.json is not 'memory data already here'"
        );
        std::fs::remove_file(&config).unwrap();

        // Lay out a fully-complete layout by hand and probe it.
        std::fs::create_dir_all(data.join("notes")).unwrap();
        std::fs::create_dir_all(data.join("imports")).unwrap();
        std::fs::create_dir_all(data.join("models")).unwrap();
        std::fs::write(data.join("store.db"), "").unwrap();
        std::fs::write(data.join(".gitignore"), "store.db\n").unwrap();
        std::fs::write(&config, "{}").unwrap();
        std::fs::write(target.join(exe), "binary").unwrap();
        std::fs::write(
            target.join(".gitignore"),
            format!("{GITIGNORE_MARKER}\ncm\ncm.exe\n"),
        )
        .unwrap();
        std::fs::write(target.join(CM_GUIDE_NAME), "guide").unwrap();
        // An instruction file carrying the CURRENT pointer for this exe display.
        let exe_display = "./cm";
        std::fs::write(
            target.join("AGENTS.md"),
            format!("{AGENTS_HEADER}\n{}\n", entry_point_block(exe_display)),
        )
        .unwrap();

        let full = LayoutState::probe(target, &data, &config);
        assert!(full.any_present());
        assert!(full.config && full.store && full.dirs && full.data_gitignore);
        assert!(full.exe && full.root_ignored && full.guide);
        assert!(
            full.is_complete(target, exe_display),
            "all parts present → complete"
        );

        // A STALE pointer (different exe display) makes it incomplete → re-wire path.
        assert!(
            !full.is_complete(target, "./other/cm"),
            "stale pointer must read as incomplete"
        );

        // Remove one core piece (store.db) → present-but-incomplete (the repair path).
        std::fs::remove_file(data.join("store.db")).unwrap();
        let partial = LayoutState::probe(target, &data, &config);
        assert!(partial.any_present() && !partial.is_complete(target, exe_display));
    }

    #[test]
    fn init_bad_dimension_errors() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--dimension", "abc"]);
        assert!(res.unwrap_err().msg.contains("must be a number"));
        assert!(!folder.exists()); // folder not created on early error
    }

    #[test]
    fn init_default_name_is_memory() {
        let tmp = TempDir::new().unwrap();
        let (res, _) = run_init(&tmp, &[]);
        res.unwrap();
        assert!(tmp.path().join("memory").join("store.db").exists());
    }

    #[test]
    fn init_custom_name_and_flags_in_config() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(
            &tmp,
            &[
                "--name",
                "m",
                "--model",
                "custom-model",
                "--dimension",
                "256",
            ],
        );
        res.unwrap();
        let _ = folder; // data folder; config now lives at the root
        let cfg = Config::load(&tmp.path().join("config.json")).unwrap();
        assert_eq!(cfg.name, "m");
        assert_eq!(cfg.data_dir, "m");
        assert_eq!(cfg.embedding.model, "custom-model");
        assert_eq!(cfg.embedding.dimension, 256);
    }

    // ---- entry-point wiring -------------------------------------------

    #[test]
    fn wire_entry_points_appends_block_to_known_files_only() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Two known entry points (one lowercase-ish via case-exact name) and a
        // nested one, plus an unrelated file that must be left alone.
        std::fs::write(d.join("CLAUDE.md"), "existing rules\n").unwrap();
        std::fs::write(d.join("AGENTS.md"), "agent rules").unwrap(); // no trailing newline
        std::fs::create_dir_all(d.join(".github")).unwrap();
        std::fs::write(d.join(".github").join("copilot-instructions.md"), "copilot").unwrap();
        std::fs::write(d.join("README.md"), "just a readme\n").unwrap(); // NOT an entry point

        wire_entry_points(d, "cm.exe");

        let claude = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("existing rules\n")); // original kept
        assert!(claude.contains(WIRE_BEGIN) && claude.contains(WIRE_END));
        assert!(claude.contains("recall") && claude.contains("remember"));
        // The block also points the model at the code map (map --query/--uses/...).
        assert!(claude.contains("map --query") && claude.contains("map --uses"));

        // No trailing newline original still gets a clean blank-line separation.
        let agents = std::fs::read_to_string(d.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("agent rules\n\n"));
        assert!(agents.contains(WIRE_BEGIN));

        let copilot =
            std::fs::read_to_string(d.join(".github").join("copilot-instructions.md")).unwrap();
        assert!(copilot.contains(WIRE_BEGIN)); // nested path wired

        let readme = std::fs::read_to_string(d.join("README.md")).unwrap();
        assert!(!readme.contains(WIRE_BEGIN)); // untouched
    }

    #[test]
    fn wire_entry_points_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("CLAUDE.md"), "rules\n").unwrap();
        wire_entry_points(d, "cm.exe");
        let once = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        // Same exe path twice -> second run is a byte-for-byte no-op: exactly one
        // block, content unchanged (no extra append, no rewrite).
        wire_entry_points(d, "cm.exe");
        let twice = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice.matches(WIRE_BEGIN).count(), 1);
    }

    #[test]
    fn wire_entry_points_refreshes_stale_path_in_place() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("CLAUDE.md"), "rules\n").unwrap();
        // First init under one name, then a re-init that lands the binary elsewhere
        // (the "init with a different --name" case).
        wire_entry_points(d, ".memory/cm.exe");
        wire_entry_points(d, ".memory2/cm.exe");
        let out = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        // Still exactly one block, but it now points at the NEW path, not the old.
        assert_eq!(out.matches(WIRE_BEGIN).count(), 1);
        assert_eq!(out.matches(WIRE_END).count(), 1);
        assert!(out.contains(".memory2/cm.exe"));
        assert!(!out.contains(".memory/cm.exe")); // stale path gone
        assert!(out.starts_with("rules\n")); // original content preserved
    }

    #[test]
    fn wire_then_unwire_roundtrips_to_original() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Cover both separator cases: file ending in a newline, and one that doesn't.
        for original in ["rules\n", "no trailing newline"] {
            std::fs::write(d.join("CLAUDE.md"), original).unwrap();
            wire_entry_points(d, ".memory/cm.exe");
            let wired = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
            assert!(wired.contains(WIRE_BEGIN)); // it was wired
            let changed = unwire_entry_points(d);
            assert_eq!(changed, 1);
            let back = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
            // A file with no trailing newline gets one back (we always terminate
            // restored content); otherwise it's byte-identical.
            let expected = if original.ends_with('\n') {
                original.to_string()
            } else {
                format!("{original}\n")
            };
            assert_eq!(back, expected, "roundtrip failed for {original:?}");
        }
    }

    #[test]
    fn strip_block_handles_no_block_and_block_only_file() {
        // No markers -> None (nothing to strip).
        assert!(strip_block("just docs, no markers").is_none());
        // A file that is ONLY the block (no prior content) strips to empty.
        let only = format!("{WIRE_BEGIN}\nbody\n{WIRE_END}\n");
        assert_eq!(strip_block(&only).unwrap(), "");
        // Malformed (BEGIN without END) -> None, left alone.
        assert!(strip_block(&format!("x\n{WIRE_BEGIN}\nno end")).is_none());
    }

    #[test]
    fn unwire_entry_points_skips_files_without_block() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("CLAUDE.md"), "plain rules, never wired\n").unwrap();
        let changed = unwire_entry_points(d);
        assert_eq!(changed, 0);
        // Untouched.
        assert_eq!(
            std::fs::read_to_string(d.join("CLAUDE.md")).unwrap(),
            "plain rules, never wired\n"
        );
    }

    #[test]
    fn replace_block_swaps_only_when_different() {
        let old = entry_point_block("OLD/cm.exe");
        let new = entry_point_block("NEW/cm.exe");
        let text = format!("intro\n\n{old}\ntail\n");
        // Different block -> region swapped, surrounding text kept verbatim.
        let got = replace_block(&text, &new).expect("should replace");
        assert!(got.contains("NEW/cm.exe") && !got.contains("OLD/cm.exe"));
        assert!(got.starts_with("intro\n\n") && got.ends_with("\ntail\n"));
        // Identical block -> None (nothing to do).
        assert!(replace_block(&got, &new).is_none());
        // No block at all -> None.
        assert!(replace_block("just text, no markers", &new).is_none());
        // Malformed (BEGIN without END) -> left alone (None).
        let malformed = format!("x\n{WIRE_BEGIN}\nbody but no end");
        assert!(replace_block(&malformed, &new).is_none());
    }

    #[test]
    fn wire_entry_points_never_creates_missing_files() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // No entry-point files exist; wiring must not create any.
        wire_entry_points(d, "cm.exe");
        assert!(!d.join("CLAUDE.md").exists());
        assert!(!d.join("AGENTS.md").exists());
    }

    #[test]
    fn init_wires_existing_claude_md() {
        let tmp = TempDir::new().unwrap();
        // A CLAUDE.md sitting in the target before init runs.
        std::fs::write(tmp.path().join("CLAUDE.md"), "project rules\n").unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("project rules\n"));
        assert!(claude.contains(WIRE_BEGIN));
        assert!(claude.contains("recall"));
    }

    #[test]
    fn init_creates_agents_md_when_no_instruction_file() {
        // No CLAUDE/AGENTS/… in the target → init creates AGENTS.md with the header
        // and a pointer block, so a model has an entry point.
        let tmp = TempDir::new().unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert!(agents.starts_with(AGENTS_HEADER));
        assert!(agents.contains(WIRE_BEGIN) && agents.contains(WIRE_END));
        assert!(agents.contains("recall") && agents.contains("map --query"));
    }

    #[test]
    fn init_creates_cm_guide_with_examples_and_records_it() {
        // init drops a standalone CM_GUIDE.md at the root and flags it in the manifest.
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        let guide = tmp.path().join("CM_GUIDE.md");
        assert!(guide.exists(), "init should have created CM_GUIDE.md");
        let text = std::fs::read_to_string(&guide).unwrap();
        // It teaches both loops and shows real output shapes (so no `cm help` needed).
        assert!(text.contains("recall") && text.contains("remember"));
        assert!(text.contains("map --query") && text.contains("\"signature\""));
        assert!(text.contains("STDIN")); // the load-bearing stdin rule
        assert!(read_manifest(&folder).created_cm_guide);
    }

    #[test]
    fn init_does_not_clobber_existing_cm_guide() {
        // A user's own CM_GUIDE.md is left untouched, and the manifest does NOT flag it
        // (so deinit won't delete the user's file).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CM_GUIDE.md"), "my own guide\n").unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("CM_GUIDE.md")).unwrap(),
            "my own guide\n"
        );
        assert!(!read_manifest(&folder).created_cm_guide);
    }

    #[test]
    fn entry_point_block_shows_output_shapes_and_links_guide() {
        // The wired pointer must be self-sufficient: flag NAMES plus the JSON they
        // return, and a link to the standalone guide instead of forcing `cm help`.
        let block = entry_point_block("cm");
        assert!(block.contains("recall") && block.contains("remember"));
        for mode in ["--query", "--uses", "--calls", "--defines", "--like"] {
            assert!(block.contains(mode), "block should mention map {mode}");
        }
        // Output-shape hints so a model knows what comes back without a help call.
        assert!(block.contains("signature") && block.contains("def_line"));
        assert!(block.contains("resolved"));
        assert!(block.contains("CM_GUIDE.md"));
    }

    #[test]
    fn init_does_not_create_agents_md_when_claude_present() {
        // An existing instruction file is wired; we must NOT also spawn AGENTS.md.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "rules\n").unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        assert!(tmp.path().join("CLAUDE.md").exists());
        assert!(
            !tmp.path().join("AGENTS.md").exists(),
            "AGENTS.md must not be created when another instruction file exists"
        );
    }

    #[test]
    fn unwire_removes_cm_created_agents_md_but_keeps_user_file() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // A cm-created AGENTS.md (header + block) round-trips to NOTHING on unwire.
        let block = entry_point_block("X/cm.exe");
        std::fs::write(d.join("AGENTS.md"), format!("{AGENTS_HEADER}\n{block}\n")).unwrap();
        // A user's own CLAUDE.md (real content + block) keeps its content.
        std::fs::write(d.join("CLAUDE.md"), format!("my rules\n\n{block}\n")).unwrap();

        let changed = unwire_entry_points(d);
        assert_eq!(changed, 2);
        assert!(
            !d.join("AGENTS.md").exists(),
            "cm-created AGENTS.md should be removed wholesale"
        );
        let claude = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        assert_eq!(
            claude, "my rules\n",
            "user content preserved, block stripped"
        );
    }

    // ---- rollback manifest --------------------------------------------

    /// Read the manifest init wrote into a data folder, failing the test if absent
    /// or unparsable.
    fn read_manifest(folder: &Path) -> InitManifest {
        let text = std::fs::read_to_string(folder.join(MANIFEST_NAME))
            .expect("init should have written .init-manifest.json");
        serde_json::from_str(&text).expect("manifest should parse")
    }

    #[test]
    fn init_writes_manifest_with_imported_doc() {
        let tmp = TempDir::new().unwrap();
        // A doc under docs/ that --docs imports (so no interactive scan prompt).
        let docs = tmp.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("spec.md"), "# Spec\nbody text here").unwrap();
        // Pass the docs dir as an ABSOLUTE path: --docs resolves relative entries
        // against the process cwd (the repo root in tests), not the target.
        let docs_arg = docs.to_str().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--docs", docs_arg, "--no-code"]);
        res.unwrap();

        let manifest = read_manifest(&folder);
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.data_dir, "m");
        // The imported doc is recorded with its original (target-relative) path and
        // the imports/ copy name; deletion is prompt-gated and EOF declines it here.
        assert_eq!(manifest.docs.len(), 1, "one doc imported");
        let rec = &manifest.docs[0];
        // The doc sits under the target, so the path is relativized to it.
        assert_eq!(rec.orig_path, "docs/spec.md");
        assert_eq!(rec.import_copy, "spec.md");
        assert!(!rec.deleted, "delete prompt declined on piped stdin");
    }

    #[test]
    fn init_manifest_records_created_agents_md() {
        // No instruction file → init creates AGENTS.md → manifest flags it.
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert!(tmp.path().join("AGENTS.md").exists());
        assert!(read_manifest(&folder).created_agents_md);
    }

    #[test]
    fn init_manifest_no_created_agents_md_when_instruction_file_exists() {
        // A pre-existing CLAUDE.md is wired, not created → flag stays false.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "rules\n").unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert!(!read_manifest(&folder).created_agents_md);
    }

    #[test]
    fn init_manifest_gitignore_created_when_none_before() {
        // No root .gitignore before init → init creates one → manifest says
        // modified && !existed (deinit will DELETE it).
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        let gi = read_manifest(&folder).root_gitignore;
        assert!(gi.modified && !gi.existed);
        assert!(tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn init_manifest_gitignore_snapshots_user_content() {
        // A user .gitignore with no cm.exe rule → init appends its block → manifest
        // records modified && existed with the EXACT pre-init bytes (for restore).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "/build\n# mine\n").unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        let gi = read_manifest(&folder).root_gitignore;
        assert!(gi.modified && gi.existed);
        assert_eq!(gi.original, "/build\n# mine\n");
    }

    #[test]
    fn init_manifest_gitignore_untouched_when_already_ignoring_cm() {
        // The user already ignores cm.exe → init changes nothing → manifest says
        // not modified (deinit leaves the file alone).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "cm.exe\n").unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--no-code"]);
        res.unwrap();
        assert!(!read_manifest(&folder).root_gitignore.modified);
    }

    #[test]
    fn exe_command_display_is_relative_runnable_under_target() {
        let sep = std::path::MAIN_SEPARATOR;
        let target = Path::new("/proj");
        // Binary at the root → `./cm` (platform separator), short and runnable.
        let exe = target.join("cm.exe");
        assert_eq!(exe_command_display(target, &exe), format!(".{sep}cm.exe"));
        // A binary OUTSIDE the target falls back to its full display path.
        let outside = Path::new("/elsewhere/cm.exe");
        assert_eq!(exe_command_display(target, outside), display_path(outside));
    }

    #[test]
    fn rel_or_raw_relativizes_under_target() {
        let target = Path::new("/proj");
        assert_eq!(
            rel_or_raw(target, Path::new("/proj/docs/a.md")),
            "docs/a.md"
        );
        // A path outside the target is kept raw (forward-slashed).
        let outside = rel_or_raw(target, Path::new("/other/b.md"));
        assert_eq!(outside, "/other/b.md");
    }

    #[cfg(feature = "api")]
    #[test]
    fn init_api_provider_without_endpoint_skips_signature() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--provider", "api"]);
        res.unwrap();
        let store = Store::open(&folder.join("store.db")).unwrap();
        // Embedder can't be built (api without endpoint) -> no signature written...
        assert_eq!(store.meta_get("embedder_signature").unwrap(), None);
        // ...but the store is valid and init was logged.
        assert!(store
            .recent_logs(10)
            .unwrap()
            .iter()
            .any(|l| l.op == "init"));
    }
}
