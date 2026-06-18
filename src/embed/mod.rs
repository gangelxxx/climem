//! All the embedding providers sit behind one trait (desc.md §3 "Один интерфейс —
//! любой бэкенд"). The default `local` provider is an offline, deterministic
//! hashing embedder; `api` talks to a remote neural model over HTTP and only
//! exists when the `api` feature is compiled in.

#[cfg(feature = "api")]
mod api;
mod hashing;

use crate::config::Config;
use crate::util::{AppError, Result};

pub trait Embedder {
    /// Turn text into a fixed-length vector. For a given provider this has to be
    /// deterministic (same text in, same vector out).
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    #[allow(dead_code)] // part of the trait; not every caller needs it
    fn dim(&self) -> usize;
    /// A short id we stash in `meta` so we can later notice if the provider or
    /// dimension changed out from under the stored vectors.
    fn signature(&self) -> String;
}

/// Build whichever embedder `config.embedding.provider` asks for.
pub fn build(cfg: &Config) -> Result<Box<dyn Embedder>> {
    match cfg.embedding.provider.as_str() {
        "local" => Ok(Box::new(hashing::HashingEmbedder::new(
            cfg.embedding.dimension,
            cfg.embedding.model.clone(),
        ))),
        "api" => {
            #[cfg(feature = "api")]
            {
                Ok(Box::new(api::ApiEmbedder::from_config(cfg)?))
            }
            #[cfg(not(feature = "api"))]
            {
                let _ = cfg;
                Err(AppError::with_hint(
                    "embedding.provider = \"api\" but this binary was built without the `api` feature",
                    "Rebuild with `cargo build --release --features api`, or `cm config set embedding.provider local`.",
                ))
            }
        }
        other => Err(AppError::with_hint(
            format!("unknown embedding.provider '{other}'"),
            "Valid providers: \"local\" (offline) or \"api\" (neural over HTTP).",
        )),
    }
}

/// Cosine similarity. If the two vectors are different lengths we just return 0
/// instead of panicking, which is what we want when the provider or dimension
/// changed between re-embeds.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Pack a vector into a little-endian f32 blob to store in SQLite.
pub fn encode(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Unpack that little-endian f32 blob back into a vector.
pub fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::util::AppError;

    #[test]
    fn cosine_identical_is_one() {
        let v = [1.0, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() <= 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() <= 1e-6);
    }

    #[test]
    fn cosine_mismatched_len_returns_zero() {
        // Early return: exactly 0.0, not arithmetic.
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_opposite_is_minus_one() {
        assert!((cosine(&[1.0, 2.0], &[-1.0, -2.0]) + 1.0).abs() <= 1e-6);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let v = vec![1.5, -2.25, 0.0, 100.0, -0.5];
        assert_eq!(decode(&encode(&v)), v);
    }

    #[test]
    fn encode_is_little_endian() {
        assert_eq!(encode(&[1.0f32]), vec![0, 0, 128, 63]);
    }

    #[test]
    fn decode_drops_trailing_partial_bytes() {
        // 6 bytes -> one full f32, trailing 2 bytes dropped (chunks_exact).
        let bytes = vec![0, 0, 128, 63, 1, 2];
        assert_eq!(decode(&bytes), vec![1.0f32]);
    }

    #[test]
    fn encode_decode_empty() {
        assert!(encode(&[]).is_empty());
        assert!(decode(&[]).is_empty());
    }

    #[test]
    fn encode_decode_special_floats_bits() {
        let v = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.0f32];
        let back = decode(&encode(&v));
        for (a, b) in v.iter().zip(back.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn build_local_provider_ok() {
        let emb = build(&Config::default()).unwrap();
        assert!(emb.signature().starts_with("local:"));
    }

    /// `Box<dyn Embedder>` has no `Debug`, so `unwrap_err()` won't compile.
    fn build_err(cfg: &Config) -> AppError {
        match build(cfg) {
            Ok(_) => panic!("expected build() to fail"),
            Err(e) => e,
        }
    }

    #[test]
    fn build_unknown_provider_errors_with_hint() {
        let mut cfg = Config::default();
        cfg.embedding.provider = "bogus".into();
        let err = build_err(&cfg);
        assert!(err.msg.contains("unknown embedding.provider"));
        assert!(err.hint.is_some());
    }

    #[cfg(not(feature = "api"))]
    #[test]
    fn build_api_without_feature_errors() {
        let mut cfg = Config::default();
        cfg.embedding.provider = "api".into();
        assert!(build_err(&cfg).hint.is_some());
    }
}
