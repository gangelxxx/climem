//! `doctor`: a read-only project health check + statistics + (under `--fix`) safe,
//! delegated repairs. It is the read-side mirror of `init`'s self-heal: it diagnoses
//! exactly what `init`/`reindex` would fix, but never reimplements repair — every fix
//! composes an existing idempotent primitive (`commands::reindex`, `Store::create`,
//! `mkdir`, and the renamed-folder `config.data_dir` rewrite).
//!
//! The two scenarios it exists for: a deleted `store.db` (the derived index is
//! rebuildable from the md truth) and a renamed/moved data folder (config's
//! `data_dir` no longer resolves). It also surfaces the store invariants SQLite
//! doesn't enforce for us — the hand-maintained `notes`↔`notes_fts` sync, embedder
//! signature/dimension drift, store↔disk drift, and dangling-vs-resolved edges.
//!
//! Conventions honored (conventions.md): data is JSONL on stdout (`print_line`),
//! narration/warnings on stderr; it reads NOTHING from stdin; it is read-only except
//! under `--fix`; it never prints secrets (only the embedder provider/model/dim).

use crate::cli::Parsed;
use crate::commands::{self, Ctx};
use crate::config::{self, Config};
use crate::embed;
use crate::graph;
use crate::init;
use crate::output::print_line;
use crate::store::{ImportRow, Store};
use crate::util::Result;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Severity of a single check result. `Ok` is silent on stderr; the rest print a
/// one-line note. `Info` is for advisory states (no embeddings yet, scaffold not
/// fully wired) that aren't a problem on their own.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Status {
    Ok,
    Info,
    Warn,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Info => "info",
            Status::Warn => "warn",
            Status::Error => "error",
        }
    }
}

/// One check's outcome. `fix` is the exact command (or short instruction) a caller
/// can run; `fixable` marks the ones `cm doctor --fix` will apply itself.
#[derive(Debug)]
struct Finding {
    check: &'static str,
    status: Status,
    detail: Option<String>,
    fixable: bool,
    fix: Option<String>,
}

/// The full read-only diagnosis, kept separate from any printing or mutation so it's
/// unit-testable. `run` prints it and, under `--fix`, acts on the repair flags.
struct Report {
    findings: Vec<Finding>,
    stats: Vec<(&'static str, Value)>,
    /// A single sibling folder that looks like the renamed data dir (else `None`).
    renamed: Option<String>,
    /// Did config's `data_dir` resolve to an existing folder?
    data_resolved: bool,
    /// A wipe+rebuild is needed (signature/dimension drift, fts desync).
    need_reindex_all: bool,
    /// An incremental rebuild is needed (missing store, orphan/new files, stale edges).
    need_reindex: bool,
}

fn ok(check: &'static str) -> Finding {
    Finding {
        check,
        status: Status::Ok,
        detail: None,
        fixable: false,
        fix: None,
    }
}

fn finding(
    check: &'static str,
    status: Status,
    detail: impl Into<String>,
    fixable: bool,
    fix: Option<String>,
) -> Finding {
    Finding {
        check,
        status,
        detail: Some(detail.into()),
        fixable,
        fix,
    }
}

/// Tally of finding severities, for the summary line and the "N safe issues" hint.
#[derive(Default)]
struct Counts {
    errors: u32,
    warnings: u32,
    infos: u32,
    fixable: u32,
}

impl Counts {
    fn tally(&mut self, f: &Finding) {
        match f.status {
            Status::Error => self.errors += 1,
            Status::Warn => self.warnings += 1,
            Status::Info => self.infos += 1,
            Status::Ok => {}
        }
        if f.fixable {
            self.fixable += 1;
        }
    }
}

/// Entry point. Diagnose, then render either JSONL (default, machine-readable) or a
/// human report (`--text`); under `--fix` apply the safe repairs (create missing dirs
/// / rebuild the index, or rewrite a renamed `data_dir`). Always exits Ok: findings are
/// DATA, not a command failure — a caller reads the summary's `errors`/`warnings`.
pub fn run(p: &Parsed, ctx: &Ctx) -> Result<()> {
    let fix = p.has("fix");
    let text = p.has("text");
    let root = ctx
        .config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let data_folder = ctx
        .store_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.clone());
    let data_str = data_folder.to_string_lossy().into_owned();

    let report = diagnose(ctx, &root);
    let mut counts = Counts::default();
    for f in &report.findings {
        counts.tally(f);
    }

    if text {
        // Human report on stdout (a documented exception to JSONL-on-stdout, like
        // `config`/`export`/`help`): aligned checks + a formatted stats block.
        print!("{}", render_text(&report, &data_str));
    } else {
        eprintln!("── cm doctor ── {data_str}");
        // Findings → JSONL on stdout; a one-line note per non-ok finding on stderr.
        for f in &report.findings {
            print_line(&finding_json(f));
            if f.status != Status::Ok {
                let hint = f
                    .fix
                    .as_deref()
                    .map(|x| format!(" → {x}"))
                    .unwrap_or_default();
                let detail = f.detail.clone().unwrap_or_else(|| f.check.to_string());
                eprintln!("  {}: {} ({}){}", f.status.as_str(), detail, f.check, hint);
            }
        }
        for (k, v) in &report.stats {
            let mut o = serde_json::Map::new();
            o.insert("stat".into(), json!(k));
            o.insert("value".into(), v.clone());
            print_line(&Value::Object(o));
        }
    }

