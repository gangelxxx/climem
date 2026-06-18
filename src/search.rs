//! Hybrid recall: run an FTS5 keyword ranking and a brute-force vector cosine,
//! then fuse them with Reciprocal Rank Fusion (RRF) and hand back the top-k
//! (desc.md §3, §11). All of that hides behind `recall`, so the contract doesn't
//! change.
//!
//! We moved to RRF (token-efficiency-plan R1) from per-channel min-max
//! normalization. RRF fuses the *positions* in each ranking rather than the raw
//! scores, which is the whole point: bm25 (unbounded) and cosine ([-1,1]) become
//! directly comparable without any scale-sensitive tuning, and the resulting
//! top-k is stable enough to chop down to a small `limit`. One thing to know: the
//! `score`/`fts`/`vector` we report are the fused contributions (`score == fts +
//! vector`), not [0,1] similarities, so they're naturally small (~`w/(k+rank)`).

use crate::config::Config;
use crate::embed::{cosine, Embedder};
use crate::store::{NoteRow, Store};
use crate::util::Result;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

pub struct Hit {
    pub row: NoteRow,
    /// The fused RRF score (`fts + vector + graph`); bigger is better.
    pub score: f32,
    /// The keyword channel's share (`w_fts / (k + rank_fts)`, 0 if it didn't match).
    pub fts: f32,
    /// The vector channel's share (`w_vec / (k + rank_vec)`, 0 if there's no vector).
    pub vector: f32,
    /// The graph-proximity share from `--related` (0 unless you opted in).
    pub graph: f32,
}

/// Knobs and optional pre-filters for a single recall.
#[derive(Default)]
pub struct RecallOpts<'a> {
    pub limit: usize,
    /// Keep only notes that carry this tag (case-insensitive). Plan R4.
    pub tag: Option<&'a str>,
    /// Keep only notes whose `origin` starts with this prefix. Plan R4.
    pub origin_prefix: Option<&'a str>,
    /// Drop anything whose fused score is below this floor (0 keeps everything). Plan K1.
    pub min_score: f32,
    /// Give a boost to notes that sit near this one in the graph (the RRF graph
    /// channel, weighted by `config.search.hybrid_weights.graph`; off by default).
    pub related: Option<&'a str>,
}

/// The easy entry point: recall with just a `limit` and no filters (used by
/// `export` and the tests).
pub fn recall(
    store: &Store,
    embedder: &dyn Embedder,
    cfg: &Config,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    recall_with(
        store,
        embedder,
        cfg,
        query,
        &RecallOpts {
            limit,
            ..Default::default()
        },
    )
}

