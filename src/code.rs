//! `code`: a source-code knowledge graph, kept DELIBERATELY SEPARATE from the
//! notes graph (it lives in the `code_*` tables and is reached only through
//! `cm map`). For each source file we run tree-sitter over the syntax tree and
//! extract a stream of tags via the grammar's own `tags.scm` query — the same
//! "tagging" mechanism GitHub uses for go-to-definition — using the
//! `tree-sitter-tags` engine. A `@definition.*` tag becomes a symbol node plus a
//! `defines` edge (file → symbol); a `@reference.*` tag becomes a `uses` edge
//! that we resolve by name against the symbol table (dangling when unresolved,
//! the same first-class-dangling trick as the notes graph in `graph.rs`).
//!
//! Everything here is pure: it turns bytes into `CodeParse`. The store wiring and
//! name-resolution pass live in `commands` (mirroring how `graph.rs` pairs with
//! `commands::index_note_edges`). The whole tree-sitter path is behind the `code`
//! cargo feature; without it `parse` returns a self-healing rebuild hint, exactly
//! like `import::pdf_chunks` does for the `pdf` feature.
//!
//! Adding a language is one crate in `Cargo.toml` + one row in `LANGUAGES` below;
//! the extraction code never changes, because every grammar exposes a `LANGUAGE`
//! constant and a `TAGS_QUERY`, and the tag stream is uniform across languages. A
//! grammar whose shipped `tags.scm` is thin can append a supplemental query in its
//! registry row (see TypeScript) without touching the extractor.

use crate::util::{content_hash_hex, AppError, Result};
use std::path::Path;

/// A symbol DEFINITION found in a file: its name, the tag's syntax kind
/// (`function`, `struct`, `class`, `type`, ...), the 1-based line, the trimmed
/// first line of source as a human-facing signature, and whether it sits in test
/// code (so listings can hide it by default — test symbols are noise when you're
/// learning a module's real API).
#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub signature: String,
    pub is_test: bool,
}

/// A symbol REFERENCE (a call / use site): the referenced name and the 1-based
/// line it occurs on. Resolution to a concrete definition happens later, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub name: String,
    pub line: usize,
}

/// The result of parsing one source file: its definitions and references.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeParse {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

/// Content-addressed id for a symbol definition. Stable across rebuilds (depends
/// only on where/what the symbol is, not on insertion order), so re-`map`ping an
/// unchanged file reproduces identical ids — the same property chunk ids rely on.
pub fn symbol_id(path: &str, kind: &str, name: &str, line: usize) -> String {
    content_hash_hex(format!("{path}\0{kind}\0{name}\0{line}").as_bytes())
}

/// Ubiquitous stdlib / built-in method names (iterator combinators, Option/Result
/// helpers, conversions, common collection ops) that appear as `uses` references
/// everywhere. Because `uses` resolves by name with no scope analysis, a project
/// symbol that happens to be named `map`/`get`/`new` would otherwise swallow every
/// `.map()`/`.get()`/`::new()` call as a false dependency. We refuse to resolve
/// these names to a project symbol — they stay external — so `--calls`/`--uses`
/// surface real, in-project relationships, not language noise. (They're still
/// visible under `--calls --external` as unresolved names.)
pub fn is_stdlib_combinator(name: &str) -> bool {
    matches!(
        name,
        // iterator / option / result combinators
        "map" | "filter" | "filter_map" | "flat_map" | "flatten" | "fold" | "reduce"
            | "for_each" | "collect" | "zip" | "chain" | "take" | "skip" | "rev"
            | "enumerate" | "find" | "any" | "all" | "count" | "sum" | "product"
            | "min" | "max" | "sort" | "sort_by" | "position" | "last" | "next"
            | "and_then" | "or_else" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            // unwrapping / conversion
            | "unwrap" | "expect" | "clone" | "into" | "from" | "to_string" | "to_owned"
            | "as_str" | "as_ref" | "as_deref" | "as_mut" | "as_bytes" | "to_vec"
            | "parse" | "try_into" | "try_from" | "borrow"
            // common collection / string ops
            | "push" | "pop" | "insert" | "remove" | "get" | "get_mut" | "len" | "is_empty"
            | "contains" | "iter" | "iter_mut" | "into_iter" | "keys" | "values" | "entry"
            | "split" | "join" | "trim" | "replace" | "starts_with" | "ends_with"
            | "to_lowercase" | "to_uppercase" | "chars" | "lines" | "format"
            // Option/Result constructors that are everywhere
            | "Some" | "Ok" | "Err" | "None"
    )
}

