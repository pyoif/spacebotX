//! Write-time consolidation: advisory near-duplicate detection and
//! per-partition debt, executed atomically by the branch.
//!
//! The save always lands — consolidation is advisory and never blocks a
//! write. After a memory is persisted, `check_save` reports two signals on
//! the output the branch sees:
//!
//! - **near-duplicates** — existing memories closely matching the new one
//!   (vector similarity above `consolidation_near_duplicate_threshold`)
//! - **per-partition over-cap debt** — the partition (memory type) has grown
//!   past `consolidation_partition_cap`, so the render's per-type budgets are
//!   showing a thinning slice of a bloated store
//!
//! The branch responds with one atomic `memory_consolidate` batch per
//! partition (`execute_batch`): every merge in the batch is applied in a
//! single SQLite transaction, so a failing op leaves the store untouched.
//! Absorbed memories are marked forgotten rather than deleted — the
//! forgotten row is the provenance target of the survivor's Updates edge.
//! Embedding updates happen after commit and are warn-only, consistent with
//! save-never-fails. Batches are serialized per partition — one consolidator
//! at a time.

use crate::memory::{EmbeddingModel, MemoryStore, MemoryType, MemoryVectorTable};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Consecutive over-cap advisories a partition may receive without a
/// successful consolidation batch before further advisories are withheld.
const MAX_ADVICE_STRIKES: u32 = 3;

