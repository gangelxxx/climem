//! Where we shape the JSON that commands print (JSONL on stdout, desc.md §4).
//!
//! Object keys come out in alphabetical order, because serde_json backs objects
//! with a `BTreeMap` by default and we don't turn on `preserve_order`. That makes
//! the output byte-for-byte identical run to run, which keeps it a stable,
//! cacheable prefix (plan A2).

use crate::store::NoteRow;
use crate::util::preview;
use serde_json::{json, Value};

/// The fields a caller can ask for from `recall` via `--fields`. The lean default
/// and `--explain` pull from this same set too. Plan E1.
pub const RECALL_FIELDS: &[&str] = &[
    "id",
    "kind",
    "body",
    "tags",
    "origin",
    "source",
    "score",
    "fts",
    "vector",
    "graph",
    "created_at",
    "preview",
    // Full character length of the body — the marker that travels with a
    // budget-truncated `preview` so a caller knows there's more to `cm get`.
    "chars",
];

/// Round a similarity scalar to 4 decimals, so the JSON doesn't wobble on f32 noise.
pub fn round4(x: f32) -> f64 {
    ((x * 10_000.0).round() / 10_000.0) as f64
}

/// Full record for `get` / `recall` / `export`.
pub fn note_value(row: &NoteRow) -> Value {
    json!({
        "id": row.id,
        "kind": row.kind,
        "body": row.body,
        "tags": split_tags(&row.tags),
        "source": row.source,
        "origin": row.origin,
        "created_at": row.created_iso,
    })
}

/// Shape one `recall` hit as a lean JSON object (plan E1).
///
/// - With `fields = Some(list)` you get exactly those fields and nothing else
///   (the explicit projection bypasses `body_budget` — `body` means the whole body).
/// - With `fields = None` you get the default set: `id, kind`, then either `body`
///   (whole) or, when `body_budget = Some(n)` and the body is longer than `n`
///   chars, a `preview` (truncated to `n`) plus `chars` (the full length) instead —
///   the preview-first default that keeps the main read path lean. Add
///   `tags`/`origin`/`source` only when present, plus `score/fts/vector/graph`
///   only when `explain` is on.
///
/// The point of dropping the debug scalars, the empty/null fields, and the long
/// tail of big bodies by default is to keep each recall row small.
// The score channels (score/fts/vector/graph) are distinct f32s a caller already
// holds as separate locals; bundling them into a struct just to dodge the arg
// count would add ceremony at every call site for no clarity. Plan E1.
#[allow(clippy::too_many_arguments)]
pub fn recall_value(
    row: &NoteRow,
    score: f32,
    fts: f32,
    vector: f32,
    graph: f32,
    fields: Option<&[String]>,
    explain: bool,
    body_budget: Option<usize>,
) -> Value {
    let mut map = serde_json::Map::new();
    match fields {
        Some(fs) => {
            for f in fs {
                if let Some(v) = field_value(f, row, score, fts, vector, graph) {
                    map.insert(f.clone(), v);
                }
            }
        }
        None => {
            map.insert("id".into(), json!(row.id));
            map.insert("kind".into(), json!(row.kind));
            insert_body(&mut map, &row.body, body_budget);
            let tags = split_tags(&row.tags);
            if !tags.is_empty() {
                map.insert("tags".into(), json!(tags));
            }
            if let Some(o) = &row.origin {
                map.insert("origin".into(), json!(o));
            }
            if let Some(s) = &row.source {
                map.insert("source".into(), json!(s));
            }
            if explain {
                map.insert("score".into(), json!(round4(score)));
                map.insert("fts".into(), json!(round4(fts)));
                map.insert("vector".into(), json!(round4(vector)));
                map.insert("graph".into(), json!(round4(graph)));
            }
        }
    }
    Value::Object(map)
}

