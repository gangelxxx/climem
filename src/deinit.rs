//! `deinit`: the FULL rollback of `init` — return a project to its pre-init state,
//! leaving only the things `init` is allowed to leave: the `cm(.exe)` binary and its
//! `config.json`. It is driven by the snapshot manifest `init` writes
//! (`<data_folder>/.init-manifest.json`):
//!
//! - the pointer blocks appended to CLAUDE.md/AGENTS.md/… are stripped, and an
//!   `AGENTS.md` that `init` *created* is removed wholesale;
//! - every imported doc is RESTORED to disk — to its original path when that spot
//!   is free, otherwise under `<dir>/climem/<file>` so a user's newer file is never
//!   clobbered; docs added later via `cm import` (no manifest entry) land in
//!   `<target>/docs/climem/`;
//! - the project-root `.gitignore` is restored to its pre-init bytes, or deleted if
//!   `init` created it;
//! - the data folder (`memory/`) is removed ENTIRELY (store.db, notes/, imports/,
//!   models/, the manifest).
//!
//! When no manifest is present (a store from before this existed, or a hand-edited
//! folder) it falls back to the `imports/` sidecars (restoring by basename into
//! `docs/climem/`) and strips only its own `# cm binary…` block from the root
//! `.gitignore`. A running `.exe` can't delete itself on Windows, so the binary is
//! always the user's to remove last — but everything else is gone.

use crate::cli::Parsed;
use crate::import;
use crate::init::{self, GitignoreState, InitManifest};
use crate::output::print_line;
use crate::util::Result;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Derived files we strip in the LEGACY single-folder layout (`data_dir="."`), where
/// the data folder IS the project root and so can't be `remove_dir_all`'d. In the
/// normal split layout the whole data folder is removed instead.
const LEGACY_DERIVED_FILES: &[&str] = &[
    "store.db",
    "store.db-wal",
    "store.db-shm",
    ".gitignore",
    init::MANIFEST_NAME,
];
/// Derived subfolders removed in the legacy layout (re-downloadable weights, plus the
/// truth dirs — deinit is a full uninstall now). `notes`/`imports` are emptied only
/// after their contents have been restored to the project tree.
const LEGACY_DERIVED_DIRS: &[&str] = &["models", "notes", "imports"];

