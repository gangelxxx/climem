//! Our own little markdown note format: a `---`-fenced frontmatter block (flat
//! `key: value` pairs, deliberately NOT full YAML, so no serde_yaml) and then the
//! body. climem is the only thing that ever writes these, so a parser that only
//! understands what we emit is plenty, and a fixed key order keeps the files
//! byte-stable and nice to diff (same reasoning as output.rs A2).
//!
//! The md files under `notes/` are the SOURCE OF TRUTH; `store.db` is just a
//! derived index that `reindex` rebuilds from them (desc.md §3). So anything the
//! index or graph cares about has to be expressible here: `id`, `created`, `tags`,
//! `source`, `slug`, and the `relations` edge list (bodies can also carry
//! `[[wiki-links]]`).

use crate::util::{AppError, Result};

const EXAMPLE: &str =
    "---\nid: 0a1b2c3d\ncreated: 2026-06-17T10:00:00Z\ntags: auth, decision\n---\nbody text";

/// A note's frontmatter fields plus its body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Note {
    /// Short lowercase-hex id; equals the file stem (`notes/<id>.md`).
    pub id: String,
    /// ISO-8601 UTC creation time, authored once and preserved across reindex.
    pub created: String,
    /// Raw comma-separated tag string (`a, b`); split with `output::split_tags`.
    pub tags: String,
    /// `manual` or `import:<file>` provenance.
    pub source: Option<String>,
    /// Optional human handle other notes can link to (graph layer).
    pub slug: Option<String>,
    /// `(predicate, target)` relation edges authored in frontmatter (graph layer).
    pub relations: Vec<(String, String)>,
    /// The note text (frontmatter stripped, surrounding blank lines trimmed).
    pub body: String,
}

/// Render a note to its on-disk md form. The key order is fixed (so the bytes
/// are stable), empty optional fields are skipped, and there's always a trailing
/// newline.
pub fn render(note: &Note) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("id: {}\n", note.id));
    s.push_str(&format!("created: {}\n", note.created));
    if !note.tags.trim().is_empty() {
        s.push_str(&format!("tags: {}\n", note.tags.trim()));
    }
    if let Some(slug) = note.slug.as_deref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("slug: {slug}\n"));
    }
    if let Some(src) = note.source.as_deref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("source: {src}\n"));
    }
    if !note.relations.is_empty() {
        s.push_str("relations:\n");
        for (pred, target) in &note.relations {
            s.push_str(&format!("  - {pred}: {target}\n"));
        }
    }
    s.push_str("---\n");
    s.push_str(note.body.trim_matches(|c| c == '\n' || c == '\r'));
    s.push('\n');
    s
}

/// Parse a note's md form. There must be a leading `---` fence and a closing
/// `---` line; everything after the closing fence is the body, so a `---`
/// horizontal rule down in the body won't be mistaken for the end of the
/// frontmatter. Unknown keys are ignored (handy for forward compatibility), and
/// errors come with a copy-pasteable example.
pub fn parse(text: &str) -> Result<Note> {
    let after_open = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| AppError::with_hint("note is missing its `---` frontmatter", EXAMPLE))?;

    let mut note = Note::default();
    let mut body_start: Option<usize> = None;
    let mut offset = 0usize;
    let mut in_relations = false;

    for line in after_open.split_inclusive('\n') {
        let raw = line.trim_end_matches(['\n', '\r']);
        if raw == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        // Relations block: indented `- predicate: target` items.
        if in_relations {
            let item = raw.trim_start();
            if let Some(rest) = item.strip_prefix('-') {
                if let Some((pred, target)) = rest.split_once(':') {
                    let pred = pred.trim();
                    let target = target.trim();
                    if !pred.is_empty() && !target.is_empty() {
                        note.relations.push((pred.to_string(), target.to_string()));
                    }
                }
                offset += line.len();
                continue;
            }
            in_relations = false; // a line that isn't a `- ...` item ends the block; fall through and treat it as a key
        }
        if !raw.trim().is_empty() {
            if let Some((key, val)) = raw.split_once(':') {
                let val = val.trim();
                match key.trim() {
                    "id" => note.id = val.to_string(),
                    "created" => note.created = val.to_string(),
                    "tags" => note.tags = val.to_string(),
                    "source" => note.source = some_nonempty(val),
                    "slug" => note.slug = some_nonempty(val),
                    "relations" => in_relations = true, // items on following lines
                    _ => {}
                }
            }
        }
        offset += line.len();
    }

    let body_start = body_start
        .ok_or_else(|| AppError::with_hint("frontmatter is not closed with `---`", EXAMPLE))?;
    note.body = after_open[body_start..]
        .trim_matches(|c| c == '\n' || c == '\r')
        .to_string();
    Ok(note)
}

