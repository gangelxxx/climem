//! Importing a document: pull out the text, chunk it along its structure, embed
//! each chunk, and index it with a back-reference to where it came from (desc.md §6).

use crate::chunk::{self, Chunk};
use crate::config::Config;
use crate::embed::Embedder;
use crate::store::Store;
use crate::util::{content_hash_hex, mtime_secs, now, AppError, Result};
use std::path::Path;

#[derive(Debug)]
pub struct ImportResult {
    pub chunks: usize,
}

/// Import a document. We copy the ORIGINAL into `imports/` as the source of truth
/// (desc.md §3/§7), register it in the import registry (which sits alongside the
/// truth), then derive and index its chunks. Importing the same file twice is a
/// no-op; importing a changed file replaces its chunks. The chunks hold nothing
/// unique, since `reindex` can rebuild them from `imports/` any time.
pub fn import_file(
    store: &Store,
    embedder: &dyn Embedder,
    cfg: &Config,
    path: &Path,
    tags: &str,
    imports_dir: &Path,
) -> Result<ImportResult> {
    if !path.exists() {
        return Err(AppError::with_hint(
            format!("file not found: {}", path.display()),
            "cm import ./docs/architecture.md --tags spec,architecture",
        ));
    }
    let bytes = std::fs::read(path)?;
    let file_hash = content_hash_hex(&bytes);
    let orig_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("source")
        .to_string();

    // Bail on a file we can't get text out of (e.g. an empty one) BEFORE touching
    // anything, so a junk import never leaves a trace in imports/ or the registry.
    if chunks_for(path, &orig_name, cfg)?.is_empty() {
        return Err(AppError::new(format!(
            "no text extracted from {}",
            path.display()
        )));
    }

    // Copy the original into imports/ as the truth (this is the commit point)
    // BEFORE we embed anything, so a failure mid-embed is recoverable by `reindex`.
    std::fs::create_dir_all(imports_dir)?;
    let dest_name = canonical_import_name(imports_dir, &orig_name, &bytes)?;
    let dest = imports_dir.join(&dest_name);
    if !file_has_bytes(&dest, &bytes) {
        std::fs::write(&dest, &bytes)?;
    }
    let source = format!("imports/{dest_name}");

    // The sidecar is what makes imports/ self-describing truth: the original name
    // and tags (neither of which you can recover from the file itself) survive even
    // a full store.db deletion, so `reindex` restores both the content and the
    // metadata (desc.md §10).
    write_sidecar(imports_dir, &dest_name, &orig_name, tags)?;

    store.record_import(&source, &orig_name, tags, 0, &file_hash)?;
    let chunks = index_import(
        store,
        Some(embedder),
        cfg,
        &source,
        &orig_name,
        &dest,
        tags,
        &file_hash,
    )?;
    store.record_import(&source, &orig_name, tags, chunks as i64, &file_hash)?;
    let mtime = mtime_secs(&dest);
    store.file_state_set(&source, "import", &source, &file_hash, mtime)?;

    Ok(ImportResult { chunks })
}

/// (Re)derive and index all the chunk rows for ONE import from its `imports/`
/// original. Both `import` and `reindex` go through here. Chunk ids are
/// content-addressed (`content_hash(file_hash#n)`), so a rebuild lands on exactly
/// the same ids. We clear out any existing chunks for this source first, which
/// makes it idempotent.
#[allow(clippy::too_many_arguments)]
pub fn index_import(
    store: &Store,
    embedder: Option<&dyn Embedder>,
    cfg: &Config,
    source: &str,
    orig_name: &str,
    file_path: &Path,
    tags: &str,
    file_hash: &str,
) -> Result<usize> {
    let chunks = chunks_for(file_path, orig_name, cfg)?;
    if chunks.is_empty() {
        return Err(AppError::new(format!(
            "no text extracted from {}",
            file_path.display()
        )));
    }
    store.delete_chunks_for_source(source)?;
    let (epoch, iso) = now();
    for (n, ch) in chunks.iter().enumerate() {
        let origin = if ch.origin.is_empty() {
            orig_name.to_string()
        } else {
            format!("{orig_name} {}", ch.origin)
        };
        // An FTS-only reindex (no embedder) stores an empty vector; the vector
        // channel just contributes nothing until a later reindex with an embedder.
        let vec = match embedder {
            Some(e) => e.embed(&ch.text)?,
            None => Vec::new(),
        };
        let id = content_hash_hex(format!("{file_hash}#{n}").as_bytes());
        store.upsert_note(
            &id,
            &ch.text,
            tags,
            Some(source),
            Some(&origin),
            "chunk",
            epoch,
            &iso,
            &vec,
        )?;
    }
    Ok(chunks.len())
}