    // Repairs (only under --fix; report-only otherwise). Narration goes to stderr in
    // both modes; the machine `{"fixed":…}` line only in JSONL mode.
    let fixed = if fix {
        apply_fixes(&report, ctx, &data_folder, text)
    } else {
        0
    };

    if text {
        print!("{}", summary_text(&counts, fixed, fix));
    } else {
        print_line(&json!({
            "doctor": data_str,
            "errors": counts.errors,
            "warnings": counts.warnings,
            "info": counts.infos,
            "fixed": fixed,
        }));
        if counts.errors == 0 && counts.warnings == 0 {
            eprintln!("All checks passed.");
        } else {
            eprint!(
                "{} error(s), {} warning(s).",
                counts.errors, counts.warnings
            );
            if !fix && counts.fixable > 0 {
                eprint!(
                    " Run `cm doctor --fix` to repair {} safe issue(s).",
                    counts.fixable
                );
            }
            eprintln!();
        }
    }
    Ok(())
}

/// Apply the safe, delegated repairs (renamed-folder data_dir rewrite, or create
/// missing dirs + `reindex`). Narration is stderr; the machine `{"fixed":…}` line is
/// printed only in JSONL mode (`!text`). Returns how many repairs were applied. We
/// never write our own repair logic — every branch composes an existing primitive.
fn apply_fixes(report: &Report, ctx: &Ctx, data_folder: &Path, text: bool) -> u32 {
    let mut fixed = 0u32;
    if let Some(name) = &report.renamed {
        // The renamed-folder fix is terminal: once data_dir moves, the store resolves
        // under the new path, so a follow-up run finishes the rebuild.
        match rewrite_data_dir(&ctx.config_path, name) {
            Ok(()) => {
                if !text {
                    print_line(&json!({ "fixed": "data_dir_resolves", "data_dir": name }));
                }
                fixed += 1;
                eprintln!("  data_dir → {name}; re-run `cm doctor --fix` to finish rebuilding.");
            }
            Err(e) => eprintln!("  could not rewrite data_dir: {}", e.msg),
        }
    } else if report.data_resolved {
        for sub in ["notes", "imports", "models"] {
            let _ = std::fs::create_dir_all(data_folder.join(sub));
        }
        if report.need_reindex_all || report.need_reindex {
            let all = report.need_reindex_all;
            match run_reindex(ctx, all) {
                Ok(()) => {
                    fixed += 1;
                    eprintln!("  reindex{} done.", if all { " --all" } else { "" });
                }
                Err(e) => {
                    let hint = e
                        .hint
                        .as_deref()
                        .map(|h| format!(" ({h})"))
                        .unwrap_or_default();
                    eprintln!("  reindex failed: {}{}", e.msg, hint);
                }
            }
        } else {
            eprintln!("  nothing to auto-fix.");
        }
    } else {
        eprintln!("  cannot auto-fix without a resolvable data folder.");
    }
    fixed
}

/// Render the human report (`--text`): a header, an aligned list of checks (✓/•/⚠/✗
/// with the detail + fix for non-ok ones), and a formatted statistics block.
fn render_text(report: &Report, data_folder: &str) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(o, "cm doctor — {data_folder}\n");
    let _ = writeln!(o, "  Checks");
    for f in &report.findings {
        let mark = match f.status {
            Status::Ok => "✓",
            Status::Info => "•",
            Status::Warn => "⚠",
            Status::Error => "✗",
        };
        if f.status == Status::Ok {
            let _ = writeln!(o, "    {mark} {}", f.check);
        } else {
            let _ = writeln!(
                o,
                "    {mark} {}  {}",
                f.check,
                f.detail.as_deref().unwrap_or("")
            );
            if let Some(fx) = &f.fix {
                let _ = writeln!(o, "        → {fx}");
            }
        }
    }
    if !report.stats.is_empty() {
        let _ = writeln!(o, "\n  Statistics");
        const W: usize = 19;
        for (k, v) in &report.stats {
            let label = k.replace('_', " ");
            let dots = ".".repeat(W.saturating_sub(label.len()));
            let _ = writeln!(o, "    {label} {dots} {}", format_stat(k, v));
        }
    }
    o
}

