//! `init`: scaffold a self-contained, portable memory folder (desc.md §5). If the
//! target directory already has `.md` files in it, we offer to import them all in
//! one pass (y/n) and then optionally delete the originals.

use crate::cli::Parsed;
use crate::config::Config;
use crate::embed;
use crate::help;
use crate::import;
use crate::output::print_line;
use crate::store::Store;
use crate::util::{AppError, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn run(p: &Parsed) -> Result<()> {
    let target = p.arg(0).ok_or_else(|| {
        AppError::with_hint(
            "init needs a target path",
            "cm init ./ --name project-memory",
        )
    })?;

    let name = p.value("name").unwrap_or(".memory");
    let folder = Path::new(target).join(name);

    // Never clobber an existing store (desc.md §5); just say so and stop.
    if folder.exists() {
        println!(
            "{}",
            json!({
                "status": "already_exists",
                "path": folder.to_string_lossy(),
                "note": "Хранилище уже существует — ничего не тронуто. Удалите/переименуйте папку, чтобы развернуть заново.",
            })
        );
        return Ok(());
    }

    // Start from the defaults, then layer on any flag overrides.
    let mut cfg = Config::new_named(name);
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

    // Lay out the folder. notes/ and imports/ are the source of truth (the md
    // files and the import originals); store.db is the derived, rebuildable index.
    std::fs::create_dir_all(folder.join("notes"))?;
    std::fs::create_dir_all(folder.join("imports"))?;
    std::fs::create_dir_all(folder.join("models"))?;
    let config_path = folder.join("config.json");
    cfg.save(&config_path)?;

    let store_path = folder.join("store.db");
    Store::create(&store_path)?;

    // A .gitignore so a committed memory folder keeps the TRUTH (notes/ + imports/
    // + config.json) but ignores the derived index, binary, and re-downloadable weights.
    // Commit: notes/  imports/  config.json
    // Ignore: binary (large, rebuildable via `cargo build`), store.db (derived),
    //         models/ (re-downloadable weights).
    let gitignore = "# Binary — re-buildable; not source of truth.\ncm\ncm.exe\n\
                     # Derived index — rebuild from md with `cm reindex`.\n\
                     store.db\nstore.db-wal\nstore.db-shm\n\
                     # Embedding weights (re-downloadable).\nmodels/\n";
    let _ = std::fs::write(folder.join(".gitignore"), gitignore);

    // Copy the running binary in too, so the whole memory folder travels together.
    let exe_name = if cfg!(windows) { "cm.exe" } else { "cm" };
    let exe_dest = folder.join(exe_name);
    let self_exe = std::env::current_exe()?;
    if let Err(e) = std::fs::copy(&self_exe, &exe_dest) {
        // Not fatal: the store still works, we just couldn't copy the binary.
        eprintln!("warning: could not copy the binary into the folder: {e}");
    }

    // Record the embedder signature (so we can spot provider/dimension drift
    // later) and log the init. If the provider can't be built yet (say, api with
    // no endpoint) we just skip the signature; the store is still perfectly valid.
    if let Ok(store) = Store::open(&store_path) {
        if let Ok(emb) = embed::build(&cfg) {
            let _ = store.meta_set("embedder_signature", &emb.signature());
        }
        let _ = store.log_op("init", Some(&folder.to_string_lossy()));
    }

    // Offer to import any .md files that already live in the target directory.
    // This covers the "docs folder" scenario: user drops cm.exe there, runs
    // `init ./`, and gets a one-shot bulk import + optional cleanup.
    import_existing_md(Path::new(target), &folder, &cfg, &store_path);

    let exe_display = display_path(&exe_dest);
    println!(
        "{}",
        json!({
            "created": folder.to_string_lossy(),
            "exe": exe_dest.to_string_lossy(),
            "store": store_path.to_string_lossy(),
            "config": config_path.to_string_lossy(),
            "notes": folder.join("notes").to_string_lossy(),
            "imports": folder.join("imports").to_string_lossy(),
            "provider": cfg.embedding.provider,
            "offline": cfg.embedding.provider == "local",
        })
    );

    // Print the ready-to-paste pointer (desc.md §8).
    eprintln!("\n— Указатель для системного промпта / CLAUDE.md —\n");
    eprintln!("{}", help::pointer(&exe_display));

    Ok(())
}

/// Spot any `.md` files in the target directory and offer to import the lot.
/// Nothing here is fatal: errors go to stderr as warnings, and an import that
/// failed is never deleted. If stdin isn't readable (piped or non-interactive
/// use) we just quietly skip the whole thing.
fn import_existing_md(target: &Path, folder: &Path, cfg: &Config, store_path: &Path) {
    // Recurse the docs tree, but never descend into the memory folder we just laid
    // out under `target` (when target is `./`, `folder` lives inside it): its
    // notes/ + imports/ originals aren't user docs to re-absorb.
    let md_files = collect_md_files(target, folder);
    if md_files.is_empty() {
        return;
    }

    eprintln!();
    if !prompt_yes_no(&format!(
        "Найдено {} .md файлов (включая вложенные папки). Импортировать все? [y/N]: ",
        md_files.len()
    )) {
        return;
    }

    let emb = match embed::build(cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "warning: не удалось собрать эмбеддер — импорт пропущен: {}",
                e.msg
            );
            return;
        }
    };
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "warning: не удалось открыть хранилище — импорт пропущен: {}",
                e.msg
            );
            return;
        }
    };
    let imports_dir = folder.join("imports");
    let mut imported_paths: Vec<&PathBuf> = Vec::new();

    for path in &md_files {
        match import::import_file(&store, emb.as_ref(), cfg, path, "", &imports_dir) {
            Ok(res) => {
                imported_paths.push(path);
                print_line(&json!({
                    "imported": path.to_string_lossy(),
                    "chunks": res.chunks,
                }));
            }
            Err(e) => {
                eprintln!(
                    "warning: не удалось импортировать {}: {}",
                    path.display(),
                    e.msg
                );
            }
        }
    }
    let _ = store.log_op(
        "init-import",
        Some(&format!("{} files", imported_paths.len())),
    );

    if imported_paths.is_empty() {
        return;
    }

    if prompt_yes_no("Удалить исходные файлы? [y/N]: ") {
        for path in &imported_paths {
            if let Err(e) = std::fs::remove_file(path) {
                eprintln!("warning: не удалось удалить {}: {e}", path.display());
            }
        }
    }
}

