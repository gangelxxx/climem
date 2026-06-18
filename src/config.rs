//! `config.json` — every tunable lives in one file next to the store (desc.md §5).
//!
//! At runtime we work with the typed `Config`. The `get`/`set`/`show` commands,
//! though, poke at the raw JSON instead, so any keys we don't know about (yours,
//! or a future version's) survive an edit untouched.

use crate::util::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub embedding: Embedding,
    #[serde(default)]
    pub search: Search,
    #[serde(default)]
    pub chunking: Chunking,
    #[serde(default = "default_version")]
    pub version: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Embedding {
    /// "local" is the offline hashing embedder (default); "api" is a remote model.
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_dimension")]
    pub dimension: usize,
    /// For a future local neural provider: where weights live.
    #[serde(default = "default_weights_path")]
    pub weights_path: String,
    /// For "api": endpoint URL.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// For "api": the NAME of the env var that holds the key, never the key itself.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// For "api": which request/response shape to use, "openai" or "ollama".
    #[serde(default = "default_api_format")]
    pub api_format: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Search {
    #[serde(default)]
    pub hybrid_weights: HybridWeights,
    /// How many keyword candidates to pull before scoring. Keeping this wide and
    /// independent of `limit` gives rank fusion (RRF) enough of the tail to work
    /// with, and makes the final k-cut safe (token-efficiency-plan R2). The vector
    /// channel already scans everything, so it needs no equivalent.
    #[serde(default = "default_candidates")]
    pub candidates: usize,
    /// The `k` in Reciprocal Rank Fusion. Larger flattens the weighting across
    /// ranks, smaller sharpens it; 60 is the usual OpenSearch/TREC default. Plan R1.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: usize,
    /// How many graph hops the `recall --related <id>` proximity channel explores
    /// (D8). Defaults to 2; `cm related` uses its own `--depth` (default 1). Kept
    /// configurable so the recall channel's reach is no longer a silent hardcode.
    #[serde(default = "default_graph_depth")]
    pub graph_depth: usize,
    /// Optional predicate filter for the `recall --related` graph channel (D9):
    /// when set (and non-empty), the channel only follows edges with this
    /// (normalized) predicate, mirroring `related --predicate`. Empty/absent =
    /// follow every predicate.
    #[serde(default)]
    pub graph_predicate: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HybridWeights {
    #[serde(default = "half")]
    pub fts: f32,
    #[serde(default = "half")]
    pub vector: f32,
    /// Weight of the graph-proximity channel for `recall --related <id>` (RRF).
    /// Defaults to 0.0, i.e. the channel is off unless you opt in, so a plain
    /// `recall` behaves exactly as before.
    #[serde(default)]
    pub graph: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chunking {
    /// The chunk-size budget. Despite the name, `chunk::{markdown,text}` count
    /// this in words (`split_whitespace`), not real BPE tokens; roughly 200 words
    /// is about 256 tokens. Smaller chunks mean fewer body tokens per recalled
    /// note (plan C1). The field name stays "max_tokens" only so old configs keep
    /// loading; read it as "max words".
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// How many words to repeat across a chunk boundary (about 10–15% of `max_tokens`).
    #[serde(default = "default_overlap")]
    pub overlap: usize,
}

fn default_version() -> u32 {
    // v2 is the md-as-truth layout: notes/*.md + imports/ are the source of
    // truth, and store.db is just an index `reindex` rebuilds (desc.md §3).
    2
}
fn default_provider() -> String {
    "local".into()
}
fn default_model() -> String {
    "hash-ngram-v1".into()
}
fn default_dimension() -> usize {
    384
}
fn default_weights_path() -> String {
    "./models".into()
}
fn default_api_key_env() -> String {
    "MEMORY_EMBED_API_KEY".into()
}
fn default_api_format() -> String {
    "openai".into()
}
fn default_max_tokens() -> usize {
    200
}
fn default_overlap() -> usize {
    32
}
fn default_candidates() -> usize {
    50
}
fn default_rrf_k() -> usize {
    60
}
fn default_graph_depth() -> usize {
    2
}
fn half() -> f32 {
    0.5
}

impl Default for Embedding {
    fn default() -> Self {
        Embedding {
            provider: default_provider(),
            model: default_model(),
            dimension: default_dimension(),
            weights_path: default_weights_path(),
            endpoint: None,
            api_key_env: default_api_key_env(),
            api_format: default_api_format(),
        }
    }
}
impl Default for HybridWeights {
    fn default() -> Self {
        HybridWeights {
            fts: 0.5,
            vector: 0.5,
            graph: 0.0,
        }
    }
}
impl Default for Search {
    fn default() -> Self {
        Search {
            hybrid_weights: HybridWeights::default(),
            candidates: default_candidates(),
            rrf_k: default_rrf_k(),
            graph_depth: default_graph_depth(),
            graph_predicate: String::new(),
        }
    }
}
impl Default for Chunking {
    fn default() -> Self {
        Chunking {
            max_tokens: default_max_tokens(),
            overlap: default_overlap(),
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        Config {
            name: String::new(),
            embedding: Embedding::default(),
            search: Search::default(),
            chunking: Chunking::default(),
            version: default_version(),
        }
    }
}

impl Config {
    pub fn new_named(name: &str) -> Self {
        Config {
            name: name.to_string(),
            ..Config::default()
        }
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|_| {
            AppError::with_hint(
                format!("config.json not found at {}", path.display()),
                "Run `cm init <path>` first, or set MEMORY_DIR to a memory folder.",
            )
        })?;
        let cfg: Config = serde_json::from_str(&text)
            .map_err(|e| AppError::new(format!("config.json is invalid: {e}")))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

/// Load the raw JSON value (preserving unknown keys) for `get`/`set`.
pub fn load_raw(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|_| {
        AppError::with_hint(
            format!("config.json not found at {}", path.display()),
            "Run `cm init <path>` first.",
        )
    })?;
    Ok(serde_json::from_str(&text)?)
}

pub fn save_raw(path: &Path, v: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

/// Look up a dotted key like `embedding.model`.
pub fn get_path<'a>(v: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in dotted.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Set a dotted key, building any missing objects along the way. We try to parse
/// the value as JSON first, so `384`, `true`, and `0.7` keep their real types;
/// anything that doesn't parse is stored as a plain string.
pub fn set_path(v: &mut Value, dotted: &str, raw: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string()));
    let parts: Vec<&str> = dotted.split('.').collect();
    let mut cur = v;
    for p in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            return Err(AppError::new(format!(
                "cannot descend into non-object at '{p}'"
            )));
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(p.to_string())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    let last = parts[parts.len() - 1];
    cur.as_object_mut()
        .ok_or_else(|| AppError::new("config root is not an object"))?
        .insert(last.to_string(), parsed);
    Ok(())
}

/// Walk the JSON and mask any value whose key looks secret. We never store raw
/// secrets ourselves, but someone might have hand-edited one in, so play it safe.
pub fn mask_secrets(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                let lk = k.to_lowercase();
                let looks_secret = (lk.contains("key")
                    || lk.contains("secret")
                    || lk.contains("token")
                    || lk.contains("password"))
                    && !lk.contains("api_key_env"); // this holds only the env-var name
                if looks_secret {
                    if let Value::String(s) = val {
                        if !s.is_empty() {
                            out.insert(k.clone(), Value::String("***".into()));
                            continue;
                        }
                    }
                }
                out.insert(k.clone(), mask_secrets(val));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn as_value(c: &Config) -> Value {
        serde_json::to_value(c).unwrap()
    }

    // ---- typed defaults & serde ----------------------------------------

    #[test]
    fn config_default_matches_explicit_values() {
        let c = Config::default();
        assert_eq!(c.embedding.dimension, 384);
        assert_eq!(c.embedding.provider, "local");
        assert_eq!(c.embedding.model, "hash-ngram-v1");
        assert_eq!(c.search.hybrid_weights.fts, 0.5);
        assert_eq!(c.search.hybrid_weights.vector, 0.5);
        assert_eq!(c.search.hybrid_weights.graph, 0.0); // graph channel off by default

        assert_eq!(c.search.candidates, 50);
        assert_eq!(c.search.rrf_k, 60);
        assert_eq!(c.search.graph_depth, 2); // recall --related explores 2 hops
        assert!(c.search.graph_predicate.is_empty()); // no predicate filter by default
        assert_eq!(c.chunking.max_tokens, 200);
        assert_eq!(c.chunking.overlap, 32);
        assert_eq!(c.version, 2);
    }

    #[test]
    fn deserialize_empty_object_applies_all_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(as_value(&c), as_value(&Config::default()));
    }

    #[test]
    fn deserialize_partial_embedding_keeps_other_defaults() {
        let c: Config = serde_json::from_str(r#"{"embedding":{"provider":"api"}}"#).unwrap();
        assert_eq!(c.embedding.provider, "api");
        assert_eq!(c.embedding.model, "hash-ngram-v1");
        assert_eq!(c.embedding.dimension, 384);
    }

    #[test]
    fn config_serde_round_trip_in_memory() {
        let c = Config::default();
        let text = serde_json::to_string_pretty(&c).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(as_value(&c), as_value(&back));
        // endpoint None serializes to null.
        assert_eq!(as_value(&c)["embedding"]["endpoint"], Value::Null);
    }

    #[test]
    fn deserialize_drops_unknown_keys_typed() {
        let c: Config = serde_json::from_str(r#"{"future_flag": true, "name": "x"}"#).unwrap();
        assert_eq!(c.name, "x");
        assert!(as_value(&c).get("future_flag").is_none());
    }

    // ---- get_path / set_path -------------------------------------------

    #[test]
    fn get_path_simple_and_nested() {
        let v = json!({"name": "m", "embedding": {"model": "hash-ngram-v1"}});
        assert_eq!(get_path(&v, "name"), Some(&json!("m")));
        assert_eq!(
            get_path(&v, "embedding.model"),
            Some(&json!("hash-ngram-v1"))
        );
    }

    #[test]
    fn get_path_missing_and_descend_into_scalar() {
        let v = json!({"name": "m"});
        assert_eq!(get_path(&v, "nope"), None);
        assert_eq!(get_path(&v, ""), None);
        // Descending past a scalar yields None.
        assert_eq!(get_path(&v, "name.foo"), None);
    }

    #[test]
    fn set_path_type_inference() {
        let mut v = json!({});
        set_path(&mut v, "i", "384").unwrap();
        set_path(&mut v, "b", "true").unwrap();
        set_path(&mut v, "f", "0.7").unwrap();
        set_path(&mut v, "s", "api").unwrap();
        set_path(&mut v, "qs", "\"api\"").unwrap();
        assert_eq!(v["i"], json!(384));
        assert_eq!(v["b"], json!(true));
        assert_eq!(v["f"], json!(0.7));
        assert_eq!(v["s"], json!("api"));
        assert_eq!(v["qs"], json!("api"));
    }

    #[test]
    fn set_path_creates_intermediate_objects() {
        let mut v = json!({});
        set_path(&mut v, "a.b.c", "1").unwrap();
        assert_eq!(v["a"]["b"]["c"], json!(1));
    }

    #[test]
    fn set_path_descend_into_non_object_errors() {
        let mut v = json!({"a": 5});
        let err = set_path(&mut v, "a.b.c", "1").unwrap_err();
        assert!(err.msg.contains("cannot descend into non-object"));
    }

    #[test]
    fn set_path_overwrites_existing() {
        let mut v = json!({"embedding": {"model": "old", "dimension": 384}});
        set_path(&mut v, "embedding.model", "new").unwrap();
        assert_eq!(v["embedding"]["model"], json!("new"));
        assert_eq!(v["embedding"]["dimension"], json!(384)); // neighbor preserved
    }

    #[test]
    fn set_path_root_not_object_errors() {
        let mut arr = json!([1, 2]);
        assert!(set_path(&mut arr, "key", "1")
            .unwrap_err()
            .msg
            .contains("config root is not an object"));
        let mut null = Value::Null;
        assert!(set_path(&mut null, "key", "1").is_err());
    }

    #[test]
    fn set_path_invalid_json_stored_as_string() {
        let mut v = json!({});
        set_path(&mut v, "k", "{").unwrap();
        assert_eq!(v["k"], Value::String("{".into()));
    }

    // ---- mask_secrets ---------------------------------------------------

    #[test]
    fn mask_secrets_masks_only_nonempty_string_secrets() {
        let v = json!({
            "api_key": "sk-1",
            "token": 123,            // not a string -> untouched
            "empty_secret": "",      // empty -> untouched
            "Password": "p",         // case-insensitive match
            "name": "m",
        });
        let m = mask_secrets(&v);
        assert_eq!(m["api_key"], json!("***"));
        assert_eq!(m["token"], json!(123));
        assert_eq!(m["empty_secret"], json!(""));
        assert_eq!(m["Password"], json!("***"));
        assert_eq!(m["name"], json!("m"));
    }

    #[test]
    fn mask_secrets_preserves_api_key_env() {
        let v = json!({"api_key_env": "MEMORY_EMBED_API_KEY"});
        assert_eq!(
            mask_secrets(&v)["api_key_env"],
            json!("MEMORY_EMBED_API_KEY")
        );
    }

    #[test]
    fn mask_secrets_recurses_objects_not_arrays() {
        let v = json!({
            "embedding": {"api_key": "sk-deep"},
            "tokens": ["secret-looking-string"],
        });
        let m = mask_secrets(&v);
        assert_eq!(m["embedding"]["api_key"], json!("***"));
        // Arrays are returned as-is (not traversed).
        assert_eq!(m["tokens"], json!(["secret-looking-string"]));
        // Input is not mutated.
        assert_eq!(v["embedding"]["api_key"], json!("sk-deep"));
    }

    // ---- FS round-trips -------------------------------------------------

    #[test]
    fn load_save_typed_round_trip_tempfile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let c = Config::new_named("project");
        c.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(as_value(&c), as_value(&back));
    }

    #[test]
    fn load_missing_file_errors_with_hint() {
        let dir = TempDir::new().unwrap();
        let err = Config::load(&dir.path().join("nope.json")).unwrap_err();
        assert!(err.msg.contains("config.json not found"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn load_invalid_json_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        assert!(Config::load(&path)
            .unwrap_err()
            .msg
            .contains("config.json is invalid"));
    }

    #[test]
    fn load_raw_preserves_unknown_keys() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"future_flag": true, "name": "x"}"#).unwrap();
        let raw = load_raw(&path).unwrap();
        assert_eq!(raw["future_flag"], json!(true));
    }

    #[test]
    fn save_raw_load_raw_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let v = json!({"name": "m", "extra": {"nested": [1, 2, 3]}});
        save_raw(&path, &v).unwrap();
        assert_eq!(load_raw(&path).unwrap(), v);
    }

    #[test]
    fn load_raw_missing_file_errors_with_hint() {
        let dir = TempDir::new().unwrap();
        let err = load_raw(&dir.path().join("nope.json")).unwrap_err();
        assert!(err.msg.contains("config.json not found"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn set_path_then_config_validation_rejects_bad_type() {
        let mut v = as_value(&Config::default());
        // A non-numeric dimension is accepted as a raw string by set_path...
        set_path(&mut v, "embedding.dimension", "abc").unwrap();
        // ...but fails to deserialize back into a typed Config (commands.rs guard).
        assert!(serde_json::from_value::<Config>(v).is_err());
    }
}
