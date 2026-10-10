//! SQLite-side storage for memory and chronicle search (stage 1 of the
//! LanceDB → SQLite migration).
//!
//! Stage 1A added the schema (`migrations/20260816000001_memory_search_sqlite.sql`)
//! and the pure analyzer (`src/memory/analyzer.rs`) without wiring either into
//! the live path. This module is the operational half: the backfills that bring
//! already-stored rows up to the analysed representation, and — from stage 1
//! step B onward — the `EmbeddingTable` / `ChronicleEmbeddingTable` replaced by
//! their SQLite-backed equivalents.
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

use crate::error::Result;
use crate::memory::analyzer;

use anyhow::Context as _;
use sqlx::{Row, SqlitePool};

/// How many rows one backfill statement examines. Bounds peak memory on a large
/// store; the loop it feeds is keyset-paginated, so this affects only batch size.
const BACKFILL_BATCH: i64 = 500;

/// Split a checkpoint's indexed text the way the write path does.
fn chronicle_source_text(title: &str, summary: &str) -> String {
    format!("{title} {summary}")
}

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

            let analyzed = analyzer::search_text(&chronicle_source_text(&title, &summary));

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
}