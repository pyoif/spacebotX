//! SQLite-side storage for memory and chronicle search (stage 1 of the
//! LanceDB → SQLite migration).
//!
//! Stage 1A added the schema (`migrations/20260816000001_memory_search_sqlite.sql`)
//! and the pure analyzer (`src/memory/analyzer.rs`) without wiring either into
//! the live path. This module is the operational half: the vector tables that
//! replace LanceDB's `memory_embeddings` / `chronicle_embeddings`, the FTS5
//! queries that replace its tantivy inverted index, and the backfills that bring
//! already-stored rows up to the analysed representation.
//!
//! ## Why a backfill is needed at all
//!
//! `search_text` holds `<stemmed tokens> <raw text>` and FTS5 has no stemmer, so
//! the stemming happens in Rust. The migration's SQL backfill could only copy
//! the *raw* half (SQL cannot stem), which leaves a legacy row findable by its
//! whole words but invisible to a stem-sharing query: `memories` does not match
//! a row containing `memory`, because the stemmed half is missing. FTS5 will
//! never bridge that gap on its own — stemmed and unstemmed tokens do not
//! cross-match — so the raw copy has to be rewritten before the read path can
//! rely on these tables.
//!
//! ## Vectors are stored raw and scored in Rust
//!
//! `memory_vectors.embedding` holds the f32 little-endian bytes fastembed
//! produced, and [`MemoryVectorTable::vector_search`] does an exact dot-product
//! scan. That is not a ranking change: `create_indexes()` had zero callers, so
//! the LanceDB path was already an exact brute-force scan. fastembed
//! L2-normalizes on both the write and the query path, so the dot product is
//! numerically identical to the cosine similarity LanceDB reported.

use crate::error::Result;
use crate::memory::analyzer;

use anyhow::Context as _;
use sqlx::{Row, SqlitePool};

/// How many rows one backfill statement examines. Bounds peak memory on a large
/// store; the loop it feeds is keyset-paginated, so this affects only batch size.
const BACKFILL_BATCH: i64 = 500;

/// Embedding model identifier recorded alongside every stored vector.
///
/// fastembed's default `TextEmbedding` model is `AllMiniLML6V2`; the string is
/// recorded so a future model change is detectable (rows written by a different
/// model are skipped by the `model = ?` filter rather than scored against
/// incompatible vectors). Keep in sync with `fastembed::InitOptions::default()`.
pub const EMBEDDING_MODEL_ID: &str = "AllMiniLML6V2";

/// Split a checkpoint's indexed text the way the write path does. Mirrors
/// `MemorySearch::embed_chronicle_checkpoint`'s `format!("{} — {}", title, summary)`
/// and the migration's `title || ' ' || summary` backfill.
fn chronicle_source_text(title: &str, summary: &str) -> String {
    format!("{title} {summary}")
}

/// The analysed `search_text` for a chronicle checkpoint — the value the write
/// path stores on insert and the backfill stores on rewrite. One function so
/// the two cannot disagree about what is indexed.
pub fn chronicle_search_text(title: &str, summary: &str) -> String {
    analyzer::search_text(&chronicle_source_text(title, summary))
}