/// Chunk a file based on its extension (the dispatch shared by import and reindex).
fn chunks_for(path: &Path, orig_name: &str, cfg: &Config) -> Result<Vec<Chunk>> {
    let ext = Path::new(orig_name)
        .extension()
        .or_else(|| path.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let max = cfg.chunking.max_tokens;
    let overlap = cfg.chunking.overlap;
    Ok(match ext.as_str() {
        "md" | "markdown" => chunk::markdown(&read_utf8(path)?, max, overlap),
        "txt" | "text" | "" => chunk::text(&read_utf8(path)?, max, overlap),
        "html" | "htm" => chunk::text(&html_to_text(&read_utf8(path)?), max, overlap),
        "pdf" => pdf_chunks(path, max, overlap)?,
        other => {
            return Err(AppError::with_hint(
                format!("unsupported file type: .{other}"),
                "Supported: .md .txt .html .pdf — add a parser to extend.",
            ))
        }
    })
}

/// Pick the `imports/` filename for an original. We keep its name as-is, unless
/// something with the same name but DIFFERENT bytes is already there, in which
/// case we tack on a short content hash so neither file gets clobbered.
fn canonical_import_name(imports_dir: &Path, orig_name: &str, bytes: &[u8]) -> Result<String> {
    let candidate = imports_dir.join(orig_name);
    if !candidate.exists() || file_has_bytes(&candidate, bytes) {
        return Ok(orig_name.to_string());
    }
    let p = Path::new(orig_name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("source");
    let h = &content_hash_hex(bytes)[..8];
    Ok(match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{h}.{ext}"),
        None => format!("{stem}-{h}"),
    })
}

fn file_has_bytes(path: &Path, bytes: &[u8]) -> bool {
    std::fs::read(path).map(|b| b == bytes).unwrap_or(false)
}

/// Sidecar path for an import original: `imports/<name>.meta.json`.
fn sidecar_path(imports_dir: &Path, name: &str) -> std::path::PathBuf {
    imports_dir.join(format!("{name}.meta.json"))
}

/// True if `name` is a sidecar (so `reindex` skips it when scanning originals).
pub fn is_sidecar(name: &str) -> bool {
    name.ends_with(".meta.json")
}

fn write_sidecar(imports_dir: &Path, name: &str, orig: &str, tags: &str) -> Result<()> {
    let v = serde_json::json!({ "orig": orig, "tags": tags });
    std::fs::write(sidecar_path(imports_dir, name), serde_json::to_string(&v)?)?;
    Ok(())
}

/// Read an import's `(orig_name, tags)` back out of its sidecar. If there's no
/// sidecar (someone dropped a file into imports/ by hand), fall back to the
/// filename and empty tags.
pub fn read_sidecar(imports_dir: &Path, name: &str) -> (String, String) {
    if let Ok(text) = std::fs::read_to_string(sidecar_path(imports_dir, name)) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let orig = v.get("orig").and_then(|x| x.as_str()).unwrap_or(name);
            let tags = v.get("tags").and_then(|x| x.as_str()).unwrap_or("");
            return (orig.to_string(), tags.to_string());
        }
    }
    (name.to_string(), String::new())
}