pub fn run(p: &Parsed) -> Result<()> {
    // No target given → default to the current working directory, mirroring `init`
    // (a bare `cm init` scaffolds here, so a bare `cm deinit` undoes it here). An
    // explicit path still overrides (e.g. `cm deinit ./project-memory`).
    let cwd = std::env::current_dir()?;
    let target = match p.arg(0) {
        Some(t) => PathBuf::from(t),
        None => cwd,
    };
    let target = target.as_path();
    // Find the data folder (store.db/notes/imports/models). The root config.json is
    // the AUTHORITATIVE pointer (it records `data_dir`), so prefer it: this keeps a
    // mistaken/omitted --name from resolving `folder` to the wrong place. --name is
    // only the fallback for a legacy store with no root config; `memory` is the
    // last-resort default. (Load-bearing precedence — see the tests.)
    let root_config = target.join("config.json");
    let name = crate::config::Config::load(&root_config)
        .ok()
        .map(|c| c.data_dir)
        .or_else(|| p.value("name").map(str::to_string))
        .unwrap_or_else(|| "memory".to_string());
    let folder = crate::commands::resolve_data_dir(target, &name);

    // `--yes`/`--force` skips the prompt (scripts/CI); otherwise we ask, and a piped
    // stdin (EOF) safely declines. This is now a destructive full uninstall (the data
    // folder goes away), so be explicit about what survives.
    let assume_yes = p.has("yes") || p.has("force");
    if !assume_yes
        && !init::prompt_yes_no(&format!(
            "Full cm rollback: restore documents to the project tree and delete the memory \
             folder {} entirely? Only cm and config.json will remain. [y/N]: ",
            folder.display()
        ))
    {
        eprintln!("Cancelled — nothing touched.");
        return Ok(());
    }

    // Read the rollback manifest (None if it's missing/old/garbled → fallback mode).
    let manifest = read_manifest(&folder);
    let has_manifest = manifest.is_some();

    // 1. Strip the pointer blocks out of the agent's instruction files. This also
    //    removes a cm-CREATED AGENTS.md that round-trips to just its header (the
    //    heuristic). The manifest makes that authoritative for an AGENTS.md the user
    //    later edited: delete it outright if init recorded creating it.
    let unwired = init::unwire_entry_points(target);
    if manifest.as_ref().is_some_and(|m| m.created_agents_md) {
        let agents = target.join("AGENTS.md");
        if agents.is_file() {
            if let Err(e) = std::fs::remove_file(&agents) {
                eprintln!("warning: could not remove init-created AGENTS.md: {e}");
            } else {
                print_line(&json!({ "removed": agents.to_string_lossy() }));
            }
        }
    }
    // Remove the standalone CM_GUIDE.md iff init created it (never a user's own file).
    if manifest.as_ref().is_some_and(|m| m.created_cm_guide) {
        let guide = target.join(init::CM_GUIDE_NAME);
        if guide.is_file() {
            if let Err(e) = std::fs::remove_file(&guide) {
                eprintln!("warning: could not remove init-created CM_GUIDE.md: {e}");
            } else {
                print_line(&json!({ "removed": guide.to_string_lossy() }));
            }
        }
    }

    // 2. Restore the imported docs to the project tree BEFORE we delete the data
    //    folder — the copies we read from live inside `folder/imports/`.
    let restored = restore_docs(target, &folder, manifest.as_ref());

    // 3. Restore (or delete) the project-root .gitignore.
    let gitignore = restore_root_gitignore(target, manifest.as_ref().map(|m| &m.root_gitignore));

    // 4. Remove the data folder. In the split layout it's a subfolder of the project,
    //    so it goes away wholesale. In the legacy `data_dir="."` layout the data
    //    folder IS the project root (holding the running exe + config we must keep),
    //    so we surgically remove only the cm-owned bits there.
    let folder_removed = if folder != *target {
        match std::fs::remove_dir_all(&folder) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "warning: could not remove memory folder {}: {e}",
                    folder.display()
                );
                false
            }
        }
    } else {
        remove_legacy_in_place(&folder);
        false
    };

    print_line(&json!({
        "deinit": folder.to_string_lossy(),
        "unwired_files": unwired,
        "restored_docs": restored,
        "gitignore": gitignore,
        "folder_removed": folder_removed,
        "manifest": has_manifest,
    }));

    eprintln!(
        "\nDone. Documents restored to the project tree; memory folder removed. \
         Only cm and config.json remain (remove the cm binary manually if needed)."
    );
    Ok(())
}