/// Serialize an embedding as little-endian f32 bytes — the format
/// `memory_vectors.embedding` / `chronicle_vectors.embedding` document.
pub fn encode_vector(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for value in embedding {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

/// Decode a little-endian f32 blob. Returns `None` when the byte length does not
/// describe exactly `dim` floats, so a truncated or foreign blob is skipped
/// instead of being silently reinterpreted.
pub fn decode_vector(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if blob.len() != dim * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

/// Dot product of two equal-length vectors. Both sides are L2-normalized by
/// fastembed, so this is the cosine similarity.
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// Vector storage (replaces the LanceDB `memory_embeddings` table)
// ---------------------------------------------------------------------------

/// SQLite-backed replacement for LanceDB's `EmbeddingTable`. Stores one vector
/// per memory in `memory_vectors`, keyed by the memory it describes; the text
/// stays in `memories.content` (the LanceDB table duplicated it only so its
/// FTS index had something to index, which `memories_fts` now covers directly).
#[derive(Debug, Clone)]
pub struct MemoryVectorTable {
    pool: SqlitePool,
}

impl MemoryVectorTable {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert or replace the vector for a memory. `dim` is derived from the
    /// slice, so the stored metadata can never disagree with the blob.
    pub async fn store(&self, memory_id: &str, embedding: &[f32]) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO memory_vectors (memory_id, model, dim, embedding)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(memory_id) DO UPDATE SET
                model = excluded.model,
                dim = excluded.dim,
                embedding = excluded.embedding,
                created_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(memory_id)
        .bind(EMBEDDING_MODEL_ID)
        .bind(embedding.len() as i64)
        .bind(encode_vector(embedding))
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to store vector for memory {memory_id}"))?;
        Ok(())
    }

    /// Remove a memory's vector. A no-op when no row exists, matching the
    /// LanceDB `delete` this replaces.
    pub async fn delete(&self, memory_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?")
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete vector for memory {memory_id}"))?;
        Ok(())
    }

    /// Exact nearest-neighbour scan. Returns `(memory_id, similarity)` sorted by
    /// similarity descending, where `similarity = dot(query, row)` — the cosine
    /// similarity, since fastembed normalizes both sides.
    pub async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        let rows = sqlx::query(
            "SELECT memory_id, dim, embedding FROM memory_vectors WHERE model = ?",
        )
        .bind(EMBEDDING_MODEL_ID)
        .fetch_all(&self.pool)
        .await
        .context("failed to scan memory vectors")?;

        let mut scored: Vec<(String, f32)> = Vec::with_capacity(rows.len());
        for row in rows {
            let memory_id: String = row.try_get("memory_id").context("vector row has no id")?;
            let dim: i64 = row.try_get("dim").context("vector row has no dim")?;
            let blob: Vec<u8> = row.try_get("embedding").context("vector row has no blob")?;

            let Some(embedding) = decode_vector(&blob, dim as usize) else {
                tracing::debug!(%memory_id, dim, "skipping vector with an undecodable blob");
                continue;
            };
            if embedding.len() != query_embedding.len() {
                continue;
            }

            scored.push((memory_id, dot_product(query_embedding, &embedding)));
        }

        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Memories similar to the memory identified by `memory_id`, excluding
    /// itself, at or above `threshold`. The LanceDB version returned
    /// `(id, similarity)`; this keeps that shape.
    pub async fn find_similar(
        &self,
        memory_id: &str,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT embedding FROM memory_vectors WHERE memory_id = ?")
                .bind(memory_id)
                .fetch_optional(&self.pool)
                .await
                .context("failed to load source vector for similarity search")?;

        let Some(blob) = blob else {
            return Ok(Vec::new());
        };
        let Some(embedding) = decode_vector(&blob, blob.len() / 4) else {
            return Ok(Vec::new());
        };

        // Fetch one extra so the source row can be dropped without shortening
        // the result the caller asked for.
        let mut similar: Vec<(String, f32)> = self
            .vector_search(&embedding, limit + 1)
            .await?
            .into_iter()
            .filter(|(id, similarity)| id != memory_id && *similarity >= threshold)
            .collect();
        similar.truncate(limit);
        Ok(similar)
    }
}

// ---------------------------------------------------------------------------
// Vector storage (replaces the LanceDB `chronicle_embeddings` table)
// ---------------------------------------------------------------------------

/// A labeled hit from the chronicle vector table.
#[derive(Debug, Clone)]
pub struct ChronicleHit {
    /// Checkpoint id.
    pub checkpoint_id: String,
    /// Checkpoint title.
    pub title: String,
    /// Channel the checkpoint belongs to.
    pub channel_id: String,
    /// Checkpoint sequence within the channel.
    pub seq: i64,
    /// Cosine similarity (dot product of normalized vectors).
    pub similarity: f32,
}

/// SQLite-backed replacement for LanceDB's `ChronicleEmbeddingTable`. The
/// title/channel/seq the LanceDB table carried are read back from
/// `channel_chronicle_checkpoints` at query time, so only the vector is new
/// storage.
#[derive(Debug, Clone)]
pub struct ChronicleVectorTable {
    pool: SqlitePool,
}

impl ChronicleVectorTable {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert or replace the vector for a checkpoint.
    pub async fn store(&self, checkpoint_id: &str, embedding: &[f32]) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO chronicle_vectors (checkpoint_id, model, dim, embedding)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(checkpoint_id) DO UPDATE SET
                model = excluded.model,
                dim = excluded.dim,
                embedding = excluded.embedding,
                created_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(checkpoint_id)
        .bind(EMBEDDING_MODEL_ID)
        .bind(embedding.len() as i64)
        .bind(encode_vector(embedding))
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to store vector for checkpoint {checkpoint_id}"))?;
        Ok(())
    }

    /// Remove a checkpoint's vector.
    pub async fn delete(&self, checkpoint_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM chronicle_vectors WHERE checkpoint_id = ?")
            .bind(checkpoint_id)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete vector for checkpoint {checkpoint_id}"))?;
        Ok(())
    }

    /// Whether a checkpoint already has a vector (backfill check).
    pub async fn has(&self, checkpoint_id: &str) -> Result<bool> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM chronicle_vectors WHERE checkpoint_id = ?)",
        )
        .bind(checkpoint_id)
        .fetch_one(&self.pool)
        .await
        .context("failed to check for a checkpoint vector")?;
        Ok(exists != 0)
    }

    /// Exact nearest-neighbour scan joined to the checkpoint rows for labeling.
    /// Returns hits sorted by similarity descending.
    pub async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<ChronicleHit>> {
        let rows = sqlx::query(
            r#"
            SELECT v.checkpoint_id AS checkpoint_id,
                   v.dim AS dim,
                   v.embedding AS embedding,
                   c.title AS title,
                   c.channel_id AS channel_id,
                   c.seq AS seq
              FROM chronicle_vectors v
              JOIN channel_chronicle_checkpoints c ON c.id = v.checkpoint_id
             WHERE v.model = ?
            "#,
        )
        .bind(EMBEDDING_MODEL_ID)
        .fetch_all(&self.pool)
        .await
        .context("failed to scan chronicle vectors")?;

        let mut hits: Vec<ChronicleHit> = Vec::with_capacity(rows.len());
        for row in rows {
            let dim: i64 = row.try_get("dim").context("chronicle vector row has no dim")?;
            let blob: Vec<u8> = row
                .try_get("embedding")
                .context("chronicle vector row has no blob")?;
            let Some(embedding) = decode_vector(&blob, dim as usize) else {
                continue;
            };
            if embedding.len() != query_embedding.len() {
                continue;
            }

            hits.push(ChronicleHit {
                checkpoint_id: row
                    .try_get("checkpoint_id")
                    .context("chronicle vector row has no id")?,
                title: row.try_get("title").context("checkpoint row has no title")?,
                channel_id: row
                    .try_get("channel_id")
                    .context("checkpoint row has no channel_id")?,
                seq: row.try_get("seq").context("checkpoint row has no seq")?,
                similarity: dot_product(query_embedding, &embedding),
            });
        }

        hits.sort_by(|a, b| b.similarity.total_cmp(&a.similarity));
        hits.truncate(limit);
        Ok(hits)
    }
}

