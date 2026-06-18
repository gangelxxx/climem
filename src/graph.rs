//! Deriving the knowledge graph from a note's md (desc.md §3, §12). Edges come
//! from two places, and both can be rebuilt straight from the md, which is what
//! keeps the graph a derived, throwaway index: the frontmatter `relations`
//! (`predicate: target`) and the body's `[[wiki-links]]` (which we give the
//! synthetic predicate `links_to`).
//!
//! A `target` is a human-friendly name we resolve at index time. By default we
//! match it against a note's optional `slug`; an explicit `id:<hex>` prefix forces
//! resolution by note id instead. If a target doesn't resolve we don't drop it,
//! we keep it as a dangling edge (dst_id = None, dst_raw = the text as written),
//! which springs to life the moment its destination shows up on a later reindex.
//! Everything in this file is pure; the store wiring lives over in `commands`.

use crate::note::Note;
use std::collections::{BTreeMap, HashSet};

/// Normalize a slug or target so two spellings can match: trim it, lowercase it
/// (Unicode-aware), and collapse runs of space / `-` / `_` into a single `-`.
/// We keep Cyrillic and other non-ASCII as-is; the content is multilingual, so
/// unlike the FTS side we do NOT strip diacritics here.
pub fn normalize_slug(s: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() || ch == '-' || ch == '_' {
            pending_sep = !out.is_empty();
        } else {
            if pending_sep {
                out.push('-');
                pending_sep = false;
            }
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

/// Pull the `[[target]]` / `[[target|label]]` wiki-link targets out of a body, in
/// order. It's a plain text scan, not a real markdown parse, so a `[[link]]`
/// sitting inside a code span counts too. That's fine: edges are cheap, derived,
/// and perfectly happy to dangle.
pub fn scan_wikilinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let inner = &after[..end];
                let target = inner.split('|').next().unwrap_or("").trim();
                if !target.is_empty() {
                    out.push(target.to_string());
                }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    out
}

/// Every `(predicate, target, source)` edge a note contributes: its authored
/// `relations` (predicate lower-cased) plus the body's wiki-links (`links_to`).
pub fn note_edges(note: &Note) -> Vec<(String, String, &'static str)> {
    let mut edges = Vec::new();
    for (pred, target) in &note.relations {
        let p = pred.trim().to_lowercase();
        let t = target.trim();
        if !p.is_empty() && !t.is_empty() {
            edges.push((p, t.to_string(), "relation"));
        }
    }
    for target in scan_wikilinks(&note.body) {
        edges.push(("links_to".to_string(), target, "wikilink"));
    }
    edges
}

/// Turn a target name into a note id. An `id:<hex>` prefix forces resolution by
/// id (and only if that note actually exists); anything else is matched as a
/// normalized slug. `None` means it stays dangling. Note there's no "looks like
/// hex" guesswork, so a slug such as `db-schema` can never accidentally land on
/// a note id.
pub fn resolve_target(
    literal: &str,
    ids: &HashSet<String>,
    slug_map: &BTreeMap<String, String>,
) -> Option<String> {
    let t = literal.trim();
    if let Some(rest) = t.strip_prefix("id:") {
        let id = rest.trim();
        return ids.contains(id).then(|| id.to_string());
    }
    slug_map.get(&normalize_slug(t)).cloned()
}

/// Build a `normalized_slug -> note_id` map from `(id, slug)` pairs. The pairs
/// MUST arrive sorted by id ascending: that way, when two notes claim the same
/// slug, the lowest id wins, which keeps things deterministic and matches the
/// recall tie-break.
pub fn build_slug_map(pairs: &[(String, String)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (id, slug) in pairs {
        let key = normalize_slug(slug);
        if !key.is_empty() {
            map.entry(key).or_insert_with(|| id.clone());
        }
    }
    map
}

/// Find slugs that more than one note claims: `(normalized_slug, [ids…])`, ids in
/// ascending order (the first one wins, per `build_slug_map`). reindex uses this
/// to print a non-fatal warning so the author can go untangle the collision.
pub fn slug_collisions(pairs: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, slug) in pairs {
        let key = normalize_slug(slug);
        if !key.is_empty() {
            groups.entry(key).or_default().push(id.clone());
        }
    }
    groups
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_slug_folds_separators_and_case_keeps_cyrillic() {
        assert_eq!(normalize_slug("DB Schema"), "db-schema");
        assert_eq!(normalize_slug("db_schema"), "db-schema");
        assert_eq!(normalize_slug("  DB-Schema  "), "db-schema");
        assert_eq!(normalize_slug("a  b\tc"), "a-b-c");
        assert_eq!(normalize_slug("Авторизация"), "авторизация");
        assert_eq!(normalize_slug("--leading--trailing--"), "leading-trailing");
    }

    #[test]
    fn scan_wikilinks_targets_and_labels() {
        assert_eq!(scan_wikilinks("see [[db-schema]] now"), vec!["db-schema"]);
        assert_eq!(
            scan_wikilinks("[[a]] and [[b|the B]] and [[ c ]]"),
            vec!["a", "b", "c"]
        );
        assert!(scan_wikilinks("no links here").is_empty());
        assert!(scan_wikilinks("unclosed [[oops").is_empty());
        assert!(scan_wikilinks("[[]]").is_empty()); // empty target ignored
    }

    #[test]
    fn note_edges_from_relations_and_body() {
        let note = Note {
            relations: vec![
                ("Depends_On".into(), "db-schema".into()),
                ("blocks".into(), "id:9f8e7d".into()),
            ],
            body: "text with [[api-rate-limit]] link".into(),
            ..Default::default()
        };
        let edges = note_edges(&note);
        assert_eq!(
            edges[0],
            ("depends_on".into(), "db-schema".into(), "relation")
        );
        assert_eq!(edges[1], ("blocks".into(), "id:9f8e7d".into(), "relation"));
        assert_eq!(
            edges[2],
            ("links_to".into(), "api-rate-limit".into(), "wikilink")
        );
    }

    #[test]
    fn resolve_target_slug_then_dangling() {
        let mut slugs = BTreeMap::new();
        slugs.insert("db-schema".to_string(), "aa11".to_string());
        let ids = ids(&["aa11", "bb22"]);
        assert_eq!(
            resolve_target("DB Schema", &ids, &slugs),
            Some("aa11".into())
        );
        assert_eq!(resolve_target("unknown-thing", &ids, &slugs), None); // dangling
    }

    #[test]
    fn resolve_target_explicit_id_prefix() {
        let slugs = BTreeMap::new();
        let ids = ids(&["aa11", "bb22"]);
        // id: forces id resolution (only when the id exists).
        assert_eq!(resolve_target("id:bb22", &ids, &slugs), Some("bb22".into()));
        assert_eq!(resolve_target("id:nope", &ids, &slugs), None); // dangling
                                                                   // A slug that happens to look like hex is still just a slug, not an id.
        let mut s2 = BTreeMap::new();
        s2.insert("aa11".to_string(), "cc33".to_string()); // a slug literally "aa11"
        assert_eq!(resolve_target("aa11", &ids, &s2), Some("cc33".into()));
    }

    #[test]
    fn build_slug_map_lowest_id_wins_on_collision() {
        // Pairs ordered by id ascending; the duplicate "shared" slug keeps a1.
        let pairs = vec![
            ("a1".to_string(), "Shared".to_string()),
            ("a2".to_string(), "shared".to_string()),
            ("a3".to_string(), "unique".to_string()),
        ];
        let map = build_slug_map(&pairs);
        assert_eq!(map.get("shared"), Some(&"a1".to_string()));
        assert_eq!(map.get("unique"), Some(&"a3".to_string()));
    }

    #[test]
    fn slug_collisions_reports_only_shared_slugs() {
        let pairs = vec![
            ("a1".to_string(), "Shared".to_string()),
            ("a2".to_string(), "shared".to_string()),
            ("a3".to_string(), "unique".to_string()),
        ];
        let cols = slug_collisions(&pairs);
        assert_eq!(
            cols,
            vec![(
                "shared".to_string(),
                vec!["a1".to_string(), "a2".to_string()]
            )]
        );
    }
}
