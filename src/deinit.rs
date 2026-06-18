//! `deinit`: the reverse of `init` — strip cm's *derived* footprint out of a project
//! so the only thing left to delete by hand is the binary itself. The mirror of
//! `init`: same `<target>` (the project dir) and `--name` (the memory folder under
//! it). It removes the SQLite index, config, the wiring blocks from the agent's
//! instruction files, and the re-downloadable model weights — but it NEVER touches
//! the source of truth (`notes/*.md` + `imports/*`), which are the user's content.
//! After it runs the memory folder still holds your md (and the binary copy, which a
//! running process can't delete on Windows); a now-empty folder is removed outright.

use crate::cli::Parsed;
use crate::init;
use crate::output::print_line;
use crate::util::{AppError, Result};
use serde_json::json;
use std::path::Path;

/// Files we lay down in the memory folder that are derived / rebuildable, so they're
/// safe to remove. Pointedly NOT here: `notes/`, `imports/` (the truth), and the
/// `cm(.exe)` copy (the user removes that last — a running exe can't delete itself
/// on Windows anyway).
const DERIVED_FILES: &[&str] = &[
    "store.db",
    "store.db-wal",
    "store.db-shm",
    "config.json",
    ".gitignore",
];

/// Subfolders in the memory folder that are derived (re-downloadable weights).
const DERIVED_DIRS: &[&str] = &["models"];

pub fn run(p: &Parsed) -> Result<()> {
    let target = p.arg(0).ok_or_else(|| {
        AppError::with_hint(
            "deinit needs the project path (the same one you passed to init)",
            "cm deinit ./ --name project-memory",
        )
    })?;
    let target = Path::new(target);
    let name = p.value("name").unwrap_or(".memory");
    let folder = target.join(name);

    // `--yes`/`--force` skips the prompt (scripts/CI); otherwise we ask, and a piped
    // stdin (EOF) safely declines. The truth (md) survives either way, so the worst a
    // mistaken yes costs is a `cm reindex`.
    let assume_yes = p.has("yes") || p.has("force");
    if !assume_yes
        && !init::prompt_yes_no(&format!(
            "Удалить производные следы cm (индекс, config, указатели в CLAUDE.md/…)? \
             md-файлы в {} останутся. [y/N]: ",
            folder.display()
        ))
    {
        eprintln!("Отменено — ничего не тронуто.");
        return Ok(());
    }

    // 1. Strip the pointer blocks back out of the agent's instruction files.
    let unwired = init::unwire_entry_points(target);

    // 2. Remove the derived files/dirs from the memory folder, keeping notes/ +
    //    imports/ (truth) and the cm binary copy.
    let mut removed = Vec::new();
    for f in DERIVED_FILES {
        let path = folder.join(f);
        if path.is_file() {
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(f.to_string()),
                Err(e) => eprintln!("warning: не удалось удалить {}: {e}", path.display()),
            }
        }
    }
    for d in DERIVED_DIRS {
        let path = folder.join(d);
        if path.is_dir() {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed.push(format!("{d}/")),
                Err(e) => eprintln!("warning: не удалось удалить {}: {e}", path.display()),
            }
        }
    }

    // 3. If the memory folder is now empty (no md was kept, binary already gone),
    //    remove it too so nothing dangling remains. A folder still holding notes/
    //    or imports/ — or the cm copy — is deliberately left for the user.
    let folder_removed = remove_if_empty(&folder);

    print_line(&json!({
        "deinit": folder.to_string_lossy(),
        "removed": removed,
        "unwired_files": unwired,
        "folder_removed": folder_removed,
        "kept": "notes/ + imports/ (источник правды) и копия cm — не тронуты",
    }));

    eprintln!(
        "\nГотово. Источник правды (md в notes/ и imports/) сохранён. Осталось удалить сам \
         бинарник cm вручную."
    );
    Ok(())
}

/// Remove `dir` only if it exists and is empty; returns whether it was removed.
/// (Pruning empty `notes/`/`imports/` first lets a never-used store fold away
/// entirely.) Best-effort: a non-empty dir or a removal error just leaves it be.
fn remove_if_empty(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    // Drop our own empty truth subdirs first, so a store that never got a note still
    // collapses. A subdir holding files stops the prune (read_dir stays non-empty).
    for sub in ["notes", "imports"] {
        let p = dir.join(sub);
        if p.is_dir() && dir_is_empty(&p) {
            let _ = std::fs::remove_dir(&p);
        }
    }
    if dir_is_empty(dir) {
        std::fs::remove_dir(dir).is_ok()
    } else {
        false
    }
}