/// Insert the body into a default-shape recall row under a per-result budget.
///
/// Within budget (or no budget): the whole body lands under `body`, so short
/// remembered facts are byte-for-byte what they were before this knob existed.
/// Over budget: `body` is dropped in favor of `preview` (truncated to the budget,
/// ellipsis-terminated) and `chars` (the full length) — a self-describing signal
/// that there's more, fetchable with `cm get <id>`. The key change (`body` →
/// `preview`) is deliberate: a caller that reads `body` notices its absence rather
/// than silently acting on a truncated value.
fn insert_body(map: &mut serde_json::Map<String, Value>, body: &str, budget: Option<usize>) {
    match budget {
        Some(max) if body.chars().count() > max => {
            map.insert("preview".into(), json!(preview(body, max)));
            map.insert("chars".into(), json!(body.chars().count()));
        }
        _ => {
            map.insert("body".into(), json!(body));
        }
    }
}

/// Look up one recall field by name (`None` if the name isn't one we know).
fn field_value(
    name: &str,
    row: &NoteRow,
    score: f32,
    fts: f32,
    vector: f32,
    graph: f32,
) -> Option<Value> {
    Some(match name {
        "id" => json!(row.id),
        "kind" => json!(row.kind),
        "body" => json!(row.body),
        "tags" => json!(split_tags(&row.tags)),
        "origin" => json!(row.origin),
        "source" => json!(row.source),
        "score" => json!(round4(score)),
        "fts" => json!(round4(fts)),
        "vector" => json!(round4(vector)),
        "graph" => json!(round4(graph)),
        "created_at" => json!(row.created_iso),
        "preview" => json!(preview(&row.body, 160)),
        "chars" => json!(row.body.chars().count()),
        _ => return None,
    })
}

/// The fields a caller can ask for from `related` via `--fields`: the note fields
/// plus the graph-specific ones (`predicate`, `distance`, `dangling`, `name`).
pub const RELATED_FIELDS: &[&str] = &[
    "id",
    "kind",
    "body",
    "tags",
    "origin",
    "source",
    "predicate",
    "distance",
    "dangling",
    "name",
    "preview",
    "created_at",
];

/// Shape one `related` neighbor as a lean JSON object. A resolved neighbor carries
/// the note's `id/kind/body` (plus tags/origin/source when present); a dangling
/// target (one nobody's written yet) carries `name` (the raw target) and no `id`.
/// Either way every row carries `dangling`, `distance`, and `predicate`, so a
/// caller can branch on them without guessing.
pub fn related_value(
    row: Option<&NoteRow>,
    name: &str,
    predicate: &str,
    distance: usize,
    fields: Option<&[String]>,
) -> Value {
    let dangling = row.is_none();
    match fields {
        Some(fs) => {
            let mut map = serde_json::Map::new();
            for f in fs {
                map.insert(
                    f.clone(),
                    related_field(f, row, name, predicate, distance, dangling),
                );
            }
            Value::Object(map)
        }
        None => {
            let mut map = serde_json::Map::new();
            map.insert("dangling".into(), json!(dangling));
            map.insert("distance".into(), json!(distance));
            map.insert("predicate".into(), json!(predicate));
            match row {
                Some(r) => {
                    map.insert("id".into(), json!(r.id));
                    map.insert("kind".into(), json!(r.kind));
                    map.insert("body".into(), json!(r.body));
                    let tags = split_tags(&r.tags);
                    if !tags.is_empty() {
                        map.insert("tags".into(), json!(tags));
                    }
                    if let Some(o) = &r.origin {
                        map.insert("origin".into(), json!(o));
                    }
                    if let Some(s) = &r.source {
                        map.insert("source".into(), json!(s));
                    }
                }
                None => {
                    map.insert("name".into(), json!(name));
                }
            }
            Value::Object(map)
        }
    }
}

/// Look up one `related` field by name. Fields that don't apply to this row come
/// back as null, so the caller still gets exactly the key set it asked for.
fn related_field(
    field: &str,
    row: Option<&NoteRow>,
    name: &str,
    predicate: &str,
    distance: usize,
    dangling: bool,
) -> Value {
    match field {
        "id" => json!(row.map(|r| &r.id)),
        "kind" => json!(row.map(|r| &r.kind)),
        "body" => json!(row.map(|r| &r.body)),
        "tags" => json!(row.map(|r| split_tags(&r.tags)).unwrap_or_default()),
        "origin" => json!(row.and_then(|r| r.origin.as_ref())),
        "source" => json!(row.and_then(|r| r.source.as_ref())),
        "predicate" => json!(predicate),
        "distance" => json!(distance),
        "dangling" => json!(dangling),
        "name" => json!(name),
        "preview" => json!(row.map(|r| preview(&r.body, 160))),
        "created_at" => json!(row.map(|r| &r.created_iso)),
        _ => Value::Null,
    }
}

