//! Export the store, all of it or a filtered subset, into something portable and
//! readable (desc.md §6). Markdown is for reading and committing to a repo;
//! JSON/JSONL is for backups and migrations.

use crate::output::{note_value, split_tags};
use crate::store::NoteRow;
use crate::util::{AppError, Result};
use serde_json::Value;

/// Render one of the text formats (md / json / jsonl) into a string.
pub fn render(format: &str, rows: &[NoteRow]) -> Result<String> {
    match format {
        "md" | "markdown" => Ok(to_markdown(rows)),
        "json" => {
            let arr: Vec<Value> = rows.iter().map(note_value).collect();
            Ok(serde_json::to_string_pretty(&Value::Array(arr))?)
        }
        "jsonl" => {
            let mut s = String::new();
            for r in rows {
                s.push_str(&note_value(r).to_string());
                s.push('\n');
            }
            Ok(s)
        }
        "pdf" => Err(AppError::with_hint(
            "PDF export is not built into this binary",
            "Use `cm export md --out dump.md` (then convert), or rebuild with `--features pdf`.",
        )),
        other => Err(AppError::with_hint(
            format!("unknown export format '{other}'"),
            "Formats: md | json | jsonl  (pdf needs the `pdf` feature).",
        )),
    }
}

fn to_markdown(rows: &[NoteRow]) -> String {
    let mut s = String::new();
    s.push_str("# Memory export\n\n");
    s.push_str(&format!("_{} record(s)._\n\n", rows.len()));
    for r in rows {
        let title = r
            .origin
            .clone()
            .unwrap_or_else(|| format!("note #{}", r.id));
        s.push_str(&format!("## {title}\n\n"));
        let tags = split_tags(&r.tags);
        let mut meta = vec![
            format!("id: {}", r.id),
            format!("kind: {}", r.kind),
            format!("date: {}", r.created_iso),
        ];
        if !tags.is_empty() {
            meta.push(format!("tags: {}", tags.join(", ")));
        }
        if let Some(src) = &r.source {
            meta.push(format!("source: {src}"));
        }
        s.push_str(&format!("<sub>{}</sub>\n\n", meta.join(" · ")));
        s.push_str(r.body.trim());
        s.push_str("\n\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(
        id: &str,
        body: &str,
        tags: &str,
        source: Option<&str>,
        origin: Option<&str>,
    ) -> NoteRow {
        NoteRow {
            id: id.into(),
            body: body.into(),
            tags: tags.into(),
            source: source.map(Into::into),
            origin: origin.map(Into::into),
            kind: "note".into(),
            created_at: 0,
            created_iso: "1970-01-01T00:00:00Z".into(),
        }
    }

    fn err_of(format: &str, rows: &[NoteRow]) -> AppError {
        render(format, rows).unwrap_err()
    }

    #[test]
    fn render_json_shape_roundtrips_keys() {
        let out = render(
            "json",
            &[row("1", "body", "a,b", Some("src"), Some("Title"))],
        )
        .unwrap();
        assert!(out.contains('\n')); // pretty-printed
        let arr: Value = serde_json::from_str(&out).unwrap();
        let el = &arr.as_array().unwrap()[0];
        for k in [
            "id",
            "kind",
            "body",
            "tags",
            "source",
            "origin",
            "created_at",
        ] {
            assert!(el.get(k).is_some(), "missing key {k}");
        }
        assert!(el["tags"].is_array());
        assert_eq!(el["created_at"], json!("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn render_jsonl_one_compact_object_per_line() {
        let out = render(
            "jsonl",
            &[row("1", "a", "", None, None), row("2", "b", "", None, None)],
        )
        .unwrap();
        assert!(out.ends_with('\n'));
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        for l in lines {
            assert!(!l.contains('\n'));
            serde_json::from_str::<Value>(l).expect("each line is valid JSON");
        }
    }

    #[test]
    fn render_unknown_format_errors_with_hint() {
        let e = err_of("xml", &[]);
        assert!(e.msg.contains("unknown export format 'xml'"));
        assert!(e.hint.is_some());
    }

    #[test]
    fn render_pdf_always_errors_regardless_of_feature() {
        // The "pdf" arm is NOT feature-gated: always an error with a hint.
        let e = err_of("pdf", &[]);
        assert!(e.msg.contains("PDF export is not built"));
        assert!(e.hint.is_some());
    }

    #[test]
    fn render_md_empty_rows() {
        let out = render("md", &[]).unwrap();
        assert!(out.starts_with("# Memory export"));
        assert!(out.contains("_0 record(s)._"));
        assert!(!out.contains("## "));
    }

    #[test]
    fn render_md_uses_origin_then_falls_back_to_note_id() {
        let out = render("md", &[row("1", "x", "", None, Some("My Title"))]).unwrap();
        assert!(out.contains("## My Title"));
        let out2 = render("md", &[row("42", "x", "", None, None)]).unwrap();
        assert!(out2.contains("## note #42"));
    }

    #[test]
    fn render_md_meta_line_omits_empty_tags_and_none_source() {
        let bare = render("md", &[row("1", "x", "", None, None)]).unwrap();
        assert!(!bare.contains("tags:"));
        assert!(!bare.contains("source:"));
        let full = render("md", &[row("1", "x", "a,b", Some("S"), None)]).unwrap();
        assert!(full.contains("tags: a, b"));
        assert!(full.contains("source: S"));
    }

    #[test]
    fn render_json_empty_rows_is_empty_array() {
        let json = render("json", &[]).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&json)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(render("jsonl", &[]).unwrap(), "");
    }

    #[test]
    fn render_md_trims_body() {
        let out = render("md", &[row("1", "   hi there   ", "", None, None)]).unwrap();
        assert!(out.contains("hi there"));
        assert!(!out.contains("   hi there"));
    }

    #[test]
    fn render_md_aliases_md_and_markdown() {
        let rows = [row("1", "x", "", None, None)];
        assert_eq!(
            render("md", &rows).unwrap(),
            render("markdown", &rows).unwrap()
        );
        assert!(render("MD", &rows).is_err()); // case-sensitive
    }

    #[test]
    fn render_md_origin_not_markdown_escaped() {
        // Regression anchor: origin is interpolated raw, not escaped.
        let out = render("md", &[row("1", "x", "", None, Some("# Inject ## x"))]).unwrap();
        assert!(out.contains("## # Inject ## x"));
    }
}
