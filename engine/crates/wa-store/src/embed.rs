//! Embedding providers. 384 dimensions (MiniLM-L6-v2) everywhere.

pub const DIM: usize = 384;

pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// Deterministic hashed character-trigram embedder. Offline, zero-dependency;
/// used in tests and as a crude fallback when the ONNX model is unavailable.
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_embed(t)).collect())
    }
}

fn hash_embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    let chars: Vec<char> = format!("  {}  ", text.to_lowercase()).chars().collect();
    for w in chars.windows(3) {
        // FNV-1a over the trigram.
        let mut h: u32 = 0x811c_9dc5;
        for c in w {
            for b in c.to_string().bytes() {
                h ^= b as u32;
                h = h.wrapping_mul(0x0100_0193);
            }
        }
        v[(h as usize) % DIM] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

#[cfg(feature = "fastembed")]
pub struct MiniLm(std::sync::Mutex<fastembed::TextEmbedding>);

#[cfg(feature = "fastembed")]
impl MiniLm {
    pub fn new(cache_dir: std::path::PathBuf) -> anyhow::Result<Self> {
        let opts = fastembed::InitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false);
        Ok(Self(std::sync::Mutex::new(fastembed::TextEmbedding::try_new(opts)?)))
    }
}

#[cfg(feature = "fastembed")]
impl Embedder for MiniLm {
    fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let docs: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        self.0.lock().unwrap().embed(docs, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embed_is_normalized_and_similar() {
        let e = HashEmbedder;
        let v = e.embed(&["dinner tonight", "dinner tomorrow", "quantum physics"]).unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!((dot(&v[0], &v[0]) - 1.0).abs() < 1e-4);
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]));
    }
}