/// Read `<folder>/.init-manifest.json` if present and parsable. Any error (missing,
/// unreadable, garbled, or an older shape that won't deserialize) yields `None`, and
/// the caller drops to the sidecar-based fallback.
fn read_manifest(folder: &Path) -> Option<InitManifest> {
    let text = std::fs::read_to_string(folder.join(init::MANIFEST_NAME)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Restore every imported doc to the project tree, returning the display paths we
/// wrote (each also emitted as a `{"restored":…}` JSONL line). Source copies are read
/// from `<folder>/imports/`.
///
/// With a manifest: each recorded doc goes back to its original path when that spot is
/// free, else under `<parent>/climem/<file>` (never clobbering a user's newer file).
/// Any `imports/` copy NOT named in the manifest (e.g. added later via `cm import`)
/// is routed to `<target>/docs/climem/` via its sidecar basename.
///
/// Without a manifest (fallback): every non-sidecar file under `imports/` is restored
/// by its sidecar basename into `<target>/docs/climem/`.
fn restore_docs(target: &Path, folder: &Path, manifest: Option<&InitManifest>) -> Vec<String> {
    let imports_dir = folder.join("imports");
    let mut used: HashSet<PathBuf> = HashSet::new();
    let mut restored: Vec<String> = Vec::new();

    // Manifest path: restore each recorded doc to where it came from.
    let mut claimed: HashSet<String> = HashSet::new();
    if let Some(m) = manifest {
        for rec in &m.docs {
            claimed.insert(rec.import_copy.clone());
            let src = imports_dir.join(&rec.import_copy);
            if !src.is_file() {
                continue; // copy already gone — nothing to restore
            }
            let orig = resolve_orig(target, &rec.orig_path);
            // Free spot → original path; occupied → <parent>/climem/<file>.
            let dest = if orig.exists() || used.contains(&orig) {
                climem_dest(&orig, &mut used)
            } else {
                used.insert(orig.clone());
                orig
            };
            if copy_doc(&src, &dest) {
                restored.push(record_restore(&dest));
            }
        }
    }

    // Fallback / leftovers: any imports/ copy not claimed by the manifest. With no
    // manifest at all, this covers everything; with one, it catches later `cm import`
    // additions. Both land under <target>/docs/climem/ keyed by the sidecar basename.
    let climem_root = target.join("docs").join("climem");
    if let Ok(rd) = std::fs::read_dir(&imports_dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if import::is_sidecar(&name) || claimed.contains(&name) {
                continue;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let (orig_basename, _tags) = import::read_sidecar(&imports_dir, &name);
            let base = Path::new(&orig_basename)
                .file_name()
                .map(|n| climem_root.join(n))
                .unwrap_or_else(|| climem_root.join(&orig_basename));
            let dest = if base.exists() || used.contains(&base) {
                climem_dest(&base, &mut used)
            } else {
                used.insert(base.clone());
                base
            };
            if copy_doc(&path, &dest) {
                restored.push(record_restore(&dest));
            }
        }
    }
    restored
}

/// Resolve a manifest `orig_path` (stored relative to the project target when
/// possible) against `target`. An absolute path is used as-is.
fn resolve_orig(target: &Path, orig_path: &str) -> PathBuf {
    let p = Path::new(orig_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        target.join(p)
    }
}

/// Compute a non-clobbering destination under `<parent-of-orig>/climem/`: keep the
/// filename, but if it already exists (on disk or earlier this run) disambiguate with
/// `-1`, `-2`, … on the stem. Mirrors `import::canonical_import_name`'s approach.
fn climem_dest(orig: &Path, used: &mut HashSet<PathBuf>) -> PathBuf {
    let file = orig
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("doc"));
    let parent = orig.parent().unwrap_or(Path::new("."));
    let dir = parent.join("climem");
    let first = dir.join(file);
    if !first.exists() && !used.contains(&first) {
        used.insert(first.clone());
        return first;
    }
    // Disambiguate on the stem: name-1.ext, name-2.ext, …
    let p = Path::new(file);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("doc");
    let ext = p.extension().and_then(|e| e.to_str());
    for n in 1.. {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem}-{n}.{ext}")),
            None => dir.join(format!("{stem}-{n}")),
        };
        if !candidate.exists() && !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!("the 1.. range always yields a free name")
}

/// Copy a restored doc to `dest`, creating parent dirs. Best-effort: a failure warns
/// on stderr and returns false (so it isn't counted/announced as restored).
fn copy_doc(src: &Path, dest: &Path) -> bool {
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("warning: could not create {}: {e}", parent.display());
            return false;
        }
    }
    match std::fs::copy(src, dest) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("warning: could not restore {}: {e}", dest.display());
            false
        }
    }
}

/// Emit a `{"restored":…}` JSONL line and return the display path for the summary.
fn record_restore(dest: &Path) -> String {
    let display = dest.to_string_lossy().into_owned();
    print_line(&json!({ "restored": display }));
    display
}