/// The closing summary line for the human report.
fn summary_text(c: &Counts, fixed: u32, fix: bool) -> String {
    let mut s = format!(
        "\n  Summary: {} error(s), {} warning(s), {} info",
        c.errors, c.warnings, c.infos
    );
    if fixed > 0 {
        s.push_str(&format!(", {fixed} fixed"));
    }
    if c.errors == 0 && c.warnings == 0 {
        s.push_str(" — all checks passed");
    } else if !fix && c.fixable > 0 {
        s.push_str(&format!(
            " — run `cm doctor --fix` to repair {} safe issue(s)",
            c.fixable
        ));
    }
    s.push('\n');
    s
}

/// Format one statistic's value for the human report (numbers as-is; the few
/// structured stats rendered compactly; byte counts humanized).
fn format_stat(key: &str, v: &Value) -> String {
    match key {
        "config" => format!(
            "{} / {} / dim {} · data_dir={} · v{}",
            v["provider"].as_str().unwrap_or("?"),
            v["model"].as_str().unwrap_or("?"),
            v["dimension"].as_i64().unwrap_or(0),
            v["data_dir"].as_str().unwrap_or("?"),
            v["version"].as_i64().unwrap_or(0),
        ),
        "code" => format!(
            "{} symbols in {} files · {} edges",
            v["symbols"].as_i64().unwrap_or(0),
            v["files"].as_i64().unwrap_or(0),
            v["edges"].as_i64().unwrap_or(0),
        ),
        "embedder" => {
            let cfg = v["config"].as_str().unwrap_or("?");
            match v["stored"].as_str() {
                Some(s) if s == cfg => format!("{cfg} (stored matches)"),
                Some(s) => format!("{cfg} (stored {s} — DRIFT)"),
                None => format!("{cfg} (none stored yet)"),
            }
        }
        "last_activity" => format!(
            "{} · {}",
            v["op"].as_str().unwrap_or("?"),
            v["at"].as_str().unwrap_or("?"),
        ),
        "store_db" => human_bytes(v["bytes"].as_u64().unwrap_or(0)),
        _ => match v {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
    }
}

/// `4468736 → "4.3 MB"`. Pure; used only by the human report.
fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut f = b as f64;
    let mut i = 0;
    while f >= 1024.0 && i < UNITS.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{f:.1} {}", UNITS[i])
    }
}

fn finding_json(f: &Finding) -> Value {
    let mut o = json!({ "check": f.check, "status": f.status.as_str() });
    if let Some(d) = &f.detail {
        o["detail"] = json!(d);
    }
    if f.fixable {
        o["fixable"] = json!(true);
    }
    if let Some(fx) = &f.fix {
        o["fix"] = json!(fx);
    }
    o
}