/// Recursively collect the Markdown files under `dir` (sorted), skipping `exclude`
/// (the folder we just scaffolded) and ANY other climem memory folder we run into.
/// Without that, an `init` next to an earlier memory folder would re-absorb its
/// `imports/` copies (and a re-run would loop self-import). We match
/// `.md`/`.markdown` case-insensitively, the exact same set `import` treats as
/// Markdown, so a `README.MD` can't get skipped here while import would happily
/// have taken it. No `walkdir` dependency: a small hand-rolled walk keeps the
/// dependency tree minimal (CLAUDE.md).
fn collect_md_files(dir: &Path, exclude: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    walk_md(dir, exclude, &mut paths);
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

/// True if `dir` looks like a climem memory folder — it holds both the derived
/// index (`store.db`) and the config (`config.json`), the pair `init::run` lays
/// down. We prune these wholesale so their `imports/` copies (and `notes/`) never
/// get re-ingested as if they were user docs.
fn is_memory_folder(dir: &Path) -> bool {
    dir.join("store.db").is_file() && dir.join("config.json").is_file()
}

/// One directory level of the walk: append its `.md` files, then recurse into its
/// subdirectories (depth-first). The freshly-scaffolded `exclude` folder and any
/// other memory folder are pruned wholesale. Any unreadable entry is silently
/// skipped — collection is best-effort.
fn walk_md(dir: &Path, exclude: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // Prune our own scaffold and any sibling memory folder; don't follow
            // symlinked dirs (file_type() reflects the link, so is_dir() is false
            // for them — guards against loops / escaping the docs tree).
            if path == exclude || is_memory_folder(&path) {
                continue;
            }
            walk_md(&path, exclude, out);
        } else if ft.is_file() && is_md(&path) {
            out.push(path);
        }
    }
}

/// Print `question` to stderr and read one line back from stdin. Returns `true`
/// for a yes, and `false` for a no, EOF, or a read error, so piped/non-interactive
/// use safely declines.
fn prompt_yes_no(question: &str) -> bool {
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
    fn collect_md_files_missing_dir_is_empty() {
        assert!(collect_md_files(Path::new("/no/such/dir/here"), &no_exclude()).is_empty());
    }

    /// Build init args targeting `tmp`, returning the produced folder path too.
    fn run_init(tmp: &TempDir, extra: &[&str]) -> (Result<()>, PathBuf) {
        let mut args = vec!["init", tmp.path().to_str().unwrap()];
        args.extend_from_slice(extra);
        let name = extra
            .windows(2)
            .find(|w| w[0] == "--name")
            .map(|w| w[1])
            .unwrap_or(".memory");
        (run(&parsed(&args)), tmp.path().join(name))
    }

    #[test]
    fn init_missing_target_errors_with_hint() {
        let err = run(&parsed(&["init"])).unwrap_err();
        assert!(err.msg.contains("init needs a target"));
        assert!(err.hint.as_deref().unwrap().contains("cm init"));
    }

    #[test]
    fn init_creates_scaffold_local() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        assert!(folder.join("config.json").exists());
        assert!(folder.join("store.db").exists());
        assert!(folder.join("models").is_dir());
        assert!(folder.join("notes").is_dir()); // source of truth: notes
        assert!(folder.join("imports").is_dir()); // source of truth: imports
        assert!(Config::load(&folder.join("config.json")).is_ok());
        // .gitignore ignores derived/binary, keeps truth (notes/, imports/, config.json).
        let gi = std::fs::read_to_string(folder.join(".gitignore")).unwrap();
        assert!(gi.contains("store.db"));
        assert!(gi.contains("cm.exe") && gi.contains("cm\n"));
        assert!(!gi.contains("notes/") && !gi.contains("imports/") && !gi.contains("config.json"));
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
    fn init_idempotent_on_existing_folder() {
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join("m");
        std::fs::create_dir_all(&folder).unwrap();
        let sentinel = folder.join("sentinel.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let (res, _) = run_init(&tmp, &["--name", "m"]);
        res.unwrap();
        assert!(sentinel.exists()); // untouched
        assert!(!folder.join("store.db").exists()); // nothing scaffolded
    }

    #[test]
    fn init_bad_dimension_errors() {
        let tmp = TempDir::new().unwrap();
        let (res, folder) = run_init(&tmp, &["--name", "m", "--dimension", "abc"]);
        assert!(res.unwrap_err().msg.contains("must be a number"));
        assert!(!folder.exists()); // folder not created on early error
    }

    #[test]
    fn init_default_name_is_dot_memory() {
        let tmp = TempDir::new().unwrap();
        let (res, _) = run_init(&tmp, &[]);
        res.unwrap();
        assert!(tmp.path().join(".memory").join("store.db").exists());
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
        let cfg = Config::load(&folder.join("config.json")).unwrap();
        assert_eq!(cfg.name, "m");
        assert_eq!(cfg.embedding.model, "custom-model");
        assert_eq!(cfg.embedding.dimension, 256);
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