/// Restore (or delete) the project-root `.gitignore`. Returns one of
/// `"restored" | "deleted" | "untouched"` for the summary.
///
/// With a manifest `GitignoreState`: if init didn't modify it → leave it
/// (`untouched`); if it modified an existing file → write the saved bytes back
/// (`restored`); if init CREATED it → delete it (`deleted`).
///
/// Without a manifest (fallback): strip just our own `# cm binary…` block (marker +
/// the two `cm`/`cm.exe` lines) from the root `.gitignore`, preserving everything
/// else (`restored` if we changed anything, else `untouched`).
fn restore_root_gitignore(target: &Path, state: Option<&GitignoreState>) -> &'static str {
    let path = target.join(".gitignore");
    match state {
        Some(s) if !s.modified => "untouched",
        Some(s) if s.existed => match std::fs::write(&path, &s.original) {
            Ok(()) => "restored",
            Err(e) => {
                eprintln!("warning: could not restore .gitignore: {e}");
                "untouched"
            }
        },
        Some(_) => {
            // init created it (modified && !existed) → remove it.
            if path.is_file() {
                if let Err(e) = std::fs::remove_file(&path) {
                    eprintln!("warning: could not delete .gitignore: {e}");
                    return "untouched";
                }
            }
            "deleted"
        }
        None => strip_cm_block_from_gitignore(&path),
    }
}

/// Fallback: remove ONLY the cm-ignore block (`GITIGNORE_MARKER` + the immediately
/// following `cm` / `cm.exe` lines that init appended) from the root `.gitignore`,
/// leaving any user rules — including a `cm.exe` line that predates our marker —
/// intact. Returns `"restored"` if it changed the file, else `"untouched"`.
fn strip_cm_block_from_gitignore(path: &Path) -> &'static str {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return "untouched";
    };
    if !existing.contains(init::GITIGNORE_MARKER) {
        return "untouched";
    }
    // Walk lines, dropping our marker and the cm/cm.exe lines that follow it (the
    // exact 3-line block `ensure_binary_ignored` appends).
    let mut out: Vec<&str> = Vec::new();
    let mut lines = existing.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == init::GITIGNORE_MARKER {
            // Skip the trailing `cm` / `cm.exe` lines init wrote under the marker.
            while matches!(lines.peek().map(|l| l.trim()), Some("cm") | Some("cm.exe")) {
                lines.next();
            }
            continue;
        }
        out.push(line);
    }
    // Rebuild, preserving a trailing newline if the original had one and there's
    // content left. An empty result drops the file's leftover (it was cm-only).
    let mut rebuilt = out.join("\n");
    if !rebuilt.is_empty() && existing.ends_with('\n') {
        rebuilt.push('\n');
    }
    if rebuilt.trim().is_empty() {
        // Nothing of the user's left — remove the (now-empty) file.
        let _ = std::fs::remove_file(path);
        return "restored";
    }
    match std::fs::write(path, rebuilt) {
        Ok(()) => "restored",
        Err(e) => {
            eprintln!("warning: could not update .gitignore: {e}");
            "untouched"
        }
    }
}