/// The whole read-only diagnosis. `root` is the folder holding `config.json` and the
/// binary; the data folder is resolved from config's `data_dir`. Tolerates a missing
/// or broken config (reports it and stops), a missing/renamed data folder, and a
/// missing store.db (all the recoverable states it exists to catch).
fn diagnose(ctx: &Ctx, root: &Path) -> Report {
    let mut f: Vec<Finding> = Vec::new();
    let mut stats: Vec<(&'static str, Value)> = Vec::new();

    // 1. config.json present + parses. Fatal: nothing else resolves without it, and we
    //    never auto-rewrite a user's tuned config (secrets' env-var names, weights).
    let cfg = match Config::load(&ctx.config_path) {
        Ok(c) => {
            f.push(ok("config_present"));
            c
        }
        Err(e) => {
            let (detail, fix) = if ctx.config_path.is_file() {
                (
                    e.msg,
                    "fix the JSON, or `cm init` (it adopts an existing config, never clobbers)",
                )
            } else {
                (
                    format!("config.json not found at {}", ctx.config_path.display()),
                    "cm init  (scaffold this project)",
                )
            };
            f.push(finding(
                "config_present",
                Status::Error,
                detail,
                false,
                Some(fix.into()),
            ));
            return Report {
                findings: f,
                stats,
                renamed: None,
                data_resolved: false,
                need_reindex_all: false,
                need_reindex: false,
            };
        }
    };
    stats.push((
        "config",
        json!({
            "provider": cfg.embedding.provider,
            "model": cfg.embedding.model,
            "dimension": cfg.embedding.dimension,
            "data_dir": cfg.data_dir,
            "version": cfg.version,
        }),
    ));

    // 2. data_dir resolves to an existing folder. The RENAMED-FOLDER scenario: config
    //    still says `memory` but the folder was renamed/moved, so the store is gone
    //    from under the recorded pointer.
    let data_path = commands::resolve_data_dir(root, &cfg.data_dir);
    let mut renamed = None;
    let data_resolved = data_path.is_dir();
    if data_resolved {
        f.push(ok("data_dir_resolves"));
    } else {
        renamed = find_renamed_data_dir(root, &data_path);
        match &renamed {
            Some(name) => f.push(finding(
                "data_dir_resolves",
                Status::Error,
                format!(
                    "config.data_dir '{}' is missing; sibling '{}' holds a store.db — folder renamed?",
                    cfg.data_dir, name
                ),
                true,
                Some(format!(
                    "cm doctor --fix  (sets data_dir = {name}), or cm config set data_dir {name}"
                )),
            )),
            None => f.push(finding(
                "data_dir_resolves",
                Status::Error,
                format!(
                    "config.data_dir '{}' resolves to a missing folder {}",
                    cfg.data_dir,
                    data_path.display()
                ),
                false,
                Some("restore the data folder, or `cm init` to re-scaffold".into()),
            )),
        }
        // Without a resolvable data folder there is nothing more to inspect.
        return Report {
            findings: f,
            stats,
            renamed,
            data_resolved,
            need_reindex_all: false,
            need_reindex: false,
        };
    }

    // 3. Agent wiring — do the instruction files point at an EXISTING cm binary?
    //    The wired pointers tell the agent to run cm from the project root; if the
    //    binary isn't there, or a present file lacks/has-a-stale pointer, the agent
    //    can't actually reach cm (e.g. a CLAUDE.md that references a cm.exe that was
    //    never placed). doctor reports it; the fix is `cm init` (it places the binary
    //    and wires the pointer) — deliberately NOT under --fix, which never edits
    //    instruction files.
    let layout = init::layout_status(root, &data_path);
    let mut wiring_issues: Vec<String> = Vec::new();
    if layout.instruction_files.is_empty() {
        wiring_issues.push("no agent instruction file (CLAUDE.md/AGENTS.md/…)".into());
    } else if layout.wired_files.is_empty() {
        wiring_issues.push(format!(
            "instruction file(s) {} carry no current cm pointer",
            layout.instruction_files.join("/")
        ));
    }
    if !layout.binary_present {
        wiring_issues.push(format!(
            "the cm binary the instructions reference ({}) is missing at the project root",
            layout.exe_display
        ));
    }
    if wiring_issues.is_empty() {
        f.push(ok("agent_wiring"));
    } else {
        // A missing binary, or a present-but-unwired instruction file, is a real
        // problem (an agent can't reach cm). Merely having NO instruction file at all
        // (with the binary in place) is advisory — `init` would create an AGENTS.md.
        let only_no_file = layout.instruction_files.is_empty() && layout.binary_present;
        let status = if only_no_file {
            Status::Info
        } else {
            Status::Warn
        };
        f.push(finding(
            "agent_wiring",
            status,
            wiring_issues.join("; "),
            false,
            Some("cm init  (places the binary & wires the pointer)".into()),
        ));
    }

    // Cosmetic scaffold extras (gitignore rules, the standalone guide) — info only.
    let mut extras: Vec<&str> = Vec::new();
    if !layout.data_gitignore {
        extras.push("data .gitignore");
    }
    if !layout.root_ignored {
        extras.push("root .gitignore rule for the binary");
    }
    if !layout.guide_present {
        extras.push("CM_GUIDE.md");
    }
    if extras.is_empty() {
        f.push(ok("scaffold"));
    } else {
        f.push(finding(
            "scaffold",
            Status::Info,
            format!("missing: {}", extras.join(", ")),
            false,
            Some("cm init  (idempotent — completes only what's missing)".into()),
        ));
    }

    let mut need_reindex = false;
    let mut need_reindex_all = false;

    // 4. store.db present — the DELETED-STORE scenario. The store is derived, so a
    //    missing store with md truth on disk is fully rebuildable.
    let store_present = ctx.store_path.is_file();
    if store_present {
        f.push(ok("store_present"));
    } else {
        let has_truth = dir_has_md(&ctx.notes_dir) || dir_has_files(&ctx.imports_dir);
        let fix = if has_truth {
            "cm doctor --fix  (rebuilds the index from notes/ + imports/), or cm reindex"
        } else {
            "cm reindex  (rebuilds the index), or cm init"
        };
        f.push(finding(
            "store_present",
            Status::Error,
            format!("store.db missing at {}", ctx.store_path.display()),
            true,
            Some(fix.into()),
        ));
        need_reindex = true;
    }

    // 5. Data subdirs present.
    let missing_dirs: Vec<&str> = ["notes", "imports", "models"]
        .into_iter()
        .filter(|d| !data_path.join(d).is_dir())
        .collect();
    if missing_dirs.is_empty() {
        f.push(ok("data_dirs_present"));
    } else {
        f.push(finding(
            "data_dirs_present",
            Status::Warn,
            format!("missing data subdir(s): {}", missing_dirs.join(", ")),
            true,
            Some("cm doctor --fix  (creates them), or cm init".into()),
        ));
        need_reindex = true;
    }

    // Open the store for the deep, content-level checks. Only if it's present — a
    // missing store was already reported and is repaired by the reindex path.
    let store = if store_present {
        match Store::open(&ctx.store_path) {
            Ok(s) => {
                f.push(ok("store_opens"));
                Some(s)
            }
            Err(e) => {
                f.push(finding(
                    "store_opens",
                    Status::Error,
                    format!("store.db won't open: {}", e.msg),
                    false,
                    Some("back up, delete store.db, then `cm reindex --all`".into()),
                ));
                None
            }
        }
    } else {
        None
    };

    if let Some(store) = &store {
        store_checks(
            ctx,
            &cfg,
            &data_path,
            store,
            &mut f,
            &mut stats,
            &mut need_reindex,
            &mut need_reindex_all,
        );
    }

    // store.db size + path — informational, and a breadcrumb for the renamed-folder case.
    if let Ok(meta) = std::fs::metadata(&ctx.store_path) {
        stats.push((
            "store_db",
            json!({ "path": ctx.store_path.to_string_lossy(), "bytes": meta.len() }),
        ));
    }

    Report {
        findings: f,
        stats,
        renamed,
        data_resolved,
        need_reindex_all,
        need_reindex,
    }
}

/// The store-backed checks + stats (only runs when store.db opened). Pushed into the
/// shared `findings`/`stats`; sets the repair flags the `--fix` path reads.
#[allow(clippy::too_many_arguments)]
fn store_checks(
    ctx: &Ctx,
    cfg: &Config,
    data_path: &Path,
    store: &Store,
    f: &mut Vec<Finding>,
    stats: &mut Vec<(&'static str, Value)>,
    need_reindex: &mut bool,
    need_reindex_all: &mut bool,
) {
    // --- statistics ---
    let notes = store.count_notes_kind("note").unwrap_or(0);
    let chunks = store.count_notes_kind("chunk").unwrap_or(0);
    let imports = store.list_imports().map(|v| v.len() as i64).unwrap_or(0);
    stats.push(("notes", json!(notes)));
    stats.push(("chunks", json!(chunks)));
    stats.push(("imports", json!(imports)));
    let (cf, cs, ce) = store.code_counts().unwrap_or((0, 0, 0));
    if cf > 0 || cs > 0 || ce > 0 {
        stats.push(("code", json!({ "files": cf, "symbols": cs, "edges": ce })));
    }
    let dangling = store
        .dangling_edge_sources()
        .map(|v| v.len() as i64)
        .unwrap_or(0);
    stats.push(("dangling_edges", json!(dangling)));
    if let Ok(rl) = store.recent_logs(1) {
        if let Some(last) = rl.first() {
            stats.push((
                "last_activity",
                json!({ "op": last.op, "at": last.created_iso }),
            ));
        }
    }

    // --- notes ↔ notes_fts manual sync ---
    match store.fts_desync_counts() {
        Ok((0, 0)) => f.push(ok("notes_fts_sync")),
        Ok((no, fo)) => {
            f.push(finding(
                "notes_fts_sync",
                Status::Error,
                format!("notes↔fts desync: {no} note(s) unindexed, {fo} orphan fts row(s)"),
                true,
                Some("cm reindex --all".into()),
            ));
            *need_reindex_all = true;
        }
        Err(_) => {}
    }

    // --- embedder buildable + signature drift + dimension ---
    let live_sig = match embed::build(cfg) {
        Ok(e) => {
            f.push(ok("embedder_buildable"));
            Some(e.signature())
        }
        Err(e) => {
            let fix = e.hint.clone().or_else(|| {
                Some("cm config set embedding.provider local, or set the api endpoint/key".into())
            });
            f.push(finding(
                "embedder_buildable",
                Status::Warn,
                e.msg,
                false,
                fix,
            ));
            None
        }
    };
    stats.push((
        "embedder",
        json!({
            "config": format!("{}:{}:{}", cfg.embedding.provider, cfg.embedding.model, cfg.embedding.dimension),
            "stored": store.meta_get("embedder_signature").ok().flatten(),
        }),
    ));
    match (
        store.meta_get("embedder_signature").ok().flatten(),
        &live_sig,
    ) {
        (Some(stored), Some(live)) if &stored != live => {
            f.push(finding(
                "embedder_signature_drift",
                Status::Warn,
                format!("stored '{stored}' != live '{live}' — vectors won't compare"),
                true,
                Some("cm reindex --all  (re-embeds every note)".into()),
            ));
            *need_reindex_all = true;
        }
        (Some(_), Some(_)) => f.push(ok("embedder_signature_drift")),
        (None, Some(_)) => f.push(finding(
            "embedder_signature_drift",
            Status::Info,
            "no embedder signature recorded yet (old store)",
            false,
            Some("cm reindex  (records it)".into()),
        )),
        _ => {} // embedder unbuildable — already warned via embedder_buildable
    }

    // --- embedding blobs: well-formed + dimension matches config ---
    match store.embedding_stats() {
        Ok((0, _)) => {
            stats.push(("embeddings", json!(0)));
            f.push(finding(
                "embedding_dimension",
                Status::Info,
                "no embeddings stored (keyword-only index)",
                false,
                Some("cm reindex --all  (once the embedder is configured)".into()),
            ));
        }
        Ok((count, lengths)) => {
            stats.push(("embeddings", json!(count)));
            let expected = cfg.embedding.dimension as i64 * 4;
            let misaligned: Vec<i64> = lengths.iter().copied().filter(|l| l % 4 != 0).collect();
            let wrong: Vec<i64> = lengths
                .iter()
                .copied()
                .filter(|l| l % 4 == 0 && *l != expected)
                .collect();
            if !misaligned.is_empty() {
                f.push(finding(
                    "embedding_dimension",
                    Status::Error,
                    format!(
                        "corrupt embedding blob length(s): {misaligned:?} (not a multiple of 4)"
                    ),
                    true,
                    Some("cm reindex --all".into()),
                ));
                *need_reindex_all = true;
            } else if !wrong.is_empty() {
                let dims: Vec<i64> = wrong.iter().map(|l| l / 4).collect();
                f.push(finding(
                    "embedding_dimension",
                    Status::Warn,
                    format!(
                        "stored dimension {:?} != config {} — vectors won't compare",
                        dims, cfg.embedding.dimension
                    ),
                    true,
                    Some("cm reindex --all".into()),
                ));
                *need_reindex_all = true;
            } else {
                f.push(ok("embedding_dimension"));
            }
        }
        Err(_) => {}
    }

    // --- resolved note edges point at live notes (dangling is fine) ---
    match store.resolved_edge_orphans() {
        Ok(0) => f.push(ok("resolved_edges")),
        Ok(n) => {
            f.push(finding(
                "resolved_edges",
                Status::Warn,
                format!("{n} resolved edge(s) point at a deleted note"),
                true,
                Some("cm reindex".into()),
            ));
            *need_reindex = true;
        }
        Err(_) => {}
    }

    // --- slug collisions (links resolve to the lowest id — report, don't fix) ---
    if let Ok(slugs) = store.note_slugs() {
        let coll = graph::slug_collisions(&slugs);
        if coll.is_empty() {
            f.push(ok("slug_collisions"));
        } else {
            let detail = coll
                .iter()
                .map(|(slug, ids)| format!("{slug} → {}", ids.join("/")))
                .collect::<Vec<_>>()
                .join("; ");
            f.push(finding(
                "slug_collisions",
                Status::Warn,
                detail,
                false,
                Some("rename a slug in the colliding note's md".into()),
            ));
        }
    }

    // --- store ↔ disk drift for notes (md is the source of truth) ---
    let note_ids = store.note_ids_of_kind("note").unwrap_or_default();
    let missing_md: Vec<String> = note_ids
        .iter()
        .filter(|id| !ctx.note_path(id).is_file())
        .cloned()
        .collect();
    if missing_md.is_empty() {
        f.push(ok("notes_md_truth"));
    } else {
        f.push(finding(
            "notes_md_truth",
            Status::Error,
            format!(
                "{} note(s) in the store have no notes/<id>.md (can't rebuild): {}",
                missing_md.len(),
                sample(&missing_md)
            ),
            false,
            Some("restore the md file(s) then `cm reindex`, or `cm forget <id>` to drop".into()),
        ));
    }
    let db_set: HashSet<&str> = note_ids.iter().map(String::as_str).collect();
    let orphan_files = note_files_without_row(&ctx.notes_dir, &db_set);
    if orphan_files.is_empty() {
        f.push(ok("orphan_note_files"));
    } else {
        f.push(finding(
            "orphan_note_files",
            Status::Warn,
            format!(
                "{} note file(s) on disk not in the index: {}",
                orphan_files.len(),
                sample(&orphan_files)
            ),
            true,
            Some("cm reindex".into()),
        ));
        *need_reindex = true;
    }

    // --- import originals present on disk, and disk files recorded ---
    if let Ok(imps) = store.list_imports() {
        let missing_orig: Vec<String> = imps
            .iter()
            .filter(|r| !data_path.join(&r.source).is_file())
            .map(|r| r.source.clone())
            .collect();
        let unrecorded = import_files_unrecorded(&ctx.imports_dir, &imps);
        if missing_orig.is_empty() && unrecorded.is_empty() {
            f.push(ok("import_originals"));
        } else {
            let mut parts = Vec::new();
            if !missing_orig.is_empty() {
                parts.push(format!(
                    "{} recorded import(s) missing their file",
                    missing_orig.len()
                ));
            }
            if !unrecorded.is_empty() {
                parts.push(format!(
                    "{} file(s) under imports/ not indexed",
                    unrecorded.len()
                ));
            }
            f.push(finding(
                "import_originals",
                Status::Warn,
                parts.join("; "),
                true,
                Some("cm reindex".into()),
            ));
            *need_reindex = true;
        }
    }

    // --- code graph consistency (report only; `cm map` re-maps, --fix won't) ---
    if cf > 0 || cs > 0 || ce > 0 {
        let dangling_code = store
            .dangling_code_sources()
            .map(|v| v.len() as i64)
            .unwrap_or(0);
        stats.push(("dangling_code_uses", json!(dangling_code)));
        match store.code_orphans() {
            Ok((0, 0)) => f.push(ok("code_graph_consistency")),
            Ok((sym, edge)) => f.push(finding(
                "code_graph_consistency",
                Status::Warn,
                format!("{sym} symbol(s) orphaned from a deleted file, {edge} resolved use(s) point nowhere"),
                false,
                Some("cm map <path>  (re-maps & re-dangles)".into()),
            )),
            Err(_) => {}
        }
    }
}

/// Scan `root` for a single immediate subfolder that looks like a climem data folder
/// (store.db + imports/), excluding the `missing` path config points at. Returns the
/// folder's name only when EXACTLY one candidate is found — an unambiguous rename — so
/// `--fix` never guesses among several. Heuristic by design: only ever a hint.
fn find_renamed_data_dir(root: &Path, missing: &Path) -> Option<String> {
    let mut cands = Vec::new();
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let p = e.path();
        if !p.is_dir() || p.as_path() == missing {
            continue;
        }
        if init::is_memory_folder(&p) {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                cands.push(name.to_string());
            }
        }
    }
    if cands.len() == 1 {
        Some(cands.remove(0))
    } else {
        None
    }
}