pub fn recall_with(
    store: &Store,
    embedder: &dyn Embedder,
    cfg: &Config,
    query: &str,
    opts: &RecallOpts,
) -> Result<Vec<Hit>> {
    let limit = opts.limit;

    // --- keyword channel (FTS5) ---
    // Pull a wide pool that doesn't depend on `limit`, so fusion gets enough of
    // the tail to work with (plan R2). The vector channel already scans every
    // row, so this only really matters for FTS.
    let pool = cfg.search.candidates.max(limit).max(1);
    let match_expr = fts_match_expr(query);
    let fts_hits: Vec<(String, f64)> = if match_expr.is_empty() {
        Vec::new()
    } else {
        store.fts_search(&match_expr, pool)?
    };
    // fts_hits comes back sorted by bm25 ascending, so a row's position is its rank (1-based).
    let mut fts_rank: HashMap<String, usize> = HashMap::with_capacity(fts_hits.len());
    for (i, (id, _)) in fts_hits.iter().enumerate() {
        fts_rank.insert(id.clone(), i + 1);
    }

    // --- semantic channel (vector) ---
    let qvec = embedder.embed(query)?;
    let embeds = store.all_embeddings()?;
    let mut cos: Vec<(String, f32)> = embeds
        .iter()
        .map(|(id, v)| (id.clone(), cosine(&qvec, v)))
        .collect();
    // Rank by cosine descending; tie-break by id so ranks are deterministic.
    cos.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let mut vec_rank: HashMap<String, usize> = HashMap::with_capacity(cos.len());
    for (i, (id, _)) in cos.iter().enumerate() {
        vec_rank.insert(id.clone(), i + 1);
    }

    // The candidate set is every embedded id plus every FTS-matched id. We use a
    // BTreeSet so iteration order is stable; a HashMap's random seed would make
    // results wobble between runs.
    let mut ids: BTreeSet<String> = cos.iter().map(|(id, _)| id.clone()).collect();
    ids.extend(fts_hits.iter().map(|(id, _)| id.clone()));
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // Optional metadata pre-filter (plan R4): narrow candidates before scoring.
    let allowed = if opts.tag.is_some() || opts.origin_prefix.is_some() {
        Some(store.ids_matching(opts.tag, opts.origin_prefix)?)
    } else {
        None
    };

    let wf = cfg.search.hybrid_weights.fts;
    let wv = cfg.search.hybrid_weights.vector;
    let wg = cfg.search.hybrid_weights.graph;
    let k = cfg.search.rrf_k as f32;

    // --- graph channel (optional) ---
    // Rank candidates by their graph distance from `--related <id>`. This stays
    // empty unless you both pass a target AND give the graph channel a non-zero
    // weight, so an ordinary recall comes out byte-for-byte the same as before.
    // Anything that isn't a neighbor just contributes 0, same as any other miss.
    let graph_rank: HashMap<String, usize> = match opts.related {
        Some(start) if wg != 0.0 => neighbor_ranks(store, start, 2)?,
        _ => HashMap::new(),
    };

    let mut scored: Vec<(String, f32, f32, f32, f32)> = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(set) = &allowed {
            if !set.contains(id) {
                continue;
            }
        }
        let fts = fts_rank.get(id).map_or(0.0, |r| wf / (k + *r as f32));
        let vector = vec_rank.get(id).map_or(0.0, |r| wv / (k + *r as f32));
        let graph = graph_rank.get(id).map_or(0.0, |r| wg / (k + *r as f32));
        scored.push((id.clone(), fts + vector + graph, fts, vector, graph));
    }

    // Sort by fused score descending, tie-break by id ascending, which makes the
    // order fully deterministic.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    if opts.min_score > 0.0 {
        scored.retain(|s| s.1 >= opts.min_score);
    }
    scored.truncate(limit);

    let mut out = Vec::new();
    for (id, score, fts, vector, graph) in scored {
        if let Some(row) = store.get(&id)? {
            out.push(Hit {
                row,
                score,
                fts,
                vector,
                graph,
            });
        }
    }
    Ok(out)
}

/// Rank notes by how far they sit from `start` in the graph: a BFS over resolved
/// outgoing edges, up to `depth` hops, ordered by (distance, id). Returns a
/// `id -> 1-based rank` map and leaves `start` itself out. It's deterministic
/// (we sort the frontier each round, then sort the result).
fn neighbor_ranks(store: &Store, start: &str, depth: usize) -> Result<HashMap<String, usize>> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(start.to_string());
    let mut by_distance: Vec<(usize, String)> = Vec::new();
    let mut frontier: Vec<String> = vec![start.to_string()];
    let mut dist = 1;
    while dist <= depth && !frontier.is_empty() {
        frontier.sort();
        let mut next: Vec<String> = Vec::new();
        for src in &frontier {
            for e in store.edges_from(src)? {
                if let Some(did) = e.dst_id {
                    if visited.insert(did.clone()) {
                        next.push(did.clone());
                        by_distance.push((dist, did));
                    }
                }
            }
        }
        frontier = next;
        dist += 1;
    }
    by_distance.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(by_distance
        .into_iter()
        .enumerate()
        .map(|(i, (_, id))| (id, i + 1))
        .collect())
}

