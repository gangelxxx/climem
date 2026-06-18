//! The default `local` provider: an offline, deterministic embedder.
//!
//! It's the hashing trick applied to both word tokens and character n-grams. The
//! char n-grams are what catch morphology (which matters a lot for Russian
//! inflection), so cosine similarity ends up reflecting surface/lexical
//! relatedness on a sliding scale rather than all-or-nothing. There are no
//! learned weights and no network calls. When you want a real semantic model,
//! switch to the `api` provider; nothing else in the system has to change.

use super::Embedder;
use crate::util::Result;

pub struct HashingEmbedder {
    dim: usize,
    model: String,
}

impl HashingEmbedder {
    pub fn new(dim: usize, model: String) -> Self {
        let dim = if dim == 0 { 384 } else { dim };
        HashingEmbedder { dim, model }
    }

    fn add_feature(&self, vec: &mut [f32], feature_kind: u8, feature: &str, weight: f32) {
        let h = fnv1a(feature_kind, feature);
        let idx = (h % self.dim as u64) as usize;
        // Signed hashing: a colliding pair of features cancels out on average
        // instead of always piling up, so the noise stays near zero.
        let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
        vec[idx] += sign * weight;
    }
}

impl Embedder for HashingEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut vec = vec![0.0f32; self.dim];
        let lower = text.to_lowercase();

        for word in tokenize(&lower) {
            // The word itself.
            self.add_feature(&mut vec, b'w', &word, 1.0);
            // Padded character 3-grams, so inflected forms still overlap.
            let padded = format!("^{word}$");
            let chars: Vec<char> = padded.chars().collect();
            if chars.len() >= 3 {
                for w in chars.windows(3) {
                    let g: String = w.iter().collect();
                    self.add_feature(&mut vec, b'g', &g, 0.5);
                }
            }
        }

        // L2-normalize so cosine is just the dot product, and short vs long texts
        // get compared on even footing.
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut vec {
                *x /= norm;
            }
        }
        Ok(vec)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn signature(&self) -> String {
        format!("local:{}:{}", self.model, self.dim)
    }
}

/// Split into Unicode-aware alphanumeric tokens, throwing away punctuation.
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 64-bit FNV-1a over a one-byte kind tag plus the feature bytes.
fn fnv1a(kind: u8, s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    h ^= kind as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::cosine;

    fn emb(dim: usize) -> HashingEmbedder {
        HashingEmbedder::new(dim, "test-model".into())
    }

    #[test]
    fn hashing_new_zero_dim_defaults_384() {
        let e = emb(0);
        assert_eq!(e.dim(), 384);
        assert!(e.signature().ends_with(":384"));
    }

    #[test]
    fn hashing_embed_len_equals_dim() {
        assert_eq!(emb(16).embed("hello world").unwrap().len(), 16);
    }

    #[test]
    fn hashing_embed_deterministic() {
        let e = emb(64);
        assert_eq!(
            e.embed("привет мир").unwrap(),
            e.embed("привет мир").unwrap()
        );
    }

    #[test]
    fn hashing_embed_l2_normalized() {
        let v = emb(128).embed("some non empty text").unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() <= 1e-5, "norm was {norm}");
    }

    #[test]
    fn hashing_self_cosine_is_one() {
        let v = emb(128).embed("авторизация и токены").unwrap();
        assert!((cosine(&v, &v) - 1.0).abs() <= 1e-5);
    }

    #[test]
    fn hashing_embed_empty_text_all_zeros() {
        let e = emb(32);
        assert!(e.embed("").unwrap().iter().all(|&x| x == 0.0));
        // Pure punctuation tokenizes to nothing -> zero vector.
        assert!(e.embed(",.!").unwrap().iter().all(|&x| x == 0.0));
    }

    #[test]
    fn hashing_russian_morphology_graded() {
        let e = emb(384);
        let kot = e.embed("кот").unwrap();
        let koty = e.embed("коты").unwrap();
        let sobaka = e.embed("собака").unwrap();
        // Inflected forms (shared char n-grams) are closer than unrelated words.
        assert!(cosine(&kot, &koty) > cosine(&kot, &sobaka));
    }

    #[test]
    fn hashing_embed_case_insensitive() {
        let e = emb(64);
        assert_eq!(e.embed("Слово").unwrap(), e.embed("слово").unwrap());
    }

    #[test]
    fn hashing_signature_format() {
        assert_eq!(emb(384).signature(), "local:test-model:384");
    }

    #[test]
    fn tokenize_unicode_and_punctuation() {
        assert_eq!(tokenize("«Привет, мир! 42»"), vec!["Привет", "мир", "42"]);
    }

    #[test]
    fn fnv1a_kind_tag_differentiates() {
        assert_ne!(fnv1a(b'w', "x"), fnv1a(b'g', "x"));
        // Deterministic.
        assert_eq!(fnv1a(b'w', "abc"), fnv1a(b'w', "abc"));
    }
}