/// Rewrite only `data_dir` in config.json, preserving every other (and unknown) key —
/// the same raw-JSON path `cm config set` uses. The one config write `--fix` performs,
/// and only for an unambiguous renamed-folder match.
fn rewrite_data_dir(config_path: &Path, name: &str) -> Result<()> {
    let mut v = config::load_raw(config_path)?;
    config::set_path(&mut v, "data_dir", name)?;
    config::save_raw(config_path, &v)
}

/// Invoke the real `reindex` command with the same `Ctx`, incremental or `--all`.
fn run_reindex(ctx: &Ctx, all: bool) -> Result<()> {
    let args: Vec<String> = if all {
        vec!["reindex".into(), "--all".into()]
    } else {
        vec!["reindex".into()]
    };
    commands::reindex(&Parsed::parse(&args), ctx)
}

/// True if `dir` holds at least one `.md` file (md truth to rebuild a lost store from).
fn dir_has_md(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| is_md(&e.path())))
        .unwrap_or(false)
}

/// True if `dir` holds at least one regular file (e.g. import originals).
fn dir_has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.path().is_file()))
        .unwrap_or(false)
}

fn is_md(p: &Path) -> bool {
    p.extension()
        .and_then(|x| x.to_str())
        .map(|x| x.eq_ignore_ascii_case("md") || x.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

/// `notes/<id>.md` files whose `<id>` has no row in the store (un-indexed note files).
fn note_files_without_row(notes_dir: &Path, db_ids: &HashSet<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(notes_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() || !is_md(&p) {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if !db_ids.contains(stem) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out
}

/// Files under `imports/` (skipping `.meta.json` sidecars) that have no import record,
/// i.e. originals the index doesn't know about. Import sources are stored as the
/// canonical `imports/<name>` key, so we compare against that shape.
fn import_files_unrecorded(imports_dir: &Path, imps: &[ImportRow]) -> Vec<String> {
    let recorded: HashSet<&str> = imps.iter().map(|r| r.source.as_str()).collect();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(imports_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".meta.json") {
                continue;
            }
            if !recorded.contains(format!("imports/{name}").as_str()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Render up to five ids/names for a finding's detail, with a `(+N)` tail.
fn sample(v: &[String]) -> String {
    let shown: Vec<&str> = v.iter().take(5).map(String::as_str).collect();
    if v.len() > 5 {
        format!("{}, … (+{})", shown.join(", "), v.len() - 5)
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::TempDir;

    /// Lay down a healthy split-layout memory folder under `root` (data_dir=memory).
    fn healthy(root: &Path) {
        std::fs::write(
            root.join("config.json"),
            r#"{"name":"t","data_dir":"memory"}"#,
        )
        .unwrap();
        let data = root.join("memory");
        for s in ["notes", "imports", "models"] {
            std::fs::create_dir_all(data.join(s)).unwrap();
        }
        Store::create(&data.join("store.db")).unwrap();
        // A stub cm binary at the root, where the wired pointers reference it, so the
        // `agent_wiring` check sees a placed binary (it only checks presence).
        std::fs::write(
            root.join(if cfg!(windows) { "cm.exe" } else { "cm" }),
            b"stub",
        )
        .unwrap();
    }

    fn check<'a>(r: &'a Report, name: &str) -> &'a Finding {
        r.findings.iter().find(|f| f.check == name).unwrap()
    }

    #[test]
    fn healthy_empty_store_has_no_errors_or_warnings() {
        let tmp = TempDir::new().unwrap();
        healthy(tmp.path());
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        assert!(r.data_resolved);
        assert!(
            r.findings.iter().all(|f| f.status != Status::Error),
            "unexpected error: {:?}",
            r.findings
        );
        assert!(
            r.findings.iter().all(|f| f.status != Status::Warn),
            "unexpected warning: {:?}",
            r.findings
        );
        assert_eq!(check(&r, "config_present").status, Status::Ok);
        assert_eq!(check(&r, "store_present").status, Status::Ok);
        // No embeddings yet -> info, and the embedder builds (local, offline).
        assert_eq!(check(&r, "embedder_buildable").status, Status::Ok);
        assert!(!r.need_reindex && !r.need_reindex_all);
    }

    #[test]
    fn missing_store_db_is_a_fixable_error() {
        let tmp = TempDir::new().unwrap();
        healthy(tmp.path());
        std::fs::remove_file(tmp.path().join("memory").join("store.db")).unwrap();
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        let sp = check(&r, "store_present");
        assert_eq!(sp.status, Status::Error);
        assert!(sp.fixable);
        assert!(r.need_reindex, "a lost store should trigger a rebuild");
    }

    #[test]
    fn renamed_data_folder_is_detected_with_single_candidate() {
        let tmp = TempDir::new().unwrap();
        // config says `memory`, but the data lives in a sibling `mem/`.
        std::fs::write(tmp.path().join("config.json"), r#"{"data_dir":"memory"}"#).unwrap();
        let mem = tmp.path().join("mem");
        std::fs::create_dir_all(mem.join("imports")).unwrap();
        Store::create(&mem.join("store.db")).unwrap();
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        assert!(!r.data_resolved);
        assert_eq!(r.renamed.as_deref(), Some("mem"));
        let dr = check(&r, "data_dir_resolves");
        assert_eq!(dr.status, Status::Error);
        assert!(dr.fixable);
    }

    #[test]
    fn ambiguous_or_absent_rename_is_not_auto_fixable() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.json"), r#"{"data_dir":"memory"}"#).unwrap();
        // Two sibling data folders -> ambiguous -> no candidate.
        for name in ["mem", "store2"] {
            let d = tmp.path().join(name);
            std::fs::create_dir_all(d.join("imports")).unwrap();
            Store::create(&d.join("store.db")).unwrap();
        }
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        assert_eq!(r.renamed, None);
        assert!(!check(&r, "data_dir_resolves").fixable);
    }

    #[test]
    fn rewrite_data_dir_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"data_dir":"memory","future":true,"name":"x"}"#).unwrap();
        rewrite_data_dir(&path, "mem").unwrap();
        let raw = config::load_raw(&path).unwrap();
        assert_eq!(raw["data_dir"], serde_json::json!("mem"));
        assert_eq!(raw["future"], serde_json::json!(true));
        assert_eq!(raw["name"], serde_json::json!("x"));
    }

    #[test]
    fn instructions_pointing_at_missing_binary_warn() {
        let tmp = TempDir::new().unwrap();
        healthy(tmp.path());
        // Remove the binary the (would-be) pointers reference, and add an instruction
        // file that carries no pointer block — the exact "CLAUDE.md points at a cm that
        // isn't there" situation.
        std::fs::remove_file(tmp.path().join(if cfg!(windows) { "cm.exe" } else { "cm" })).unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# AGENTS\n").unwrap();
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        let aw = check(&r, "agent_wiring");
        assert_eq!(aw.status, Status::Warn);
        let detail = aw.detail.as_ref().unwrap();
        assert!(detail.contains("missing at the project root"), "{detail}");
        assert!(detail.contains("no current cm pointer"), "{detail}");
    }

    #[test]
    fn note_in_store_without_md_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        healthy(tmp.path());
        // Insert a note row but DON'T write its notes/<id>.md (store↔disk drift).
        let data = tmp.path().join("memory");
        let store = Store::open(&data.join("store.db")).unwrap();
        store
            .insert_note("orphan in db", "", None, None, "note", &[0.1, 0.2])
            .unwrap();
        drop(store);
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        assert_eq!(check(&r, "notes_md_truth").status, Status::Error);
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(4_468_736), "4.3 MB");
    }

    #[test]
    fn text_report_has_marks_and_stats() {
        let tmp = TempDir::new().unwrap();
        healthy(tmp.path());
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        let out = render_text(&r, "MEM");
        assert!(out.contains("cm doctor — MEM"));
        assert!(out.contains("✓ config_present"));
        assert!(out.contains("Statistics"));
        assert!(out.contains("notes"));
        // Summary renders the verdict.
        let mut c = Counts::default();
        for f in &r.findings {
            c.tally(f);
        }
        assert!(summary_text(&c, 0, false).contains("all checks passed"));
    }

    #[test]
    fn broken_config_stops_after_reporting() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.json"), "{ not json").unwrap();
        let ctx = Ctx::new(tmp.path());
        let r = diagnose(&ctx, tmp.path());
        assert_eq!(check(&r, "config_present").status, Status::Error);
        // Diagnosis stops: only the config finding is present.
        assert_eq!(r.findings.len(), 1);
        assert!(!r.data_resolved);
    }
}