fn read_utf8(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A lightweight HTML-to-text pass: throw away script/style blocks, strip the
/// tags, and decode the common entities. It's good enough for indexing, not a
/// real readability extraction.
fn html_to_text(html: &str) -> String {
    let mut s = strip_block(html, "<script", "</script>");
    s = strip_block(&s, "<style", "</style>");

    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_entities(&out)
}

fn strip_block(s: &str, open: &str, close: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        if let Some(rel) = lower[i..].find(open) {
            let start = i + rel;
            out.push_str(&s[i..start]);
            if let Some(rel_end) = lower[start..].find(close) {
                i = start + rel_end + close.len();
            } else {
                break;
            }
        } else {
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&ldquo;", "“")
        .replace("&rdquo;", "”")
        .replace("&lsquo;", "‘")
        .replace("&rsquo;", "’")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(feature = "pdf")]
fn pdf_chunks(path: &Path, max: usize, overlap: usize) -> Result<Vec<Chunk>> {
    let text = pdf_extract::extract_text(path)
        .map_err(|e| AppError::new(format!("pdf extraction failed: {e}")))?;
    Ok(chunk::text(&text, max, overlap))
}

#[cfg(not(feature = "pdf"))]
fn pdf_chunks(_path: &Path, _max: usize, _overlap: usize) -> Result<Vec<Chunk>> {
    Err(AppError::with_hint(
        "PDF import is not built into this binary",
        "Rebuild with `cargo build --release --features pdf`, or convert the PDF to .md/.txt first.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{self, Embedder};
    use std::cell::Cell;
    use tempfile::TempDir;

    fn mem() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn local_emb() -> Box<dyn Embedder> {
        embed::build(&Config::default()).unwrap()
    }

    /// The `imports/` truth directory inside a test's temp folder.
    fn imp(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("imports")
    }

    /// Write `content` to `<tmp>/<name>` and return the path.
    fn write_file(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    // ---- HTML helpers (unit) -------------------------------------------

    #[test]
    fn html_to_text_strips_tags_and_decodes_entities() {
        let out = html_to_text("<p>Hello&nbsp;&amp;&lt;world&gt;</p>");
        assert!(!out.contains("<p>"));
        assert!(out.contains("Hello"));
        assert!(out.contains('&')); // from &amp;
        assert!(out.contains("<world>")); // from &lt;world&gt;
    }

    #[test]
    fn html_to_text_removes_script_and_style_blocks() {
        let out =
            html_to_text("<STYLE>body{color:red}</STYLE><script>alert(1)</script><p>keep</p>");
        assert!(out.contains("keep"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("color"));
    }

    #[test]
    fn strip_block_handles_unclosed_open_tag() {
        // Open tag with no closing tag -> the remainder is dropped, no panic.
        assert_eq!(
            strip_block("before<script>after", "<script", "</script>"),
            "before"
        );
    }

    #[test]
    fn decode_entities_amp_ordering() {
        // &amp; is decoded LAST, so &amp;lt; -> &lt; (not <).
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
    }

    // ---- import_file (integration) -------------------------------------

    #[test]
    fn import_missing_file_errors_with_hint() {
        let store = mem();
        let err = import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            Path::new("/no/such/file.md"),
            "",
            Path::new("imports"),
        )
        .unwrap_err();
        assert!(err.msg.contains("file not found"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn import_unsupported_extension_errors() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "data.docx", "anything");
        let err = import_file(
            &mem(),
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap_err();
        assert!(err.msg.contains("unsupported file type: .docx"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn import_empty_file_no_text_extracted() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "empty.txt", "   \n  ");
        let store = mem();
        let err = import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap_err();
        assert!(err.msg.contains("no text extracted"));
        assert!(store.all().unwrap().is_empty());
        assert!(store.list_imports().unwrap().is_empty());
    }

    #[test]
    fn import_md_indexes_chunks_and_records_import() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "doc.md", "# Intro\nhello\n## Auth\nworld");
        let store = mem();
        let res = import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "spec",
            &imp(&dir),
        )
        .unwrap();
        assert!(res.chunks > 0);
        assert_eq!(store.all().unwrap().len(), res.chunks);
        // The original is preserved under imports/ as the source of truth.
        assert!(imp(&dir).join("doc.md").exists());
        let imps = store.list_imports().unwrap();
        assert_eq!(imps[0].chunks as usize, res.chunks);
        assert_eq!(imps[0].source, "imports/doc.md");
        assert_eq!(imps[0].tags, "spec");
        assert!(store
            .all()
            .unwrap()
            .iter()
            .any(|r| r.origin.as_deref().unwrap().contains('›')));
    }

    #[test]
    fn import_txt_uses_paragraph_origin() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "notes.txt", "para one here\n\npara two here");
        let store = mem();
        let res = import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap();
        assert_eq!(store.all().unwrap().len(), res.chunks);
        assert!(store
            .all()
            .unwrap()
            .iter()
            .any(|r| r.origin.as_deref().unwrap().contains('#')));
    }

    #[test]
    fn import_origin_and_source_backref() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "ref.md", "# H\nbody");
        let store = mem();
        import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap();
        let row = &store.all().unwrap()[0];
        // source now points at the imports/ copy (the truth), not the caller path.
        assert_eq!(row.source.as_deref(), Some("imports/ref.md"));
        assert!(row.origin.as_deref().unwrap().starts_with("ref.md"));
    }

    #[test]
    fn import_no_extension_treated_as_text() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "noext", "some plain words here");
        let res = import_file(
            &mem(),
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn import_uppercase_extension_md() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "README.MD", "# Title\nbody text");
        let store = mem();
        import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap();
        assert!(store
            .all()
            .unwrap()
            .iter()
            .any(|r| r.origin.as_deref().unwrap().contains('›')));
    }

    #[test]
    fn import_html_strips_markup_before_chunking() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "page.html",
            "<script>alert(1)</script><h1>Title</h1><p>real body text</p>",
        );
        let store = mem();
        import_file(
            &store,
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap();
        let body = store.all().unwrap()[0].body.clone();
        assert!(body.contains("real body text"));
        assert!(!body.contains("alert"));
        assert!(!body.contains("<p>"));
    }

    #[test]
    fn import_oversize_section_splits_with_overlap() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "big.md", "# Big\nw1 w2 w3 w4 w5 w6 w7 w8 w9 w10");
        let mut cfg = Config::default();
        cfg.chunking.max_tokens = 5;
        cfg.chunking.overlap = 1;
        let store = mem();
        let res = import_file(&store, local_emb().as_ref(), &cfg, &path, "", &imp(&dir)).unwrap();
        assert!(res.chunks > 1);
        assert!(store.all().unwrap().iter().any(|r| r
            .origin
            .as_deref()
            .unwrap()
            .contains("(part")));
    }

    /// An embedder that works `limit` times and then starts failing, so we can
    /// exercise the partial-state path (there's no transaction around the chunks).
    struct FailAfter {
        n: Cell<usize>,
        limit: usize,
    }
    impl Embedder for FailAfter {
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            let c = self.n.get();
            self.n.set(c + 1);
            if c >= self.limit {
                Err(AppError::new("synthetic embedder failure"))
            } else {
                Ok(vec![0.0; 8])
            }
        }
        fn dim(&self) -> usize {
            8
        }
        fn signature(&self) -> String {
            "fail".into()
        }
    }

    #[test]
    fn import_embedder_error_preserves_original_for_reindex() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "two.md", "# A\nfirst\n## B\nsecond");
        let store = mem();
        let emb = FailAfter {
            n: Cell::new(0),
            limit: 1,
        };
        let err = import_file(&store, &emb, &Config::default(), &path, "", &imp(&dir));
        assert!(err.is_err());
        // Recoverable by design: the original is still under imports/ and the
        // import is registered (with its tags), so `reindex` can rebuild the chunks.
        // There's no transaction, so a half-written chunk might linger, but that's
        // fine; reindex replaces them deterministically.
        assert!(imp(&dir).join("two.md").exists());
        assert!(store.import_record("imports/two.md").unwrap().is_some());
    }

    #[cfg(not(feature = "pdf"))]
    #[test]
    fn import_pdf_disabled_errors() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "doc.pdf", "%PDF-1.4 fake");
        let err = import_file(
            &mem(),
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap_err();
        assert!(err.msg.contains("PDF import is not built"));
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn import_pdf_enabled_rejects_invalid_pdf() {
        // With the feature on, the .pdf arm runs the extractor, and a non-PDF file
        // comes back as a "pdf extraction failed" error. (Testing the happy path
        // would need a real PDF fixture, which is out of scope here.)
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "bad.pdf", "this is not a pdf at all");
        let err = import_file(
            &mem(),
            local_emb().as_ref(),
            &Config::default(),
            &path,
            "",
            &imp(&dir),
        )
        .unwrap_err();
        assert!(err.msg.contains("pdf extraction failed"));
    }
}