// ---------------------------------------------------------------------------
// Full-text search (replaces the LanceDB tantivy inverted index)
// ---------------------------------------------------------------------------

/// FTS5 query over `memories_fts`. Returns `(memory_id, score)` sorted by
/// relevance descending. A query with no usable terms returns no rows rather
/// than an error — an empty `MATCH` string is an FTS5 syntax error.
pub async fn memory_text_search(
    pool: &SqlitePool,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, f32)>> {
    let Some(match_query) = analyzer::build_match_query(query) else {
        return Ok(Vec::new());
    };

    let rows = sqlx::query(
        r#"
        SELECT m.id AS id, bm25(memories_fts) AS rank
          FROM memories_fts
          JOIN memories m ON m.rowid = memories_fts.rowid
         WHERE memories_fts MATCH ?
         ORDER BY rank
         LIMIT ?
        "#,
    )
    .bind(&match_query)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .context("memory FTS query failed")?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id: String = row.try_get("id").ok()?;
            // bm25() is smaller-is-better (it returns negative numbers); negate
            // so callers see a higher-is-better score like the LanceDB `_score`.
            let rank: f64 = row.try_get("rank").unwrap_or(0.0);
            Some((id, (-rank) as f32))
        })
        .collect())
}

/// FTS5 query over `chronicle_fts`. Returns `(checkpoint_id, score)` sorted by
/// relevance descending.
pub async fn chronicle_text_search(
    pool: &SqlitePool,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, f32)>> {
    let Some(match_query) = analyzer::build_match_query(query) else {
        return Ok(Vec::new());
    };

    let rows = sqlx::query(
        r#"
        SELECT c.id AS id, bm25(chronicle_fts) AS rank
          FROM chronicle_fts
          JOIN channel_chronicle_checkpoints c ON c.rowid = chronicle_fts.rowid
         WHERE chronicle_fts MATCH ?
         ORDER BY rank
         LIMIT ?
        "#,
    )
    .bind(&match_query)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .context("chronicle FTS query failed")?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id: String = row.try_get("id").ok()?;
            let rank: f64 = row.try_get("rank").unwrap_or(0.0);
            Some((id, (-rank) as f32))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Backfills
// ---------------------------------------------------------------------------

/// Rewrite `memories.search_text` for every row whose stored value is not the
/// analysed one. Returns how many rows were rewritten.
///
/// Runs automatically on boot (see `main.rs` and `api/agents.rs`, alongside
/// `backfill_chronicle_embeddings`) and is idempotent: a row is only written when
/// the analysed value actually differs from what is stored, so a second run
/// rewrites nothing and reports zero.
///
/// Writing the column is what reindexes the row — `memories_fts_update` fires on
/// `UPDATE OF search_text` and rebuilds that row's index entry inside the same
/// transaction. Nothing here touches the FTS table directly, which is the point:
/// with an external-content index the content table is the only source of truth,
/// so the two cannot disagree.
pub async fn backfill_memory_search_text(pool: &SqlitePool) -> Result<usize> {
    let mut updated = 0usize;
    let mut skipped_unindexable = 0usize;
    let mut cursor: Option<String> = None;

    loop {
        // Candidates are exactly the rows the SQL backfill left raw (and any row
        // inserted while `search_text` still defaulted to empty). Keyset
        // pagination on the primary key keeps this linear and terminating even
        // though rows the loop declines to touch stay candidates.
        let rows = match &cursor {
            Some(after) => sqlx::query(
                r#"
                SELECT id, content, search_text FROM memories
                 WHERE (search_text = '' OR search_text = content)
                   AND id > ?
                 ORDER BY id
                 LIMIT ?
                "#,
            )
            .bind(after)
            .bind(BACKFILL_BATCH)
            .fetch_all(pool)
            .await
            .context("failed to list memories pending a search_text backfill")?,
            None => sqlx::query(
                r#"
                SELECT id, content, search_text FROM memories
                 WHERE search_text = '' OR search_text = content
                 ORDER BY id
                 LIMIT ?
                "#,
            )
            .bind(BACKFILL_BATCH)
            .fetch_all(pool)
            .await
            .context("failed to list memories pending a search_text backfill")?,
        };

        if rows.is_empty() {
            break;
        }

        let batch_len = rows.len();
        for row in rows {
            let id: String = row.try_get("id").context("memory row has no id")?;
            let content: String = row.try_get("content").context("memory row has no content")?;
            let stored: String = row
                .try_get("search_text")
                .context("memory row has no search_text")?;

            let analyzed = analyzer::search_text(&content);

            if analyzed.is_empty() {
                // Content with no tokens at all. There is nothing to index and
                // nothing to stem, and writing `''` here would ask the FTS
                // delete path to remove a document the index never held. Leave
                // the raw copy exactly as the migration left it.
                skipped_unindexable += 1;
                cursor = Some(id);
                continue;
            }

            if analyzed == stored {
                cursor = Some(id);
                continue;
            }

            sqlx::query(
                r#"
                UPDATE memories SET search_text = ?
                 WHERE id = ? AND (search_text = '' OR search_text = content)
                "#,
            )
            .bind(&analyzed)
            .bind(&id)
            .execute(pool)
            .await
            .with_context(|| format!("failed to backfill search_text for memory {id}"))?;

            updated += 1;
            cursor = Some(id);
        }

        if batch_len < BACKFILL_BATCH as usize {
            break;
        }
    }

    if skipped_unindexable > 0 {
        tracing::debug!(
            skipped_unindexable,
            "memories with no tokenizable content left unindexed"
        );
    }

    Ok(updated)
}

/// Rewrite `channel_chronicle_checkpoints.search_text` for every checkpoint
/// whose stored value is not the analysed one. Returns how many were rewritten.
///
/// Mirrors [`backfill_memory_search_text`]; the indexed text is the checkpoint's
/// title and summary, which is the same text the LanceDB `text` column was
/// derived from (`MemorySearch::embed_chronicle_checkpoint`).
pub async fn backfill_chronicle_search_text(pool: &SqlitePool) -> Result<usize> {
    let mut updated = 0usize;
    let mut skipped_unindexable = 0usize;
    let mut cursor: Option<String> = None;

    loop {
        let rows = match &cursor {
            Some(after) => sqlx::query(
                r#"
                SELECT id, title, summary, search_text
                  FROM channel_chronicle_checkpoints
                 WHERE (search_text = '' OR search_text = title || ' ' || summary)
                   AND id > ?
                 ORDER BY id
                 LIMIT ?
                "#,
            )
            .bind(after)
            .bind(BACKFILL_BATCH)
            .fetch_all(pool)
            .await
            .context("failed to list checkpoints pending a search_text backfill")?,
            None => sqlx::query(
                r#"
                SELECT id, title, summary, search_text
                  FROM channel_chronicle_checkpoints
                 WHERE search_text = '' OR search_text = title || ' ' || summary
                 ORDER BY id
                 LIMIT ?
                "#,
            )
            .bind(BACKFILL_BATCH)
            .fetch_all(pool)
            .await
            .context("failed to list checkpoints pending a search_text backfill")?,
        };

        if rows.is_empty() {
            break;
        }

        let batch_len = rows.len();
        for row in rows {
            let id: String = row.try_get("id").context("checkpoint row has no id")?;
            let title: String = row.try_get("title").context("checkpoint row has no title")?;
            let summary: String = row
                .try_get("summary")
                .context("checkpoint row has no summary")?;
            let stored: String = row
                .try_get("search_text")
                .context("checkpoint row has no search_text")?;

            let analyzed = chronicle_search_text(&title, &summary);

            if analyzed.is_empty() {
                skipped_unindexable += 1;
                cursor = Some(id);
                continue;
            }

            if analyzed == stored {
                cursor = Some(id);
                continue;
            }

            sqlx::query(
                r#"
                UPDATE channel_chronicle_checkpoints SET search_text = ?
                 WHERE id = ? AND (search_text = '' OR search_text = title || ' ' || summary)
                "#,
            )
            .bind(&analyzed)
            .bind(&id)
            .execute(pool)
            .await
            .with_context(|| format!("failed to backfill search_text for checkpoint {id}"))?;

            updated += 1;
            cursor = Some(id);
        }

        if batch_len < BACKFILL_BATCH as usize {
            break;
        }
    }

    if skipped_unindexable > 0 {
        tracing::debug!(
            skipped_unindexable,
            "chronicle checkpoints with no tokenizable content left unindexed"
        );
    }

    Ok(updated)
}

/// Run both `search_text` backfills, logging the outcome. The single entry point
/// the boot path calls.
pub async fn backfill_search_text(pool: &SqlitePool) -> Result<usize> {
    let memories = backfill_memory_search_text(pool).await?;
    let checkpoints = backfill_chronicle_search_text(pool).await?;

    if memories > 0 || checkpoints > 0 {
        tracing::info!(
            memories,
            checkpoints,
            "memory search_text backfill complete"
        );
    }

    Ok(memories + checkpoints)
}

/// Ids of non-forgotten memories that have no vector for `model`.
///
/// The vector backfill's work list. Filtering on `model` (not just row
/// presence) means a memory embedded under an older model is re-embedded rather
/// than left behind a `model = ?`-filtered vector scan, which is the whole point
/// of recording the label. Returned oldest-first so a long backfill makes
/// forward progress deterministically.
pub async fn pending_memory_embeddings(pool: &SqlitePool, model: &str) -> Result<Vec<String>> {
    let ids = sqlx::query_scalar(
        r#"
        SELECT m.id
          FROM memories m
          LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.model = ?
         WHERE m.forgotten = 0 AND v.memory_id IS NULL
         ORDER BY m.created_at ASC, m.id ASC
        "#,
    )
    .bind(model)
    .fetch_all(pool)
    .await
    .context("failed to list memories pending an embedding backfill")?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryStore, MemoryType};

    async fn pending_memories(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM memories WHERE search_text = '' OR search_text = content ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .expect("list pending memories")
    }

    async fn search_text_of(pool: &SqlitePool, id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT search_text FROM memories WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("load search_text")
    }

    /// A memory row whose `search_text` is the raw content, as the migration's
    /// SQL backfill leaves it.
    async fn save_legacy_memory(store: &MemoryStore, pool: &SqlitePool, content: &str) -> Memory {
        let memory = Memory::new(content, MemoryType::Fact);
        store.save(&memory).await.expect("save memory");
        sqlx::query("UPDATE memories SET search_text = content WHERE id = ?")
            .bind(&memory.id)
            .execute(pool)
            .await
            .expect("write raw search_text");
        memory
    }

    #[tokio::test]
    async fn backfill_rewrites_legacy_rows_and_is_idempotent() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = save_legacy_memory(&store, &pool, "the memories are cached").await;
        assert_eq!(search_text_of(&pool, &memory.id).await, "the memories are cached");

        let updated = backfill_memory_search_text(&pool).await.expect("backfill");
        assert_eq!(updated, 1, "the legacy row should be rewritten once");

        let rewritten = search_text_of(&pool, &memory.id).await;
        // Both halves: the stemmed tokens (stem sharing) and the raw text
        // (identifier/whole-word matching).
        assert!(rewritten.contains("memori"), "missing stem: {rewritten}");
        assert!(
            rewritten.contains("the memories are cached"),
            "missing raw copy: {rewritten}"
        );

        // Second run must do nothing.
        let second = backfill_memory_search_text(&pool).await.expect("backfill");
        assert_eq!(second, 0, "backfill is not idempotent");
        assert_eq!(search_text_of(&pool, &memory.id).await, rewritten);
    }

    #[tokio::test]
    async fn backfill_leaves_already_analysed_rows_untouched() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = Memory::new("an Ordinary sentence", MemoryType::Fact);
        store.save(&memory).await.expect("save memory");

        // Pre-analyse it by hand, exactly as the write path would.
        let analyzed = analyzer::search_text(&memory.content);
        sqlx::query("UPDATE memories SET search_text = ? WHERE id = ?")
            .bind(&analyzed)
            .bind(&memory.id)
            .execute(&pool)
            .await
            .expect("write analysed search_text");

        let updated = backfill_memory_search_text(&pool).await.expect("backfill");
        assert_eq!(updated, 0, "an analysed row must not be rewritten");
        assert_eq!(search_text_of(&pool, &memory.id).await, analyzed);
    }

    #[tokio::test]
    async fn backfill_fills_rows_that_have_no_search_text_at_all() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = Memory::new("cache invalidation strategies", MemoryType::Fact);
        store.save(&memory).await.expect("save memory");
        // Simulate a row inserted before the write path learned to analyse.
        sqlx::query("UPDATE memories SET search_text = '' WHERE id = ?")
            .bind(&memory.id)
            .execute(&pool)
            .await
            .expect("clear search_text");
        assert_eq!(search_text_of(&pool, &memory.id).await, "");

        let updated = backfill_memory_search_text(&pool).await.expect("backfill");
        assert_eq!(updated, 1);

        let written = search_text_of(&pool, &memory.id).await;
        assert!(written.contains("cach"), "missing stem: {written}");
        assert!(
            written.contains("cache invalidation strategies"),
            "missing raw copy: {written}"
        );
    }

    #[tokio::test]
    async fn backfill_makes_a_legacy_row_findable_by_its_stemmed_term() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = save_legacy_memory(&store, &pool, "the memory of a caching layer").await;

        // Before the backfill the raw copy is indexed, so the whole word matches
        // but the stem-shared form does not.
        let before = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"memori\"'",
        )
        .fetch_one(&pool)
        .await
        .expect("fts count");
        assert_eq!(before, 0, "stemmed term should not match before backfill");

        backfill_memory_search_text(&pool).await.expect("backfill");

        let after = sqlx::query_scalar::<_, String>(
            "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH '\"memori\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(after, vec![memory.id.clone()], "stemmed term should match");

        // ...and the raw form still matches, because the raw copy is kept.
        let raw = sqlx::query_scalar::<_, String>(
            "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH '\"memory\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(raw, vec![memory.id]);
    }

    #[tokio::test]
    async fn backfill_keeps_identifier_tokens_whole_and_searchable() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = save_legacy_memory(&store, &pool, "glm-5.3-flash and context_window").await;
        backfill_memory_search_text(&pool).await.expect("backfill");

        let by_identifier = sqlx::query_scalar::<_, String>(
            "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH '\"context_window\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(by_identifier, vec![memory.id.clone()]);

        // The split-and-stemmed half still matches a fragment, which is the
        // recall the current tantivy index provides.
        let by_fragment = sqlx::query_scalar::<_, String>(
            "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH '\"flash\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(by_fragment, vec![memory.id]);
    }

    #[tokio::test]
    async fn backfill_skips_content_with_no_tokens_without_breaking_the_index() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        // A row the migration's SQL backfill would have set to a non-empty but
        // untokenizable value. The backfill must not try to "fix" it into an
        // empty string, which would ask FTS5 to delete a document it never held.
        let memory = Memory::new("!!! --- !!!", MemoryType::Fact);
        store.save(&memory).await.expect("save memory");
        sqlx::query("UPDATE memories SET search_text = content WHERE id = ?")
            .bind(&memory.id)
            .execute(&pool)
            .await
            .expect("write raw search_text");

        let updated = backfill_memory_search_text(&pool).await.expect("backfill");
        assert_eq!(updated, 0, "nothing to index for this row");
        assert_eq!(search_text_of(&pool, &memory.id).await, "!!! --- !!!");

        // The index is still usable — proving the loop did not corrupt it.
        let memory2 = save_legacy_memory(&store, &pool, "a searchable sentence").await;
        backfill_memory_search_text(&pool).await.expect("backfill");
        let found = sqlx::query_scalar::<_, String>(
            "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH '\"sentenc\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(found, vec![memory2.id]);

        // And the FTS index still agrees with its content table.
        sqlx::query("INSERT INTO memories_fts(memories_fts) VALUES('integrity-check')")
            .execute(&pool)
            .await
            .expect("fts integrity-check failed");
    }

    #[tokio::test]
    async fn chronicle_backfill_rewrites_legacy_checkpoints_and_is_idempotent() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        sqlx::query(
            r#"
            INSERT INTO channel_chronicle_checkpoints
                (id, channel_id, seq, level, kind, title, summary,
                 covers_from_at, covers_to_at, message_count, token_estimate)
            VALUES ('cp-1', 'chan-1', 1, 0, 'interval', 'Caching layers',
                    'discussed the memories of a cache', '2026-01-01', '2026-01-02', 2, 20)
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert checkpoint");

        let updated = backfill_chronicle_search_text(&pool).await.expect("backfill");
        assert_eq!(updated, 1);

        let written = sqlx::query_scalar::<_, String>(
            "SELECT search_text FROM channel_chronicle_checkpoints WHERE id = 'cp-1'",
        )
        .fetch_one(&pool)
        .await
        .expect("load search_text");
        assert!(written.contains("memori"), "missing stem: {written}");
        assert!(written.contains("Caching layers"), "missing raw: {written}");

        assert_eq!(
            backfill_chronicle_search_text(&pool).await.expect("backfill"),
            0,
            "chronicle backfill is not idempotent"
        );

        let found = sqlx::query_scalar::<_, String>(
            "SELECT c.id FROM chronicle_fts JOIN channel_chronicle_checkpoints c
                ON c.rowid = chronicle_fts.rowid
              WHERE chronicle_fts MATCH '\"memori\"'",
        )
        .fetch_all(&pool)
        .await
        .expect("fts query");
        assert_eq!(found, vec!["cp-1".to_string()]);
    }

    #[tokio::test]
    async fn backfill_reports_zero_when_nothing_is_pending() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        assert_eq!(backfill_search_text(&pool).await.expect("backfill"), 0);
        assert!(pending_memories(&pool).await.is_empty());
    }

    #[test]
    fn vector_blob_round_trips_and_rejects_wrong_lengths() {
        let embedding = vec![0.5_f32, -1.25, 3.0];
        let blob = encode_vector(&embedding);
        assert_eq!(blob.len(), embedding.len() * 4);
        assert_eq!(decode_vector(&blob, 3), Some(embedding));
        assert_eq!(decode_vector(&blob, 4), None, "dim mismatch must not decode");
        assert_eq!(decode_vector(&blob[..blob.len() - 1], 3), None);
    }

    #[tokio::test]
    async fn vector_search_ranks_by_cosine_and_find_similar_excludes_self() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();
        let table = MemoryVectorTable::new(pool.clone());

        async fn save(store: &MemoryStore, content: &str) -> Memory {
            let memory = Memory::new(content, MemoryType::Fact);
            store.save(&memory).await.expect("save memory");
            memory
        }

        let near = save(&store, "nearly parallel").await;
        let far = save(&store, "orthogonal").await;

        // Unit vectors (3-4-5 rotations): query [1,0,0] is closer to near
        // [0.8,0.6,0] (cos 0.8) than to far [0.6,0.8,0] (cos 0.6), and near/far
        // still share cosine 0.96 — above the find_similar threshold.
        let near_vec = {
            let mut v = vec![0.0_f32; 384];
            v[0] = 0.8;
            v[1] = 0.6;
            v
        };
        let far_vec = {
            let mut v = vec![0.0_f32; 384];
            v[0] = 0.6;
            v[1] = 0.8;
            v
        };
        table.store(&near.id, &near_vec).await.expect("store near");
        table.store(&far.id, &far_vec).await.expect("store far");

        let query = {
            let mut v = vec![0.0_f32; 384];
            v[0] = 1.0;
            v
        };

        let hits = table.vector_search(&query, 10).await.expect("vector search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, near.id, "the closer vector must rank first");
        assert!(hits[0].1 > hits[1].1);

        let similar = table
            .find_similar(&near.id, 0.5, 10)
            .await
            .expect("find similar");
        assert_eq!(
            similar.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            vec![far.id.clone()],
            "find_similar must exclude the source and drop sub-threshold rows"
        );
    }

    #[tokio::test]
    async fn text_search_finds_stem_shared_and_identifier_terms() {
        let store = MemoryStore::connect_in_memory().await;
        let pool = store.pool().clone();

        let memory = Memory::new("the memory of a context_window", MemoryType::Fact);
        store.save(&memory).await.expect("save memory");

        // A stem-sharing query resolves to the stored row without any backfill,
        // because the write path already analysed `search_text`.
        let by_stem = memory_text_search(&pool, "memories", 10)
            .await
            .expect("text search");
        assert_eq!(by_stem.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), vec![memory.id.clone()]);

        let by_identifier = memory_text_search(&pool, "context_window", 10)
            .await
            .expect("text search");
        assert_eq!(by_identifier.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), vec![memory.id.clone()]);

        // No usable terms is "no results", not an error.
        assert!(memory_text_search(&pool, "the of", 10).await.expect("text search").is_empty());
    }
}