fn some_nonempty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Note {
        Note {
            id: "0a1b2c3d".into(),
            created: "2026-06-17T10:00:00Z".into(),
            tags: "auth, decision".into(),
            source: Some("manual".into()),
            slug: Some("jwt-auth".into()),
            relations: vec![
                ("depends_on".into(), "db-schema".into()),
                ("supersedes".into(), "id:9f8e7d".into()),
            ],
            body: "Решили: авторизацию делаем на JWT. См. [[db-schema]].".into(),
        }
    }

    #[test]
    fn render_parse_round_trip() {
        let n = sample();
        let text = render(&n);
        let back = parse(&text).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn render_has_fixed_shape() {
        let text = render(&sample());
        assert!(text.starts_with("---\nid: 0a1b2c3d\ncreated: 2026-06-17T10:00:00Z\n"));
        assert!(text.contains("\ntags: auth, decision\n"));
        assert!(text.contains("\nslug: jwt-auth\n"));
        assert!(text.contains("\nsource: manual\n"));
        assert!(text.contains("relations:\n  - depends_on: db-schema\n  - supersedes: id:9f8e7d\n"));
        assert!(text.ends_with("\n"));
    }

    #[test]
    fn render_omits_empty_optional_fields() {
        let n = Note {
            id: "ff00".into(),
            created: "2026-06-17T00:00:00Z".into(),
            body: "bare".into(),
            ..Default::default()
        };
        let text = render(&n);
        assert!(!text.contains("tags:"));
        assert!(!text.contains("slug:"));
        assert!(!text.contains("source:"));
        assert!(!text.contains("relations:"));
        assert_eq!(
            text,
            "---\nid: ff00\ncreated: 2026-06-17T00:00:00Z\n---\nbare\n"
        );
    }

    #[test]
    fn parse_minimal_note() {
        let n = parse("---\nid: abc123\ncreated: 2026-01-01T00:00:00Z\n---\nhello world").unwrap();
        assert_eq!(n.id, "abc123");
        assert_eq!(n.created, "2026-01-01T00:00:00Z");
        assert_eq!(n.body, "hello world");
        assert!(n.tags.is_empty());
        assert!(n.source.is_none());
        assert!(n.relations.is_empty());
    }

    #[test]
    fn parse_preserves_colons_in_values() {
        // created/timestamp and an `id:`-prefixed relation target both keep their
        // inner colons (split on the FIRST colon only).
        let n = parse(
            "---\nid: a1\ncreated: 2026-06-17T10:20:30Z\nrelations:\n  - blocks: id:deadbeef\n---\nx",
        )
        .unwrap();
        assert_eq!(n.created, "2026-06-17T10:20:30Z");
        assert_eq!(
            n.relations,
            vec![("blocks".to_string(), "id:deadbeef".to_string())]
        );
    }

    #[test]
    fn parse_body_with_horizontal_rule_not_terminated_early() {
        // A `---` *inside the body* (after the closing fence) is body content.
        let n = parse("---\nid: a1\ncreated: t\n---\nintro\n\n---\n\nmore").unwrap();
        assert_eq!(n.id, "a1");
        assert!(n.body.contains("intro"));
        assert!(n.body.contains("---"));
        assert!(n.body.contains("more"));
    }

    #[test]
    fn parse_missing_frontmatter_errors_with_hint() {
        let err = parse("no frontmatter here").unwrap_err();
        assert!(err.msg.contains("missing its `---` frontmatter"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn parse_unclosed_frontmatter_errors() {
        let err = parse("---\nid: a1\ncreated: t\nnever closed").unwrap_err();
        assert!(err.msg.contains("not closed"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let n = parse("---\nid: a1\ncreated: t\nfuture_key: whatever\n---\nbody").unwrap();
        assert_eq!(n.id, "a1");
        assert_eq!(n.body, "body");
    }

    #[test]
    fn parse_empty_body() {
        let n = parse("---\nid: a1\ncreated: t\n---\n").unwrap();
        assert_eq!(n.body, "");
    }
}