/// Compact record for `list`, with the body shown only as a preview.
pub fn note_preview_value(row: &NoteRow) -> Value {
    json!({
        "id": row.id,
        "kind": row.kind,
        "preview": preview(&row.body, 160),
        "tags": split_tags(&row.tags),
        "source": row.source,
        "origin": row.origin,
        "created_at": row.created_iso,
    })
}

pub fn split_tags(tags: &str) -> Vec<String> {
    tags.split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Print one compact JSON object on its own line.
///
/// Uses `writeln!` rather than `println!` so a closed reader (the normal
/// `cm recall … | head -1` case) exits cleanly instead of panicking on the
/// `BrokenPipe` write error with a backtrace.
pub fn print_line(v: &Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{v}").is_err() {
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(body: &str, tags: &str, source: Option<&str>, origin: Option<&str>) -> NoteRow {
        NoteRow {
            id: "a1b2c3".into(),
            body: body.into(),
            tags: tags.into(),
            source: source.map(Into::into),
            origin: origin.map(Into::into),
            kind: "note".into(),
            created_at: 0,
            created_iso: "1970-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn round4_basic_and_boundaries() {
        assert_eq!(round4(0.0), 0.0);
        assert!((round4(0.123456) - 0.1235).abs() < 1e-6);
        assert_eq!(round4(0.00004), 0.0);
        assert!((round4(0.00006) - 0.0001).abs() < 1e-6);
        assert!((round4(-0.123456) + 0.1235).abs() < 1e-6);
    }

    #[test]
    fn recall_value_default_is_lean_and_omits_empty_nulls() {
        // tags empty, origin/source null -> only id/kind/body remain (no debug scalars).
        let v = recall_value(
            &row("hi", "", None, None),
            0.5,
            0.3,
            0.2,
            0.0,
            None,
            false,
            None,
        );
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.keys().cloned().collect::<Vec<_>>(),
            vec!["body", "id", "kind"]
        );
        assert_eq!(v["body"], json!("hi"));
        for absent in [
            "tags",
            "origin",
            "source",
            "score",
            "fts",
            "vector",
            "graph",
            "created_at",
        ] {
            assert!(
                obj.get(absent).is_none(),
                "default recall row must omit `{absent}`"
            );
        }
    }

    #[test]
    fn recall_value_keeps_present_metadata() {
        let v = recall_value(
            &row("hi", "a,b", Some("src"), Some("Doc › H")),
            0.5,
            0.3,
            0.2,
            0.0,
            None,
            false,
            None,
        );
        assert_eq!(v["tags"], json!(["a", "b"]));
        assert_eq!(v["source"], json!("src"));
        assert_eq!(v["origin"], json!("Doc › H"));
    }

    #[test]
    fn recall_value_explain_adds_rounded_scalars() {
        // round4 returns a widened f32, so compare with a tolerance (the f32->f64
        // artifact, ~6e-4) rather than exact JSON equality.
        let v = recall_value(
            &row("hi", "", None, None),
            0.12346,
            0.1,
            0.02346,
            0.0123,
            None,
            true,
            None,
        );
        assert!((v["score"].as_f64().unwrap() - 0.1235).abs() < 1e-4); // round4(0.12346)
        assert!((v["fts"].as_f64().unwrap() - 0.1).abs() < 1e-4);
        assert!((v["vector"].as_f64().unwrap() - 0.0235).abs() < 1e-4); // round4(0.02346)
        assert!((v["graph"].as_f64().unwrap() - 0.0123).abs() < 1e-4); // graph channel under --explain
    }

    #[test]
    fn recall_value_fields_projection_exact_set() {
        let fields = vec!["id".to_string(), "body".to_string()];
        let v = recall_value(
            &row("hi", "x", Some("s"), None),
            0.5,
            0.3,
            0.2,
            0.0,
            Some(&fields),
            false,
            None,
        );
        let obj = v.as_object().unwrap();
        // Exactly the requested keys (alphabetical in output), nothing else.
        assert_eq!(obj.keys().cloned().collect::<Vec<_>>(), vec!["body", "id"]);
    }

    #[test]
    fn recall_value_fields_preview_and_scalars() {
        let fields = vec!["preview".to_string(), "score".to_string()];
        let v = recall_value(
            &row("a b c", "", None, None),
            0.4242,
            0.2,
            0.2242,
            0.0,
            Some(&fields),
            false,
            None,
        );
        assert_eq!(v["preview"], json!("a b c"));
        assert!((v["score"].as_f64().unwrap() - 0.4242).abs() < 1e-4);
    }

    #[test]
    fn recall_value_budget_passes_short_body_whole() {
        // A body within budget is printed whole under `body` — no preview, no chars.
        let v = recall_value(
            &row("short fact", "", None, None),
            0.5,
            0.3,
            0.2,
            0.0,
            None,
            false,
            Some(100),
        );
        assert_eq!(v["body"], json!("short fact"));
        assert!(v.get("preview").is_none());
        assert!(v.get("chars").is_none());
    }

    #[test]
    fn recall_value_budget_truncates_long_body_to_preview() {
        // Over budget: `body` is dropped for `preview` (budget+ellipsis) plus the
        // full `chars` length, so the caller knows to `cm get` for the rest.
        let body: String = "x".repeat(600);
        let v = recall_value(
            &row(&body, "", None, None),
            0.5,
            0.3,
            0.2,
            0.0,
            None,
            false,
            Some(100),
        );
        assert!(
            v.get("body").is_none(),
            "over-budget body must not be printed whole"
        );
        let preview = v["preview"].as_str().unwrap();
        assert_eq!(preview.chars().count(), 101); // 100 + ellipsis
        assert!(preview.ends_with('…'));
        assert_eq!(v["chars"], json!(600));
    }

    #[test]
    fn recall_value_no_budget_keeps_whole_body() {
        // `--full` / `--budget 0` -> body_budget None -> whole body regardless of size.
        let body: String = "y".repeat(600);
        let v = recall_value(
            &row(&body, "", None, None),
            0.5,
            0.3,
            0.2,
            0.0,
            None,
            false,
            None,
        );
        assert_eq!(v["body"].as_str().unwrap().chars().count(), 600);
        assert!(v.get("preview").is_none());
    }

    #[test]
    fn split_tags_variants() {
        assert_eq!(split_tags(""), Vec::<String>::new());
        assert_eq!(split_tags("a,,b"), vec!["a", "b"]);
        assert_eq!(split_tags(" a , b "), vec!["a", "b"]);
        assert_eq!(split_tags(",,,"), Vec::<String>::new());
        assert_eq!(split_tags("a b, c"), vec!["a b", "c"]);
    }

    #[test]
    fn note_value_shape_and_null_options() {
        let v = note_value(&row("a long body that stays whole", "x,y", None, None));
        assert_eq!(v["id"], json!("a1b2c3"));
        assert_eq!(v["kind"], json!("note"));
        assert_eq!(v["body"], json!("a long body that stays whole"));
        assert_eq!(v["tags"], json!(["x", "y"]));
        assert_eq!(v["source"], Value::Null);
        assert_eq!(v["origin"], Value::Null);
        // created_at carries the iso string.
        assert_eq!(v["created_at"], json!("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn note_preview_value_truncates_at_160() {
        let body = "w ".repeat(200); // > 160 chars after collapse
        let v = note_preview_value(&row(&body, "", None, None));
        let preview = v["preview"].as_str().unwrap();
        assert_eq!(preview.chars().count(), 161); // 160 + ellipsis
        assert!(preview.ends_with('…'));
        assert!(v.get("body").is_none());
    }

    #[test]
    fn preview_collapses_whitespace_and_unicode_safe() {
        assert_eq!(preview("a\n  b\tc", 160), "a b c");
        let long: String = "ж".repeat(300);
        let p = preview(&long, 160);
        assert_eq!(p.chars().count(), 161);
    }

    #[test]
    fn preview_whitespace_only_yields_empty() {
        assert_eq!(preview("   \n\t ", 160), "");
    }
}