fn dir_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// Lay out a memory folder like `init` would, plus a wired CLAUDE.md, so we can
    /// assert deinit removes the derived bits and keeps the truth.
    fn scaffold(target: &Path, name: &str) -> PathBuf {
        let folder = target.join(name);
        std::fs::create_dir_all(folder.join("notes")).unwrap();
        std::fs::create_dir_all(folder.join("imports")).unwrap();
        std::fs::create_dir_all(folder.join("models")).unwrap();
        std::fs::write(folder.join("store.db"), b"INDEX").unwrap();
        std::fs::write(folder.join("config.json"), b"{}").unwrap();
        std::fs::write(folder.join(".gitignore"), b"store.db\n").unwrap();
        std::fs::write(folder.join("models").join("w.bin"), b"weights").unwrap();
        // A copy of the binary that must survive deinit.
        std::fs::write(folder.join("cm.exe"), b"BINARY").unwrap();
        folder
    }

    #[test]
    fn deinit_removes_derived_keeps_truth_and_binary() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, ".memory");
        // A real note (truth) and a wired CLAUDE.md.
        std::fs::write(folder.join("notes").join("0a1b.md"), "# note\nbody").unwrap();
        std::fs::write(folder.join("imports").join("doc.md"), "imported").unwrap();
        std::fs::write(
            target.join("CLAUDE.md"),
            "rules\n\n<!-- BEGIN cm memory pointer -->\nx\n<!-- END cm memory pointer -->\n",
        )
        .unwrap();

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // Derived gone.
        assert!(!folder.join("store.db").exists());
        assert!(!folder.join("config.json").exists());
        assert!(!folder.join(".gitignore").exists());
        assert!(!folder.join("models").exists());
        // Truth + binary kept.
        assert!(folder.join("notes").join("0a1b.md").exists());
        assert!(folder.join("imports").join("doc.md").exists());
        assert!(folder.join("cm.exe").exists());
        // Pointer block stripped, original content restored.
        let claude = std::fs::read_to_string(target.join("CLAUDE.md")).unwrap();
        assert!(!claude.contains("BEGIN cm memory pointer"));
        assert_eq!(claude, "rules\n");
        // Folder kept (still holds md + binary).
        assert!(folder.is_dir());
    }

    #[test]
    fn deinit_removes_empty_folder_when_no_md_kept() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, ".memory");
        // Remove the binary copy too (simulate user already deleted it), no md at all.
        std::fs::remove_file(folder.join("cm.exe")).unwrap();

        run(&parsed(&["deinit", target.to_str().unwrap(), "--yes"])).unwrap();

        // Nothing of value left -> the whole folder is gone.
        assert!(!folder.exists());
    }

    #[test]
    fn deinit_declines_without_yes_on_piped_stdin() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, ".memory");
        // No --yes: prompt reads EOF from the (non-tty) test stdin -> declines.
        run(&parsed(&["deinit", target.to_str().unwrap()])).unwrap();
        // Everything still there.
        assert!(folder.join("store.db").exists());
        assert!(folder.join("config.json").exists());
    }

    #[test]
    fn deinit_missing_target_errors_with_hint() {
        let err = run(&parsed(&["deinit"])).unwrap_err();
        assert!(err.msg.contains("deinit needs the project path"));
        assert!(err.hint.as_deref().unwrap().contains("cm deinit"));
    }

    #[test]
    fn deinit_force_is_alias_for_yes() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, ".memory");
        run(&parsed(&["deinit", target.to_str().unwrap(), "--force"])).unwrap();
        assert!(!folder.join("store.db").exists());
    }

    #[test]
    fn deinit_custom_name() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let folder = scaffold(target, ".mem2");
        run(&parsed(&[
            "deinit",
            target.to_str().unwrap(),
            "--name",
            ".mem2",
            "--yes",
        ]))
        .unwrap();
        assert!(!folder.join("store.db").exists());
    }
}