/// Legacy `data_dir="."` layout: the data folder IS the project root, holding the
/// running `cm(.exe)` and the `config.json` we keep, so we can't `remove_dir_all` it.
/// Remove just the cm-owned bits in place (after docs were already restored).
fn remove_legacy_in_place(folder: &Path) {
    for f in LEGACY_DERIVED_FILES {
        let p = folder.join(f);
        if p.is_file() {
            if let Err(e) = std::fs::remove_file(&p) {
                eprintln!("warning: could not remove {}: {e}", p.display());
            }
        }
    }
    for d in LEGACY_DERIVED_DIRS {
        let p = folder.join(d);
        if p.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&p) {
                eprintln!("warning: could not remove {}: {e}", p.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::{DocRecord, GitignoreState, InitManifest, MANIFEST_NAME};
    use tempfile::TempDir;

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// Lay out a SPLIT-layout project as `init` would: config.json + cm(.exe) at the
    /// ROOT; the data folder `<name>/` holds store.db + notes/ + imports/ + models/.
    /// Returns the data folder path. No manifest is written (callers add one).
    fn scaffold(target: &Path, name: &str) -> PathBuf {
        let folder = target.join(name);
        std::fs::create_dir_all(folder.join("notes")).unwrap();
        std::fs::create_dir_all(folder.join("imports")).unwrap();
        std::fs::create_dir_all(folder.join("models")).unwrap();
        std::fs::write(folder.join("store.db"), b"INDEX").unwrap();
        std::fs::write(
            target.join("config.json"),
            format!(r#"{{"data_dir":"{name}"}}"#),
        )
        .unwrap();
        std::fs::write(folder.join(".gitignore"), b"store.db\n").unwrap();
        std::fs::write(folder.join("models").join("w.bin"), b"weights").unwrap();
        std::fs::write(target.join("cm.exe"), b"BINARY").unwrap();
        folder
    }

    /// Drop an imported copy + its sidecar under `imports/`, as init/import would.
    fn add_import(folder: &Path, copy_name: &str, orig_basename: &str, body: &str) {
        let imports = folder.join("imports");
        std::fs::write(imports.join(copy_name), body).unwrap();
        std::fs::write(
            imports.join(format!("{copy_name}.meta.json")),
            format!(r#"{{"orig":"{orig_basename}","tags":""}}"#),
        )
        .unwrap();
    }

    /// Write a manifest into the data folder.
    fn write_manifest(folder: &Path, m: &InitManifest) {
        std::fs::write(
            folder.join(MANIFEST_NAME),
            serde_json::to_string_pretty(m).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn deinit_restores_doc_to_original_free_path_and_removes_folder() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        add_import(&folder, "1.txt", "1.txt", "ORIGINAL BODY");
        // docs/ exists but docs/1.txt is ABSENT → restore goes to the original path.
        std::fs::create_dir(target.join("docs")).unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![DocRecord {
                    orig_path: "docs/1.txt".into(),
                    import_copy: "1.txt".into(),
                    deleted: true,
                }],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // The doc is back at its original path with the original bytes.
        let restored = std::fs::read_to_string(target.join("docs").join("1.txt")).unwrap();
        assert_eq!(restored, "ORIGINAL BODY");
        // The data folder is gone entirely; cm + config kept.
        assert!(!folder.exists());
        assert!(target.join("cm.exe").exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_conflict_restores_under_climem_without_clobbering() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        add_import(&folder, "1.txt", "1.txt", "IMPORTED BODY");
        // docs/1.txt ALREADY exists with the user's newer content.
        std::fs::create_dir(target.join("docs")).unwrap();
        std::fs::write(target.join("docs").join("1.txt"), "USER NEWER").unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![DocRecord {
                    orig_path: "docs/1.txt".into(),
                    import_copy: "1.txt".into(),
                    deleted: true,
                }],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // The user's file is untouched; the imported copy lands under docs/climem/.
        assert_eq!(
            std::fs::read_to_string(target.join("docs").join("1.txt")).unwrap(),
            "USER NEWER"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("docs").join("climem").join("1.txt")).unwrap(),
            "IMPORTED BODY"
        );
        assert!(!folder.exists());
    }

    #[test]
    fn deinit_late_imported_doc_without_manifest_entry_goes_to_docs_climem() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // A doc added AFTER init via `cm import` — present in imports/ but with NO
        // manifest record. An empty-docs manifest exists.
        add_import(&folder, "late.md", "late.md", "LATE BODY");
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("docs").join("climem").join("late.md")).unwrap(),
            "LATE BODY"
        );
        assert!(!folder.exists());
    }

    #[test]
    fn deinit_restores_gitignore_byte_for_byte() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // The current root .gitignore has the cm block appended; the manifest holds
        // the exact pre-init bytes.
        std::fs::write(
            target.join(".gitignore"),
            "/build\n# user\n# cm binary (large, rebuildable) — added by `cm init`\ncm\ncm.exe\n",
        )
        .unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState {
                    modified: true,
                    existed: true,
                    original: "/build\n# user\n".into(),
                },
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join(".gitignore")).unwrap(),
            "/build\n# user\n"
        );
    }

    #[test]
    fn deinit_deletes_gitignore_init_created() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // init created the root .gitignore (none existed before).
        std::fs::write(
            target.join(".gitignore"),
            "# cm binary (large, rebuildable) — added by `cm init`\ncm\ncm.exe\n",
        )
        .unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState {
                    modified: true,
                    existed: false,
                    original: String::new(),
                },
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();
        assert!(!target.join(".gitignore").exists());
    }

    #[test]
    fn deinit_leaves_gitignore_untouched_when_init_did_not_modify() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        std::fs::write(target.join(".gitignore"), "cm.exe\n# user owns this\n").unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState {
                    modified: false,
                    existed: true,
                    original: "cm.exe\n# user owns this\n".into(),
                },
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join(".gitignore")).unwrap(),
            "cm.exe\n# user owns this\n"
        );
    }

    #[test]
    fn deinit_removes_data_folder_entirely_and_keeps_cm_and_config() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        std::fs::write(folder.join("notes").join("0a1b.md"), "# note\nbody").unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // Everything cm-owned is gone; only the binary + config remain.
        assert!(!folder.exists());
        assert!(target.join("cm.exe").exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_unwires_and_removes_created_agents_md_via_manifest() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // A user CLAUDE.md with a wired block → block stripped, content kept.
        std::fs::write(
            target.join("CLAUDE.md"),
            "rules\n\n<!-- BEGIN cm memory pointer -->\nx\n<!-- END cm memory pointer -->\n",
        )
        .unwrap();
        // An AGENTS.md init created, then the USER edited (so the header heuristic
        // alone wouldn't remove it) — the manifest flag makes removal authoritative.
        std::fs::write(
            target.join("AGENTS.md"),
            "# AGENTS\n\nuser added this later\n",
        )
        .unwrap();
        write_manifest(
            &folder,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: true,
                created_cm_guide: false,
                docs: vec![],
            },
        );

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
            "rules\n"
        );
        assert!(
            !target.join("AGENTS.md").exists(),
            "init-created AGENTS.md removed via manifest flag"
        );
    }

    #[test]
    fn deinit_removes_init_created_cm_guide_but_keeps_user_guide() {
        // A CM_GUIDE.md init created (manifest flag true) is removed; with the flag
        // false (a user's own file) it survives. Two scaffolds, two manifests.
        let tmp = TempDir::new().unwrap();

        // Case 1: init created it → removed.
        let t1 = tmp.path().join("created");
        std::fs::create_dir(&t1).unwrap();
        let f1 = scaffold(&t1, "memory");
        std::fs::write(t1.join("CM_GUIDE.md"), "generated guide\n").unwrap();
        write_manifest(
            &f1,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: true,
                docs: vec![],
            },
        );
        run(&parsed(&["deinit", t1.to_str().unwrap(), "--yes"])).unwrap();
        assert!(
            !t1.join("CM_GUIDE.md").exists(),
            "init-created CM_GUIDE.md should be removed"
        );

        // Case 2: user's own → kept (flag false).
        let t2 = tmp.path().join("user");
        std::fs::create_dir(&t2).unwrap();
        let f2 = scaffold(&t2, "memory");
        std::fs::write(t2.join("CM_GUIDE.md"), "my own guide\n").unwrap();
        write_manifest(
            &f2,
            &InitManifest {
                version: 1,
                data_dir: "memory".into(),
                root_gitignore: GitignoreState::default(),
                created_agents_md: false,
                created_cm_guide: false,
                docs: vec![],
            },
        );
        run(&parsed(&["deinit", t2.to_str().unwrap(), "--yes"])).unwrap();
        assert_eq!(
            std::fs::read_to_string(t2.join("CM_GUIDE.md")).unwrap(),
            "my own guide\n",
            "a user's own CM_GUIDE.md must be preserved"
        );
    }

    #[test]
    fn deinit_fallback_without_manifest_restores_via_sidecar_and_strips_gitignore() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // Imports present, but NO manifest (a store from before this feature).
        add_import(&folder, "doc.md", "doc.md", "FALLBACK BODY");
        std::fs::write(
            target.join("CLAUDE.md"),
            "rules\n\n<!-- BEGIN cm memory pointer -->\nx\n<!-- END cm memory pointer -->\n",
        )
        .unwrap();
        // Root .gitignore with a user rule + the cm block init appended.
        std::fs::write(
            target.join(".gitignore"),
            "/build\n# cm binary (large, rebuildable) — added by `cm init`\ncm\ncm.exe\n",
        )
        .unwrap();

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // Doc restored by sidecar basename into docs/climem/.
        assert_eq!(
            std::fs::read_to_string(target.join("docs").join("climem").join("doc.md")).unwrap(),
            "FALLBACK BODY"
        );
        // Pointer block stripped; user content kept.
        assert_eq!(
            std::fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
            "rules\n"
        );
        // Only the cm block stripped from .gitignore; the user rule survives.
        assert_eq!(
            std::fs::read_to_string(target.join(".gitignore")).unwrap(),
            "/build\n"
        );
        assert!(!folder.exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_declines_without_yes_on_piped_stdin() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        // No --yes: prompt reads EOF from the (non-tty) test stdin → declines.
        run(&parsed(&["deinit", target.to_str().unwrap()])).unwrap();
        // Nothing removed.
        assert!(folder.join("store.db").exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_no_arg_defaults_to_cwd_and_declines_safely() {
        // No positional path → deinit defaults to the current working directory
        // (mirroring `cm init`). Without --yes the prompt reads EOF from the
        // non-tty test stdin and declines, so this is a safe no-op: it must NOT
        // error (the old "needs the project path") and must NOT touch anything.
        // (We can't safely assert the cwd-resolution with --yes — that would act on
        // the real repo root — so the EOF-declines path is what we exercise.)
        run(&parsed(&["deinit"])).unwrap();
    }

    #[test]
    fn deinit_force_is_alias_for_yes() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        run(&parsed(&["deinit", target.to_str().unwrap(), "--force"])).unwrap();
        assert!(!folder.exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_custom_name_removes_folder() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "mem2");
        run(&parsed(&[
            "deinit",
            target.to_str().unwrap(),
            "--name",
            "mem2",
            "--yes",
        ]))
        .unwrap();
        assert!(!folder.exists());
    }

    #[test]
    fn deinit_finds_data_folder_via_config_without_name() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, "memory");
        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();
        assert!(!folder.exists());
        assert!(target.join("config.json").exists());
    }

    #[test]
    fn deinit_config_data_dir_wins_over_mismatched_name() {
        // config.json is the authoritative pointer: a WRONG --name must NOT make
        // deinit act elsewhere. config's data_dir wins; the bogus folder is untouched.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let real = scaffold(target, "memory"); // config records data_dir="memory"
        run(&parsed(&[
            "deinit",
            target.to_str().unwrap(),
            "--name",
            "wrongname",
            "--yes",
        ]))
        .unwrap();
        assert!(!real.exists());
        assert!(target.join("config.json").exists());
        assert!(!target.join("wrongname").exists());
    }

    #[test]
    fn deinit_rerun_is_safe_noop() {
        // A second deinit (folder already gone) must not error or restore anything.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let _folder = scaffold(target, "memory");
        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();
        // Second run: no folder, no manifest, no imports → graceful no-op.
        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();
        assert!(target.join("config.json").exists());
    }
}