/// Build an FTS5 MATCH expression: quote each alphanumeric token and OR them together.
fn fts_match_expr(query: &str) -> String {
    let lower = query.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in lower.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{cosine, Embedder};
    use crate::store::Store;
    use rusqlite::params;
    use std::path::Path;

    // ---- pure functions -------------------------------------------------

    #[test]
    fn fts_match_expr_basic_or_join() {
        assert_eq!(
            fts_match_expr("Auth Token 42"),
            "\"auth\" OR \"token\" OR \"42\""
        );
    }

    #[test]
    fn fts_match_expr_empty_and_punctuation() {
        assert_eq!(fts_match_expr(""), "");
        assert_eq!(fts_match_expr("   "), "");
        assert_eq!(fts_match_expr("!!! ???"), "");
    }

    #[test]
    fn fts_match_expr_unicode_cyrillic() {
        assert_eq!(
            fts_match_expr("Авторизация, токен"),
            "\"авторизация\" OR \"токен\""
        );
    }

    #[test]
    fn cosine_properties_smoke() {
        // This channel leans on embed::cosine, so re-check the cases that matter.
        assert!((cosine(&[1.0, 1.0], &[1.0, 1.0]) - 1.0).abs() <= 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }

    // ---- recall integration (in-memory store + fake embedder) -----------

    /// Embedder whose query vector is looked up by text (full control over the
    /// semantic channel); unknown text -> zero vector.
    struct FakeEmbedder {
        map: std::collections::HashMap<String, Vec<f32>>,
        dim: usize,
    }
    impl FakeEmbedder {
        fn new(dim: usize) -> Self {
            FakeEmbedder {
                map: std::collections::HashMap::new(),
                dim,
            }
        }
        fn with(mut self, text: &str, v: Vec<f32>) -> Self {
            self.map.insert(text.to_string(), v);
            self
        }
    }
    impl Embedder for FakeEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            Ok(self
                .map
                .get(text)
                .cloned()
                .unwrap_or_else(|| vec![0.0; self.dim]))
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn signature(&self) -> String {
            "fake".into()
        }
    }

    fn mem() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn cfg_weights(fts: f32, vector: f32) -> Config {
        let mut c = Config::default();
        c.search.hybrid_weights.fts = fts;
        c.search.hybrid_weights.vector = vector;
        c
    }

    /// Insert a note with a NULL embedding (FTS-matchable, vector-absent),
    /// returning its public hex id. The FTS row is keyed on the internal rowid.
    fn insert_fts_only(store: &Store, body: &str) -> String {
        let id = "ff0001".to_string();
        store
            .conn
            .execute(
                "INSERT INTO notes(id, body, tags, kind, created_at, created_iso, embedding)
                 VALUES(?1, ?2, '', 'note', 0, '1970-01-01T00:00:00Z', NULL)",
                params![id, body],
            )
            .unwrap();
        let rowid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO notes_fts(rowid, body, tags, origin) VALUES(?1, ?2, '', '')",
                params![rowid, body],
            )
            .unwrap();
        id
    }

    #[test]
    fn recall_empty_store_returns_empty() {
        let store = mem();
        let emb = FakeEmbedder::new(4);
        let hits = recall(&store, &emb, &Config::default(), "anything", 8).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn recall_limit_zero_returns_empty() {
        let store = mem();
        store
            .insert_note("auth", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2);
        let hits = recall(&store, &emb, &Config::default(), "auth", 0).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn recall_limit_truncates_to_n() {
        let store = mem();
        for i in 0..5 {
            store
                .insert_note(
                    &format!("note auth {i}"),
                    "",
                    None,
                    None,
                    "note",
                    &[i as f32, 1.0],
                )
                .unwrap();
        }
        let emb = FakeEmbedder::new(2).with("auth", vec![2.0, 1.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 2).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn recall_dedup_no_duplicate_ids() {
        let store = mem();
        // Matches both channels: body contains "auth" and embedding == qvec.
        store
            .insert_note("auth", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn recall_deterministic_ranking_fixed_data() {
        let store = mem();
        store
            .insert_note("alpha", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        store
            .insert_note("beta", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        store
            .insert_note("gamma", "", None, None, "note", &[0.7, 0.7])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("alpha", vec![1.0, 0.0]);
        let run = || {
            recall(&store, &emb, &Config::default(), "alpha", 8)
                .unwrap()
                .iter()
                .map(|h| h.row.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn recall_weight_fts_only() {
        let store = mem();
        let a = store
            .insert_note("auth token", "", None, None, "note", &[0.0, 0.0])
            .unwrap();
        store
            .insert_note("auth", "", None, None, "note", &[0.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2);
        let hits = recall(&store, &emb, &cfg_weights(1.0, 0.0), "auth token", 8).unwrap();
        // Document matching both query terms ranks first on the FTS channel.
        assert_eq!(hits[0].row.id, a);
    }

    #[test]
    fn recall_weight_vector_only() {
        let store = mem();
        let a = store
            .insert_note("xyz one", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        store
            .insert_note("xyz two", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &cfg_weights(0.0, 1.0), "auth", 8).unwrap();
        // No body contains "auth"; ranking is purely cosine -> note A on top.
        assert_eq!(hits[0].row.id, a);
    }

    #[test]
    fn recall_zero_weights_no_panic() {
        let store = mem();
        store
            .insert_note("auth", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &cfg_weights(0.0, 0.0), "auth", 8).unwrap();
        // With both weights at 0, every channel contributes 0 and the scores just
        // collapse to ~0 — nothing divides by zero.
        for h in &hits {
            assert!(h.score.abs() < 1e-3, "score {} should be ~0", h.score);
        }
    }

    #[test]
    fn recall_fts_only_candidate_included() {
        let store = mem();
        // A vector candidate (gives the cosine channel a positive max)...
        store
            .insert_note("banana", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        // ...and an FTS-only candidate (NULL embedding, matched by keyword).
        let fts_id = insert_fts_only(&store, "auth keyword");
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        let h = hits
            .iter()
            .find(|h| h.row.id == fts_id)
            .expect("fts-only hit present");
        assert!(h.fts > 0.0, "fts channel should be positive");
        assert!(
            h.vector.abs() <= 1e-6,
            "vector channel is 0 for a non-embedded candidate"
        );
    }

    #[test]
    fn recall_vector_only_candidate_included() {
        let store = mem();
        let a = store
            .insert_note("xyzzy", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        store
            .insert_note("plugh", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        let h = hits
            .iter()
            .find(|h| h.row.id == a)
            .expect("vector-only hit present");
        assert_eq!(h.fts, 0.0, "no FTS match -> fts channel is 0");
    }

    #[test]
    fn recall_dimension_drift_cosine_zero_no_panic() {
        let store = mem();
        // Stored vector is 8-dim; query vector is 4-dim -> cosine returns 0.
        store
            .insert_note("note", "", None, None, "note", &[0.1; 8])
            .unwrap();
        let emb = FakeEmbedder::new(4).with("query", vec![0.2; 4]);
        let hits = recall(&store, &emb, &Config::default(), "query", 8).unwrap();
        assert_eq!(hits.len(), 1); // returned, not crashed
    }

    #[test]
    fn recall_bm25_best_match_has_top_fts_contribution() {
        let store = mem();
        let strong = store
            .insert_note("auth auth token", "", None, None, "note", &[0.0, 0.0])
            .unwrap();
        store
            .insert_note("auth", "", None, None, "note", &[0.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2);
        let hits = recall(&store, &emb, &cfg_weights(1.0, 0.0), "auth token", 8).unwrap();
        // RRF: best bm25 (rank 1) gets the largest fts contribution and ranks first.
        assert_eq!(hits[0].row.id, strong);
        let max_fts = hits.iter().map(|h| h.fts).fold(f32::NEG_INFINITY, f32::max);
        assert!((hits[0].fts - max_fts).abs() <= 1e-9);
        assert!(hits[0].fts > 0.0);
    }

    #[test]
    fn recall_single_candidate_rrf_contributions() {
        let store = mem();
        store
            .insert_note("auth", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        assert_eq!(hits.len(), 1);
        // Rank 1 in both channels with equal 0.5 weights and k=60 -> equal
        // contributions of 0.5/61, and score is their sum.
        let expect = 0.5 / 61.0;
        assert!((hits[0].fts - expect).abs() <= 1e-6);
        assert!((hits[0].vector - expect).abs() <= 1e-6);
        assert!((hits[0].score - (hits[0].fts + hits[0].vector)).abs() <= 1e-6);
    }

    #[test]
    fn recall_score_is_sum_of_channel_contributions() {
        let store = mem();
        store
            .insert_note("auth token here", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        store
            .insert_note("auth elsewhere", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        for h in &hits {
            assert!(
                (h.score - (h.fts + h.vector)).abs() <= 1e-6,
                "score == fts + vector"
            );
        }
    }

    #[test]
    fn recall_min_score_drops_weak_candidates() {
        let store = mem();
        for i in 0..6 {
            store
                .insert_note(
                    &format!("auth note {i}"),
                    "",
                    None,
                    None,
                    "note",
                    &[i as f32, 1.0],
                )
                .unwrap();
        }
        let emb = FakeEmbedder::new(2).with("auth", vec![5.0, 1.0]);
        let cfg = Config::default();
        let unfiltered = recall(&store, &emb, &cfg, "auth", 8).unwrap();
        assert!(unfiltered.len() > 1);
        let top = unfiltered[0].score;
        // A floor just under the top score keeps only the strongest hit(s).
        let opts = RecallOpts {
            limit: 8,
            min_score: top - 1e-6,
            ..Default::default()
        };
        let filtered = recall_with(&store, &emb, &cfg, "auth", &opts).unwrap();
        assert!(filtered.len() < unfiltered.len());
        assert!(filtered.iter().all(|h| h.score >= top - 1e-6));
    }

    #[test]
    fn recall_tag_filter_narrows_candidates() {
        let store = mem();
        let a = store
            .insert_note("auth jwt", "auth", None, None, "note", &[1.0, 0.0])
            .unwrap();
        store
            .insert_note("auth oauth", "other", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let opts = RecallOpts {
            limit: 8,
            tag: Some("auth"),
            ..Default::default()
        };
        let hits = recall_with(&store, &emb, &Config::default(), "auth", &opts).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.id, a);
    }

    #[test]
    fn recall_origin_prefix_filter_limits_to_source() {
        let store = mem();
        let a = store
            .insert_note(
                "auth a",
                "",
                None,
                Some("arch.md › Auth"),
                "chunk",
                &[1.0, 0.0],
            )
            .unwrap();
        store
            .insert_note("auth b", "", None, Some("readme.md"), "chunk", &[1.0, 0.0])
            .unwrap();
        let emb = FakeEmbedder::new(2).with("auth", vec![1.0, 0.0]);
        let opts = RecallOpts {
            limit: 8,
            origin_prefix: Some("arch.md"),
            ..Default::default()
        };
        let hits = recall_with(&store, &emb, &Config::default(), "auth", &opts).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.id, a);
    }

    #[test]
    fn recall_graph_channel_boosts_neighbors_only_when_opted_in() {
        let store = mem();
        let a = store
            .insert_note("alpha", "", None, None, "note", &[1.0, 0.0])
            .unwrap();
        let b = store
            .insert_note("beta", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        let c = store
            .insert_note("gamma", "", None, None, "note", &[0.0, 1.0])
            .unwrap();
        store
            .insert_edge(&a, "links_to", Some(&b), &b, "wikilink")
            .unwrap();
        let emb = FakeEmbedder::new(2); // no query vector -> fts/vector flat
        let opts = RecallOpts {
            limit: 8,
            related: Some(a.as_str()),
            ..Default::default()
        };

        // Opted in (graph weight > 0): b (neighbor of a) is boosted; c is not.
        let mut cfg = cfg_weights(0.0, 0.0);
        cfg.search.hybrid_weights.graph = 1.0;
        let hits = recall_with(&store, &emb, &cfg, "zzz", &opts).unwrap();
        let hb = hits.iter().find(|h| h.row.id == b).unwrap();
        let hc = hits.iter().find(|h| h.row.id == c).unwrap();
        assert!(hb.graph > 0.0, "neighbor gets a graph contribution");
        assert_eq!(hc.graph, 0.0, "non-neighbor gets none");
        assert!(hb.score > hc.score, "neighbor ranks above non-neighbor");

        // Off by default (graph weight 0): related is inert, every graph == 0.
        let hits0 = recall_with(&store, &emb, &Config::default(), "zzz", &opts).unwrap();
        assert!(hits0.iter().all(|h| h.graph == 0.0));
    }

    #[test]
    fn recall_missing_row_silently_dropped() {
        let store = mem();
        // Phantom FTS rowid with no `notes` row: excluded by the fts_search join,
        // so it never reaches scoring -> no error, no hit.
        store
            .conn
            .execute(
                "INSERT INTO notes_fts(rowid, body, tags, origin) VALUES(999, 'auth', '', '')",
                [],
            )
            .unwrap();
        let emb = FakeEmbedder::new(2);
        let hits = recall(&store, &emb, &Config::default(), "auth", 8).unwrap();
        assert!(hits.is_empty()); // dropped, no error
    }
}
