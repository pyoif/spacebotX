//! Embedding generation via fastembed.

use crate::error::{LlmError, Result};
use std::path::Path;
use std::sync::Arc;

/// Embedding model wrapper with thread-safe sharing.
///
/// fastembed's TextEmbedding is not Send, so we hold it behind an Arc and
/// use spawn_blocking to call into it from async contexts.
pub struct EmbeddingModel {
    model: Arc<fastembed::TextEmbedding>,
}

impl EmbeddingModel {
    /// Create a new embedding model, storing downloaded model files in `cache_dir`.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let options = fastembed::InitOptions::default()
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true);

        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| LlmError::EmbeddingFailed(e.to_string()))?;

        Ok(Self {
            model: Arc::new(model),
        })
    }

    /// Generate embeddings for multiple texts (blocking).
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.model
            .embed(texts, None)
            .map_err(|e| LlmError::EmbeddingFailed(e.to_string()).into())
    }

    /// Generate embedding for a single text (blocking).
    pub fn embed_one_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.embed(vec![text.to_string()])?;
        Ok(embeddings.into_iter().next().unwrap_or_default())
    }

    /// Generate embedding for a single text (async, spawns blocking task).
    pub async fn embed_one(self: &Arc<Self>, text: &str) -> Result<Vec<f32>> {
        #[cfg(feature = "metrics")]
        let _timer = crate::telemetry::Metrics::global()
            .memory_embedding_duration_seconds
            .start_timer();

        let text = text.to_string();
        let model = self.model.clone();
        let result = tokio::task::spawn_blocking(move || {
            model.embed(vec![text], None).map_err(|e| {
                crate::Error::from(crate::error::LlmError::EmbeddingFailed(e.to_string()))
            })
        })
        .await
        .map_err(|e| crate::Error::Other(anyhow::anyhow!("embedding task failed: {}", e)))??;

        Ok(result.into_iter().next().unwrap_or_default())
    }

    /// Generate embeddings for many texts in one call (async, spawns blocking
    /// task). fastembed accepts a batch natively, so the boot-time backfills
    /// use this rather than a loop of [`Self::embed_one`] — one model-inference
    /// call per batch instead of one per row.
    ///
    /// The returned vector is aligned with `texts` (fastembed preserves input
    /// order) and every embedding is L2-normalized, matching the write path.
    pub async fn embed_many(self: &Arc<Self>, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        #[cfg(feature = "metrics")]
        let _timer = crate::telemetry::Metrics::global()
            .memory_embedding_duration_seconds
            .start_timer();

        let model = self.model.clone();
        tokio::task::spawn_blocking(move || {
            model.embed(texts, None).map_err(|e| {
                crate::Error::from(crate::error::LlmError::EmbeddingFailed(e.to_string()))
            })
        })
        .await
        .map_err(|e| crate::Error::Other(anyhow::anyhow!("embedding task failed: {}", e)))?
    }
}

/// Async function to embed text using a shared model.
pub async fn embed_text(model: &Arc<EmbeddingModel>, text: &str) -> Result<Vec<f32>> {
    model.embed_one(text).await
}

/// Shared embedding model for tests. Downloads once into a stable cache
/// directory and is reused by every test in the binary, so CI can cache the
/// model files across runs instead of fetching them from Hugging Face on
/// each test that needs embeddings.
#[cfg(test)]
pub(crate) fn shared_test_model() -> std::sync::Arc<EmbeddingModel> {
    use std::sync::{Arc, OnceLock};
    static MODEL: OnceLock<Arc<EmbeddingModel>> = OnceLock::new();
    Arc::clone(MODEL.get_or_init(|| {
        let cache_dir = std::env::temp_dir().join("spacebot-test-embedding-cache");
        std::fs::create_dir_all(&cache_dir).expect("failed to create embedding cache dir");
        Arc::new(EmbeddingModel::new(&cache_dir).expect("failed to initialize embedding model"))
    }))
}