/// The registry name of the language a path maps to (by extension), or `None` if
/// we don't index that extension. Kept as a pure string lookup so the dispatch is
/// testable without the `code` feature compiled in.
pub fn lang_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.exts.contains(&ext.as_str()))
        .map(|l| l.name)
}

/// True if we have a grammar for this path's extension.
pub fn is_source_file(path: &Path) -> bool {
    lang_for_path(path).is_some()
}

/// Human-friendly display name for a registry language id, for progress/summary
/// output (`csharp` -> "C#", `cpp` -> "C++"). Unknown/unmapped ids capitalize
/// their first letter so a new grammar still reads acceptably without a table edit.
pub fn display_lang(id: &str) -> String {
    match id {
        "csharp" => "C#".to_string(),
        "cpp" => "C++".to_string(),
        "c" => "C".to_string(),
        "php" => "PHP".to_string(),
        "javascript" => "JavaScript".to_string(),
        "typescript" => "TypeScript".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// One language in the registry: its canonical name, the file extensions it owns,
/// and (under the `code` feature) the grammar + a function building its tagging
/// query. The query is a function (not a `&str`) so a language can APPEND a
/// supplemental query to the grammar's upstream `TAGS_QUERY` — see TypeScript,
/// whose shipped `tags.scm` only covers declaration forms (`.d.ts`) and misses
/// plain `function`/`class`/calls.
pub struct LangDef {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    #[cfg(feature = "code")]
    pub language: fn() -> tree_sitter::Language,
    #[cfg(feature = "code")]
    pub tags_query: fn() -> String,
}

/// Supplemental tags for TypeScript: the upstream `tags.scm` only tags signatures
/// and abstract/interface declarations, so a normal `.ts` file (concrete functions,
/// classes, calls) yields nothing. We add the concrete forms — these node types all
/// exist in the TS grammar, they're just absent from its query.
#[cfg(feature = "code")]
const TS_TAGS_EXTRA: &str = r#"
(function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(method_definition name: (property_identifier) @name) @definition.method
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (member_expression property: (property_identifier) @name)) @reference.call
"#;

/// The supported languages. A path's extension picks the row; the row's grammar
/// and `tags.scm` drive extraction. Extensions are lowercase and must stay unique
/// across rows (first match wins, but we keep them disjoint).
///
/// Markup-only grammars (css/html) and ones that ship no `tags.scm` (bash) are
/// intentionally excluded — they have no meaningful symbol graph.
pub const LANGUAGES: &[LangDef] = &[
    LangDef {
        name: "rust",
        exts: &["rs"],
        #[cfg(feature = "code")]
        language: || tree_sitter_rust::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_rust::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "python",
        exts: &["py", "pyi"],
        #[cfg(feature = "code")]
        language: || tree_sitter_python::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_python::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "javascript",
        exts: &["js", "jsx", "mjs", "cjs"],
        #[cfg(feature = "code")]
        language: || tree_sitter_javascript::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_javascript::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "typescript",
        exts: &["ts", "tsx", "mts", "cts"],
        #[cfg(feature = "code")]
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        #[cfg(feature = "code")]
        // Upstream TS tags.scm is declaration-only; append the concrete forms.
        tags_query: || format!("{}{}", tree_sitter_typescript::TAGS_QUERY, TS_TAGS_EXTRA),
    },
    LangDef {
        name: "go",
        exts: &["go"],
        #[cfg(feature = "code")]
        language: || tree_sitter_go::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_go::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "java",
        exts: &["java"],
        #[cfg(feature = "code")]
        language: || tree_sitter_java::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_java::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "c",
        exts: &["c", "h"],
        #[cfg(feature = "code")]
        language: || tree_sitter_c::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_c::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "cpp",
        exts: &["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        #[cfg(feature = "code")]
        language: || tree_sitter_cpp::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_cpp::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "csharp",
        exts: &["cs"],
        #[cfg(feature = "code")]
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_c_sharp::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "ruby",
        exts: &["rb"],
        #[cfg(feature = "code")]
        language: || tree_sitter_ruby::LANGUAGE.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_ruby::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "php",
        exts: &["php"],
        #[cfg(feature = "code")]
        language: || tree_sitter_php::LANGUAGE_PHP.into(),
        #[cfg(feature = "code")]
        tags_query: || tree_sitter_php::TAGS_QUERY.to_string(),
    },
];

/// Parse one source file's text into definitions and references. `path` is the
/// file's (relative) path — used only to tell test code from product code; `lang`
/// is a registry name from `lang_for_path`. Errors only on a genuinely broken
/// grammar query (a build-time bug, not user input); a file with no tags yields an
/// empty `CodeParse`.
#[cfg(feature = "code")]
pub fn parse(path: &str, lang: &str, src: &str) -> Result<CodeParse> {
    use tree_sitter_tags::{TagsConfiguration, TagsContext};

    let def = LANGUAGES
        .iter()
        .find(|l| l.name == lang)
        .ok_or_else(|| AppError::new(format!("no grammar registered for language '{lang}'")))?;

    let query = sanitize_tags_query(&(def.tags_query)());
    let config = TagsConfiguration::new((def.language)(), &query, "")
        .map_err(|e| AppError::new(format!("tags query for '{lang}' is invalid: {e}")))?;
    let mut ctx = TagsContext::new();
    let bytes = src.as_bytes();
    let (tags, _) = ctx
        .generate_tags(&config, bytes, None)
        .map_err(|e| AppError::new(format!("tag generation failed for '{lang}': {e}")))?;

    // Whole-file test files (tests/…, *_test.go, test_*.py, *.test.ts, …) make every
    // symbol a test symbol; otherwise a symbol is a test only if it sits inside an
    // inline test module/region (see `inline_test_from`).
    let file_is_test = path_is_test(path);
    let inline_test_line = if file_is_test {
        Some(1) // everything counts as test
    } else {
        inline_test_from(src)
    };

    let mut out = CodeParse::default();
    for tag in tags {
        let tag = match tag {
            Ok(t) => t,
            Err(_) => continue, // a bad single tag never sinks the whole file
        };
        let name = match src.get(tag.name_range.clone()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let line = tag.span.start.row + 1; // tree-sitter rows are 0-based
        if tag.is_definition {
            let kind = config.syntax_type_name(tag.syntax_type_id).to_string();
            let signature = src
                .get(tag.line_range.clone())
                .map(|l| crate::util::preview(l, 120))
                .unwrap_or_default();
            let is_test = inline_test_line.is_some_and(|start| line >= start);
            out.defs.push(Def {
                name,
                kind,
                line,
                signature,
                is_test,
            });
        } else {
            out.refs.push(Ref { name, line });
        }
    }
    Ok(out)
}

/// The capture-name prefixes `tree-sitter-tags` accepts in a tags query. Anything
/// else (e.g. C#'s bare `@module`, which is `@definition.module` written wrong)
/// makes `TagsConfiguration::new` reject the WHOLE query, so that language indexes
/// nothing. See the upstream allow-list in the error it raises:
///   "Expected one of: @definition.*, @reference.*, @doc, @name, @local.(...)".
#[cfg(feature = "code")]
const ALLOWED_TAG_CAPTURES: &[&str] = &[
    "@definition.",
    "@reference.",
    "@local.",
    "@name",
    "@doc",
    "@ignore",
];

/// Drop any tags-query pattern that ends in a capture `tree-sitter-tags` doesn't
/// understand, so one bad stanza can't sink the whole grammar. We split on the
/// blank lines that separate stanzas in every shipped `tags.scm`, and a stanza is
/// dropped only if its LAST top-level `@capture` (the one classifying the match) is
/// not in `ALLOWED_TAG_CAPTURES`. Inner `@name` captures are fine; comments and the
/// well-formed stanzas pass through untouched. The concrete bug this fixes: the C#
/// grammar's `(namespace_declaration …) @module` (a stray duplicate of the valid
/// `@definition.module` line right above it) — dropping it leaves namespaces still
/// tagged via that valid line.
#[cfg(feature = "code")]
fn sanitize_tags_query(query: &str) -> String {
    // Stanzas are separated by blank lines. Keep a stanza unless its classifying
    // capture (the last @token in it) is disallowed.
    let kept: Vec<&str> = query
        .split("\n\n")
        .filter(|stanza| {
            let last_capture = stanza
                .split_whitespace()
                .filter(|tok| tok.starts_with('@'))
                .next_back();
            match last_capture {
                // No capture at all (blank/comment-only) — harmless, keep it.
                None => true,
                Some(cap) => {
                    // Strip a trailing ')' the capture may butt against, then check.
                    let cap = cap.trim_end_matches(')');
                    ALLOWED_TAG_CAPTURES
                        .iter()
                        .any(|ok| cap == *ok || cap.starts_with(ok))
                }
            }
        })
        .collect();
    kept.join("\n\n")
}

/// Without the `code` feature, source-code indexing isn't built into the binary.
/// Self-healing error (same shape as `import::pdf_chunks`): tell the caller how to
/// get it. Keeping the signature identical means `commands::map` compiles either way.
#[cfg(not(feature = "code"))]
pub fn parse(_path: &str, _lang: &str, _src: &str) -> Result<CodeParse> {
    Err(AppError::with_hint(
        "source-code indexing is not built into this binary",
        "Rebuild with `cargo build --release --features code`.",
    ))
}

/// True when a path is a whole test file by common convention across the supported
/// languages: a `tests/` directory segment (Rust integration tests, Python/JS test
/// trees), `_test.go` / `Test.java`, `test_*.py` / `*_test.py`, or a
/// `*.test|spec.{ts,tsx,js,jsx}` file (JS/TS). Cheap string checks, no parsing.
/// Only the feature-gated `parse` consults it, so it's gated too (with its test).
#[cfg(feature = "code")]
fn path_is_test(path: &str) -> bool {
    let p = path.replace('\\', "/").to_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.split('/')
        .any(|seg| seg == "tests" || seg == "test" || seg == "__tests__")
        || file.ends_with("_test.go")
        || file.ends_with("test.java")
        || file.starts_with("test_") && file.ends_with(".py")
        || file.ends_with("_test.py")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.tsx")
        || file.ends_with(".test.js")
        || file.ends_with(".test.jsx")
        || file.ends_with(".spec.ts")
        || file.ends_with(".spec.js")
}

/// The 1-based line at which an INLINE test MODULE begins, or `None`. Rust
/// convention puts `#[cfg(test)] mod tests { … }` at the end of a file and that
/// module holds the file's unit tests, so the first `mod tests`/`mod test` line
/// marks the boundary: every definition at/after it is test code.
///
/// We deliberately key off the `mod tests` line, NOT a bare `#[cfg(test)]`
/// attribute. A `#[cfg(test)]` can sit on a single product-adjacent item (e.g. a
/// test-only `insert_note` helper inside `impl Store`) with real product code
/// AFTER it; treating "everything past the first #[cfg(test)]" as test wrongly
/// buried ~64% of this very repo's symbols. The trade-off: a lone `#[cfg(test)]`
/// item outside a test module is seen as product code — rare and harmless next to
/// the win of getting the common `mod tests` layout right. No brace matching;
/// whole-file detection (`path_is_test`) covers separate test files.
#[cfg(feature = "code")]
fn inline_test_from(src: &str) -> Option<usize> {
    let mut line_no = 0usize;
    for line in src.lines() {
        line_no += 1;
        let t = line.trim_start();
        // Match a test-module declaration: `mod tests`, `pub mod tests`, `mod test`
        // (with the next char a space/brace/EOL so `mod testing` doesn't match).
        let after_mod = t
            .strip_prefix("pub ")
            .unwrap_or(t)
            .strip_prefix("mod ")
            .map(str::trim_start);
        if let Some(rest) = after_mod {
            let name_ok = rest == "tests"
                || rest == "test"
                || rest.starts_with("tests ")
                || rest.starts_with("tests{")
                || rest.starts_with("test ")
                || rest.starts_with("test{");
            if name_ok {
                return Some(line_no);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn lang_for_path_maps_known_extensions_case_insensitively() {
        assert_eq!(lang_for_path(Path::new("src/store.rs")), Some("rust"));
        assert_eq!(lang_for_path(Path::new("a/b/main.PY")), Some("python"));
        assert_eq!(lang_for_path(Path::new("app.tsx")), Some("typescript"));
        assert_eq!(lang_for_path(Path::new("x.cpp")), Some("cpp"));
        assert_eq!(lang_for_path(Path::new("README.md")), None);
        assert_eq!(lang_for_path(Path::new("no_extension")), None);
    }

    #[test]
    fn is_source_file_agrees_with_lang_for_path() {
        assert!(is_source_file(Path::new("x.go")));
        assert!(!is_source_file(Path::new("x.txt")));
    }

    #[test]
    fn display_lang_pretties_known_ids_and_capitalizes_others() {
        assert_eq!(display_lang("csharp"), "C#");
        assert_eq!(display_lang("cpp"), "C++");
        assert_eq!(display_lang("typescript"), "TypeScript");
        assert_eq!(display_lang("rust"), "Rust"); // fallback capitalization
        assert_eq!(display_lang("go"), "Go");
    }

    #[test]
    fn symbol_id_is_stable_and_distinct() {
        let a = symbol_id("src/store.rs", "function", "upsert_note", 195);
        let b = symbol_id("src/store.rs", "function", "upsert_note", 195);
        let c = symbol_id("src/store.rs", "function", "upsert_note", 196); // diff line
        assert_eq!(a, b, "same inputs -> same id (rebuild-stable)");
        assert_ne!(a, c, "different line -> different id");
    }

    #[test]
    fn registry_extensions_are_disjoint() {
        // No extension may map to two languages, or dispatch becomes order-dependent.
        let mut seen = std::collections::HashSet::new();
        for lang in LANGUAGES {
            for ext in lang.exts {
                assert!(seen.insert(*ext), "extension '{ext}' is claimed twice");
            }
        }
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_rust_extracts_defs_and_refs() {
        let src = "struct Store { x: i32 }\n\
                   fn upsert(s: &Store) -> i32 { helper(s) }\n\
                   fn helper(s: &Store) -> i32 { s.x }\n";
        let p = parse("src/store.rs", "rust", src).unwrap();
        let names: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Store"));
        assert!(names.contains(&"upsert"));
        assert!(names.contains(&"helper"));
        // `helper(s)` on line 2 is a reference.
        assert!(p.refs.iter().any(|r| r.name == "helper" && r.line == 2));
        // A def carries a usable signature and 1-based line.
        let store = p.defs.iter().find(|d| d.name == "Store").unwrap();
        assert_eq!(store.line, 1);
        assert!(store.signature.contains("Store"));
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_typescript_uses_supplemental_tags_for_concrete_forms() {
        // Regression: the upstream TS tags.scm is declaration-only, so without our
        // TS_TAGS_EXTRA a normal .ts file yields nothing. Assert the concrete
        // function/class/call forms come through.
        let src = "export class Service { run(): number { return helper(); } }\n\
                   function helper(): number { return 42; }\n";
        let p = parse("app.ts", "typescript", src).unwrap();
        assert!(
            p.defs.iter().any(|d| d.name == "Service"),
            "class not tagged"
        );
        assert!(p.defs.iter().any(|d| d.name == "helper"), "fn not tagged");
        assert!(p.refs.iter().any(|r| r.name == "helper"), "call not tagged");
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_python_extracts_class_and_calls() {
        let src = "class Store:\n    pass\n\
                   def upsert(s):\n    return helper(s)\n\
                   def helper(s):\n    return s\n";
        let p = parse("a.py", "python", src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "Store"));
        assert!(p.defs.iter().any(|d| d.name == "upsert"));
        assert!(p.refs.iter().any(|r| r.name == "helper"));
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_csharp_handles_bad_module_capture_in_tags_query() {
        // Regression: the C# grammar's tags.scm ends with a stray bare `@module`
        // capture, which `tree-sitter-tags` rejects — sinking the WHOLE query so no
        // .cs file indexed at all. The sanitizer drops that one stanza; namespace,
        // class and method must still come through (namespace via the valid
        // `@definition.module` line right above the bad one).
        let src = "namespace Foo {\n\
                   \x20 public class Bar {\n\
                   \x20   public void Baz() { Qux(); }\n\
                   \x20 }\n\
                   }\n";
        let p = parse("App.cs", "csharp", src).expect("C# must parse after sanitize");
        assert!(p.defs.iter().any(|d| d.name == "Foo" && d.kind == "module"));
        assert!(p.defs.iter().any(|d| d.name == "Bar" && d.kind == "class"));
        assert!(p.defs.iter().any(|d| d.name == "Baz" && d.kind == "method"));
    }

    #[cfg(feature = "code")]
    #[test]
    fn sanitize_tags_query_drops_only_disallowed_stanzas() {
        // Keeps valid stanzas (definition/reference/name), drops a bare @module.
        let q = "(class_declaration name: (identifier) @name) @definition.class\n\n\
                 (namespace_declaration name: (identifier) @name) @definition.module\n\n\
                 (namespace_declaration name: (identifier) @name) @module\n\n\
                 (invocation_expression (identifier) @name) @reference.send\n";
        let out = sanitize_tags_query(q);
        assert!(out.contains("@definition.class"));
        assert!(out.contains("@definition.module"));
        assert!(out.contains("@reference.send"));
        // The only stanza classified by a bare `@module` is gone.
        assert!(!out.contains("@module\n") && !out.trim_end().ends_with("@module"));
        // A comment-only / blank stanza is harmless and preserved.
        assert_eq!(sanitize_tags_query("; just a comment"), "; just a comment");
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_marks_inline_test_module_symbols_as_test() {
        // Product code above an inline `mod tests` is not test; symbols at/after the
        // module boundary are. Mirrors this repo's own #[cfg(test)] layout.
        let src = "pub fn real() {}\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       fn helper_test() {}\n\
                   }\n";
        let p = parse("src/lib.rs", "rust", src).unwrap();
        let real = p.defs.iter().find(|d| d.name == "real").unwrap();
        assert!(!real.is_test, "product fn must not be test");
        let ht = p.defs.iter().find(|d| d.name == "helper_test").unwrap();
        assert!(ht.is_test, "symbol inside mod tests must be test");
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_bare_cfg_test_attr_does_not_bury_following_product_code() {
        // Regression (caught dogfooding on memory/): a #[cfg(test)] on a single
        // method inside an impl, with REAL product code after it, must NOT mark the
        // rest of the file as test. Only a `mod tests` boundary does that.
        let src = "impl S {\n\
                       #[cfg(test)]\n\
                       fn test_only(&self) {}\n\
                       pub fn product(&self) {}\n\
                   }\n\
                   mod tests {\n\
                       fn real_test() {}\n\
                   }\n";
        let p = parse("src/store.rs", "rust", src).unwrap();
        let product = p.defs.iter().find(|d| d.name == "product").unwrap();
        assert!(
            !product.is_test,
            "product fn after a bare #[cfg(test)] must stay product"
        );
        let rt = p.defs.iter().find(|d| d.name == "real_test").unwrap();
        assert!(rt.is_test, "fn inside mod tests is still test");
    }

    #[cfg(feature = "code")]
    #[test]
    fn parse_marks_whole_test_file_symbols_as_test() {
        let src = "fn it_works() {}\n";
        let p = parse("tests/lifecycle.rs", "rust", src).unwrap();
        assert!(
            p.defs.iter().all(|d| d.is_test),
            "every symbol in a tests/ file is test code"
        );
    }

    #[cfg(feature = "code")]
    #[test]
    fn path_is_test_recognizes_conventions() {
        assert!(path_is_test("tests/lifecycle.rs"));
        assert!(path_is_test("src/__tests__/x.ts"));
        assert!(path_is_test("foo_test.go"));
        assert!(path_is_test("test_thing.py"));
        assert!(path_is_test("widget.test.tsx"));
        assert!(!path_is_test("src/store.rs"));
        assert!(!path_is_test("src/contest.rs")); // 'test' substring, not a segment
    }

    #[test]
    fn is_stdlib_combinator_flags_common_methods() {
        for m in ["map", "filter", "unwrap", "collect", "get", "clone", "Some"] {
            assert!(is_stdlib_combinator(m), "{m} should be a combinator");
        }
        for m in ["reindex_notes", "upsert_note", "recall_with"] {
            assert!(!is_stdlib_combinator(m), "{m} is a real symbol");
        }
    }

    #[cfg(not(feature = "code"))]
    #[test]
    fn parse_without_feature_errors_with_hint() {
        let err = parse("x.rs", "rust", "fn x() {}").unwrap_err();
        assert!(err.msg.contains("not built into this binary"));
        assert!(err.hint.is_some());
    }
}