/// Shared per-agent consolidation state: per-partition debt counters and
/// per-partition serialization locks. Lives inside `MemorySearch` so every
/// tool clone shares one instance. Process-local — persistence and UI
/// surfacing are a later phase.
#[derive(Debug, Default)]
pub struct ConsolidationState {
    /// Unresolved consolidation batches per partition (memory type label).
    debt: Mutex<HashMap<String, u64>>,
    /// Last cap observed per partition, recorded at save so a batch can
    /// recompute over-cap state without re-plumbing config.
    partition_caps: Mutex<HashMap<String, usize>>,
    /// Consecutive over-cap advisories issued per partition without a
    /// successful batch. At `MAX_ADVICE_STRIKES` the advisory is withheld
    /// until a batch succeeds and resets the counter.
    advice_strikes: Mutex<HashMap<String, u32>>,
    /// One lock per partition; held for the duration of a consolidation
    /// batch so concurrent batches never interleave.
    partition_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ConsolidationState {
    /// Record one more over-cap save for a partition. Returns the new debt.
    pub fn record_debt(&self, partition: &str) -> u64 {
        let mut debt = self.debt.lock().expect("consolidation debt poisoned");
        let count = debt.entry(partition.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// Current unresolved debt for a partition.
    pub fn debt(&self, partition: &str) -> u64 {
        self.debt
            .lock()
            .expect("consolidation debt poisoned")
            .get(partition)
            .copied()
            .unwrap_or(0)
    }

    /// Settle a partition's debt after a successful consolidation batch,
    /// from a recomputation of its over-cap state: a partition still over
    /// cap keeps one unit of debt (residual work), an under-cap partition
    /// clears. Also resets the advisory strike counter so over-cap advice
    /// resumes.
    pub fn settle_debt(&self, partition: &str, still_over_cap: bool) {
        {
            let mut debt = self.debt.lock().expect("consolidation debt poisoned");
            if still_over_cap {
                debt.insert(partition.to_string(), 1);
            } else {
                debt.remove(partition);
            }
        }
        self.advice_strikes
            .lock()
            .expect("consolidation strikes poisoned")
            .remove(partition);
    }

    /// Record the configured cap for a partition, observed at save time.
    fn record_partition_cap(&self, partition: &str, cap: usize) {
        self.partition_caps
            .lock()
            .expect("consolidation caps poisoned")
            .insert(partition.to_string(), cap);
    }

    /// Last cap observed for a partition, if any save has reported one.
    fn partition_cap(&self, partition: &str) -> Option<usize> {
        self.partition_caps
            .lock()
            .expect("consolidation caps poisoned")
            .get(partition)
            .copied()
    }

    /// Record that an over-cap advisory was attached to a save output.
    fn record_advice(&self, partition: &str) {
        let mut strikes = self
            .advice_strikes
            .lock()
            .expect("consolidation strikes poisoned");
        *strikes.entry(partition.to_string()).or_insert(0) += 1;
    }

    /// True when the partition has exhausted its consecutive over-cap
    /// advisories and further ones are withheld until a batch succeeds.
    fn advice_exhausted(&self, partition: &str) -> bool {
        self.advice_strikes
            .lock()
            .expect("consolidation strikes poisoned")
            .get(partition)
            .copied()
            .unwrap_or(0)
            >= MAX_ADVICE_STRIKES
    }

    /// The per-partition serialization lock (created on first use).
    fn partition_lock(&self, partition: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .partition_locks
            .lock()
            .expect("consolidation locks poisoned");
        locks
            .entry(partition.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// A closely matching existing memory, surfaced on a save.
#[derive(Debug, Clone, Serialize)]
pub struct NearDuplicate {
    pub memory_id: String,
    /// Similarity (1.0 - cosine distance) against the newly saved memory.
    pub similarity: f32,
    /// First non-empty line of the existing memory's content.
    pub content: String,
}

/// Advisory returned from a save, rendered on the tool output for the branch.
#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationAdvice {
    /// Partition (memory type label) the advice applies to.
    pub partition: String,
    /// Total memories in the partition after this save.
    pub partition_count: i64,
    /// Per-partition cap from config.
    pub partition_cap: usize,
    /// True when the partition is over its cap and a consolidation batch is
    /// being requested. Withheld (false) once the partition has exhausted
    /// its consecutive advisories without a successful batch.
    pub over_cap: bool,
    /// Unresolved consolidation batches queued for this partition.
    pub debt: u64,
    /// Closely matching existing memories.
    pub near_duplicates: Vec<NearDuplicate>,
}

/// One merge operation of a consolidation batch: absorb `absorbed_id` into
/// `survivor_id`, replacing the survivor's content with the branch's composed
/// `merged_content`.
#[derive(Debug, Clone)]
pub struct ConsolidateOp {
    pub survivor_id: String,
    pub absorbed_id: String,
    pub merged_content: String,
}

/// Outcome of a consolidation batch.
#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationReport {
    pub partition: String,
    /// Merges applied in the batch.
    pub consolidated: usize,
    /// Memories remaining in the partition after the batch.
    pub remaining_count: i64,
}

/// Run the post-save consolidation check. Advisory: returns `None` when
/// nothing is actionable (no near-duplicates, partition under cap or with
/// its advisories exhausted).
pub async fn check_save(
    state: &ConsolidationState,
    store: &MemoryStore,
    embedding_table: &MemoryVectorTable,
    memory_id: &str,
    memory_type: MemoryType,
    cap: usize,
    threshold: f32,
) -> Result<Option<ConsolidationAdvice>> {
    let partition = memory_type.to_string();
    let partition_count = store.count_by_type(memory_type).await?;
    state.record_partition_cap(&partition, cap);
    let over_cap = partition_count > cap as i64;
    let debt = if over_cap {
        state.record_debt(&partition)
    } else {
        state.debt(&partition)
    };

    let mut near_duplicates = Vec::new();
    for (candidate_id, similarity) in embedding_table
        .find_similar(memory_id, threshold, 5)
        .await?
    {
        let preview = match store.load(&candidate_id).await {
            Ok(Some(memory)) => first_line(&memory.content),
            _ => String::new(),
        };
        near_duplicates.push(NearDuplicate {
            memory_id: candidate_id,
            similarity,
            content: preview,
        });
    }

    // Bounded advisories: a partition that keeps accruing over-cap saves
    // without a successful batch stops receiving the over-cap request until
    // one succeeds, instead of re-asking every consolidator forever.
    let advise_over_cap = over_cap && !state.advice_exhausted(&partition);
    if advise_over_cap {
        state.record_advice(&partition);
    }

    if !advise_over_cap && near_duplicates.is_empty() {
        return Ok(None);
    }

    Ok(Some(ConsolidationAdvice {
        partition,
        partition_count,
        partition_cap: cap,
        over_cap: advise_over_cap,
        debt,
        near_duplicates,
    }))
}

/// A merge op validated against the store, ready to apply.
struct PreparedOp {
    survivor_id: String,
    absorbed_id: String,
    merged_content: String,
    /// Chronicle checkpoint covering the absorbed memory's origin span, for
    /// supersede provenance (1.7).
    provenance_checkpoint_id: Option<String>,
}

/// Execute a consolidation batch for one partition. Serialized per partition:
/// the partition lock is held for the whole batch. All merges are applied in
/// one SQLite transaction — a failing op rolls the entire batch back.
/// Embedding updates run after commit and are warn-only.
pub async fn execute_batch(
    state: &ConsolidationState,
    store: &MemoryStore,
    embedding_table: &MemoryVectorTable,
    embedding_model: &Arc<EmbeddingModel>,
    partition: &str,
    batch: &[ConsolidateOp],
) -> Result<ConsolidationReport> {
    let memory_type = MemoryType::from_label(partition)
        .with_context(|| format!("unknown consolidation partition {partition:?}"))?;

    let partition_lock = state.partition_lock(partition);
    let _partition_guard = partition_lock.lock().await;

    // Validate every op against the store before touching anything, so a
    // malformed batch fails without opening a transaction.
    let mut prepared = Vec::with_capacity(batch.len());
    for op in batch {
        let survivor = store
            .load(&op.survivor_id)
            .await?
            .with_context(|| format!("consolidation survivor {} missing", op.survivor_id))?;
        let absorbed = store
            .load(&op.absorbed_id)
            .await?
            .with_context(|| format!("consolidation target {} missing", op.absorbed_id))?;

        if survivor.memory_type != memory_type {
            anyhow::bail!(
                "consolidation survivor {} is a {} memory, not in partition {partition:?}",
                survivor.id,
                survivor.memory_type,
            );
        }
        if absorbed.memory_type != memory_type {
            anyhow::bail!(
                "consolidation target {} is a {} memory, not in partition {partition:?}",
                absorbed.id,
                absorbed.memory_type,
            );
        }

        let merged_content = op.merged_content.trim().to_string();
        if merged_content.is_empty() {
            anyhow::bail!(
                "consolidation merged_content is empty for {}",
                op.survivor_id
            );
        }

        // Supersede-with-provenance (1.7): record the chronicle checkpoint
        // whose span the absorbed memory came from, so the merged memory
        // carries where the superseded claim originated. Resolved before the
        // batch transaction opens — it only reads the chronicle spine.
        let mut provenance_checkpoint_id = None;
        if let Some(channel_id) = &absorbed.channel_id {
            let chronicle =
                crate::conversation::chronicle::ChronicleStore::new(store.pool().clone());
            if let Ok(Some(checkpoint)) = chronicle
                .covering_checkpoint(channel_id, absorbed.created_at)
                .await
            {
                provenance_checkpoint_id = Some(checkpoint.id);
            }
        }

        prepared.push(PreparedOp {
            survivor_id: op.survivor_id.clone(),
            absorbed_id: op.absorbed_id.clone(),
            merged_content,
            provenance_checkpoint_id,
        });
    }

    // Apply the whole batch in one transaction. Ops are applied in order and
    // re-read through the transaction, so a later op that reuses an earlier
    // survivor sees its merged state.
    let mut transaction = store
        .pool()
        .begin()
        .await
        .context("failed to start consolidation batch transaction")?;
    // Final content per survivor, for post-commit re-embedding. Keyed by id
    // so a survivor reused across ops is embedded once with its final text.
    let mut reembed: HashMap<String, String> = HashMap::new();
    let mut absorbed_ids = Vec::with_capacity(prepared.len());

    for op in &prepared {
        let survivor = super::store::load_in_tx(&mut transaction, &op.survivor_id)
            .await?
            .with_context(|| format!("consolidation survivor {} missing", op.survivor_id))?;
        let absorbed = super::store::load_in_tx(&mut transaction, &op.absorbed_id)
            .await?
            .with_context(|| format!("consolidation target {} missing", op.absorbed_id))?;

        let mut updated_survivor = survivor.clone();
        updated_survivor.content = op.merged_content.clone();
        updated_survivor.importance = survivor.importance.max(absorbed.importance);
        updated_survivor.updated_at = chrono::Utc::now();
        if updated_survivor.supersedes_checkpoint_id.is_none() {
            updated_survivor.supersedes_checkpoint_id = op.provenance_checkpoint_id.clone();
        }

        super::store::merge_memories_in_tx(&mut transaction, &updated_survivor, &absorbed)
            .await
            .with_context(|| {
                format!("failed to merge {} into {}", op.absorbed_id, op.survivor_id)
            })?;

        reembed.insert(op.survivor_id.clone(), op.merged_content.clone());
        absorbed_ids.push(op.absorbed_id.clone());
    }

    transaction
        .commit()
        .await
        .context("failed to commit consolidation batch transaction")?;

    // The merges are durable. Embedding upkeep is warn-only from here: a
    // stale or orphaned embedding row is invisible to recall (joins resolve
    // through SQLite) and self-heals on the next content change. The
    // absorbed rows stay in SQLite as forgotten memories — each one is the
    // provenance target of its survivor's Updates edge — only their
    // embedding rows are removed.
    for (survivor_id, content) in &reembed {
        match embedding_model.embed_one(content).await {
            Ok(embedding) => {
                if let Err(error) = embedding_table.store(survivor_id, &embedding).await {
                    tracing::warn!(
                        %survivor_id,
                        %error,
                        "failed to store re-embedding after consolidation batch"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    %survivor_id,
                    %error,
                    "failed to re-embed survivor after consolidation batch"
                );
            }
        }
    }
    for absorbed_id in &absorbed_ids {
        if let Err(error) = embedding_table.delete(absorbed_id).await {
            tracing::warn!(
                %absorbed_id,
                %error,
                "failed to delete absorbed memory embedding"
            );
        }
    }

    // Settle debt from a recomputation of the partition's over-cap state
    // rather than assuming the batch resolved everything.
    let remaining = store.count_by_type(memory_type).await?;
    let still_over_cap = state
        .partition_cap(partition)
        .is_some_and(|cap| remaining > cap as i64);
    state.settle_debt(partition, still_over_cap);

    Ok(ConsolidationReport {
        partition: partition.to_string(),
        consolidated: batch.len(),
        remaining_count: remaining,
    })
}

/// First non-empty line, capped for previews.
fn first_line(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(140)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, RelationType};

    /// A store-backed vector table for the consolidation tests. Vectors live in
    /// the store's own SQLite database now, so the "table" is just a handle to
    /// the same pool and needs no temp directory of its own.
    fn vector_table(store: &MemoryStore) -> MemoryVectorTable {
        MemoryVectorTable::new(store.pool().clone())
    }

    async fn save_fact(store: &MemoryStore, content: &str) -> Memory {
        let memory = Memory::new(content, MemoryType::Fact);
        store.save(&memory).await.expect("save memory");
        memory
    }

    #[tokio::test]
    async fn failing_op_rolls_back_the_whole_batch() {
        let store = MemoryStore::connect_in_memory().await;
        let embedding_table = vector_table(&store);
        let embedding_model = crate::memory::embedding::shared_test_model();
        let state = ConsolidationState::default();

        let survivor = save_fact(&store, "original survivor content").await;
        let absorbed = save_fact(&store, "duplicate content").await;

        let batch = vec![
            ConsolidateOp {
                survivor_id: survivor.id.clone(),
                absorbed_id: absorbed.id.clone(),
                merged_content: "merged content".to_string(),
            },
            ConsolidateOp {
                survivor_id: survivor.id.clone(),
                absorbed_id: "no-such-memory".to_string(),
                merged_content: "unreachable".to_string(),
            },
        ];

        let result = execute_batch(
            &state,
            &store,
            &embedding_table,
            &embedding_model,
            "fact",
            &batch,
        )
        .await;
        assert!(result.is_err(), "batch with a missing target must fail");

        // The valid first op must not have been applied.
        let survivor_after = store.load(&survivor.id).await.unwrap().unwrap();
        assert_eq!(survivor_after.content, "original survivor content");
        let absorbed_after = store.load(&absorbed.id).await.unwrap().unwrap();
        assert!(!absorbed_after.forgotten);
        assert!(
            store
                .get_associations(&survivor.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_partition_is_rejected() {
        let store = MemoryStore::connect_in_memory().await;
        let embedding_table = vector_table(&store);
        let embedding_model = crate::memory::embedding::shared_test_model();
        let state = ConsolidationState::default();

        let error = execute_batch(
            &state,
            &store,
            &embedding_table,
            &embedding_model,
            "nonsense",
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown consolidation partition")
        );
    }

    #[tokio::test]
    async fn op_outside_the_partition_is_rejected() {
        let store = MemoryStore::connect_in_memory().await;
        let embedding_table = vector_table(&store);
        let embedding_model = crate::memory::embedding::shared_test_model();
        let state = ConsolidationState::default();

        let survivor = save_fact(&store, "a fact").await;
        let stray = Memory::new("a preference", MemoryType::Preference);
        store.save(&stray).await.unwrap();

        let batch = vec![ConsolidateOp {
            survivor_id: survivor.id.clone(),
            absorbed_id: stray.id.clone(),
            merged_content: "merged".to_string(),
        }];

        let error = execute_batch(
            &state,
            &store,
            &embedding_table,
            &embedding_model,
            "fact",
            &batch,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&stray.id));
        assert!(message.contains("preference"));

        // Nothing was applied.
        assert!(!store.load(&stray.id).await.unwrap().unwrap().forgotten);
    }

    #[tokio::test]
    async fn successful_batch_keeps_absorbed_row_as_provenance() {
        let store = MemoryStore::connect_in_memory().await;
        let embedding_table = vector_table(&store);
        let embedding_model = crate::memory::embedding::shared_test_model();
        let state = ConsolidationState::default();

        let survivor = save_fact(&store, "spacebot uses SQLite").await;
        let absorbed = save_fact(&store, "spacebot stores data in SQLite").await;

        let batch = vec![ConsolidateOp {
            survivor_id: survivor.id.clone(),
            absorbed_id: absorbed.id.clone(),
            merged_content: "spacebot persists all state in SQLite".to_string(),
        }];

        let report = execute_batch(
            &state,
            &store,
            &embedding_table,
            &embedding_model,
            "fact",
            &batch,
        )
        .await
        .expect("batch should succeed");
        assert_eq!(report.consolidated, 1);
        assert_eq!(report.remaining_count, 1);

        let survivor_after = store.load(&survivor.id).await.unwrap().unwrap();
        assert_eq!(
            survivor_after.content,
            "spacebot persists all state in SQLite"
        );

        // The absorbed row survives as the forgotten provenance target of
        // the survivor's Updates edge.
        let absorbed_after = store.load(&absorbed.id).await.unwrap().unwrap();
        assert!(absorbed_after.forgotten);
        let associations = store.get_associations(&survivor.id).await.unwrap();
        assert!(associations.iter().any(|association| {
            association.source_id == survivor.id
                && association.target_id == absorbed.id
                && association.relation_type == RelationType::Updates
        }));
    }

    #[test]
    fn settled_debt_reflects_recomputed_over_cap_state() {
        let state = ConsolidationState::default();
        state.record_debt("fact");
        state.record_debt("fact");
        state.record_debt("fact");
        assert_eq!(state.debt("fact"), 3);

        state.settle_debt("fact", true);
        assert_eq!(
            state.debt("fact"),
            1,
            "over-cap partition keeps residual debt"
        );

        state.settle_debt("fact", false);
        assert_eq!(state.debt("fact"), 0);
    }

    #[test]
    fn over_cap_advice_is_bounded_and_reset_by_a_successful_batch() {
        let state = ConsolidationState::default();
        for _ in 0..MAX_ADVICE_STRIKES {
            assert!(!state.advice_exhausted("fact"));
            state.record_advice("fact");
        }
        assert!(state.advice_exhausted("fact"));

        state.settle_debt("fact", false);
        assert!(!state.advice_exhausted("fact"));
    }
}
