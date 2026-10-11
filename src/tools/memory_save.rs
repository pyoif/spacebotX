//! Memory save tool for channels and branches.

use crate::error::Result;
use crate::memory::types::Association;
use crate::memory::{Memory, MemorySearch, MemoryType};
use crate::{AgentId, ProcessEvent};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

/// Maximum allowed memory content length (bytes). Prevents oversized memories
/// from bloating the database and embedding index.
const MAX_MEMORY_CONTENT_BYTES: usize = 50_000;

fn human_anchor_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Tool for saving memories to the store.
#[derive(Debug, Clone)]
pub struct MemorySaveTool {
    memory_search: Arc<MemorySearch>,
    event_context: Option<MemorySaveEventContext>,
    contract_state: Option<Arc<super::memory_persistence_complete::MemoryPersistenceContractState>>,
    working_memory: Option<Arc<crate::memory::WorkingMemoryStore>>,
    runtime_config: Option<Arc<crate::config::RuntimeConfig>>,
    /// Serializes human-anchor creation and merging across tool instances.
    anchor_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone)]
struct MemorySaveEventContext {
    agent_id: AgentId,
    memory_event_tx: tokio::sync::broadcast::Sender<ProcessEvent>,
}

impl MemorySaveTool {
    /// Create a new memory save tool.
    pub fn new(memory_search: Arc<MemorySearch>) -> Self {
        Self {
            memory_search,
            event_context: None,
            contract_state: None,
            working_memory: None,
            runtime_config: None,
            anchor_lock: human_anchor_lock(),
        }
    }

    /// Enable process event emission for successful memory saves.
    pub fn with_event_bus(
        mut self,
        agent_id: AgentId,
        memory_event_tx: tokio::sync::broadcast::Sender<ProcessEvent>,
    ) -> Self {
        self.event_context = Some(MemorySaveEventContext {
            agent_id,
            memory_event_tx,
        });
        self
    }

    pub fn with_contract_state(
        mut self,
        contract_state: Arc<super::memory_persistence_complete::MemoryPersistenceContractState>,
    ) -> Self {
        self.contract_state = Some(contract_state);
        self
    }

    /// Enable working memory event emission for successful memory saves.
    pub fn with_working_memory(mut self, store: Arc<crate::memory::WorkingMemoryStore>) -> Self {
        self.working_memory = Some(store);
        self
    }

    /// Enable the write-time consolidation check (config for cap/threshold).
    pub fn with_runtime_config(mut self, config: Arc<crate::config::RuntimeConfig>) -> Self {
        self.runtime_config = Some(config);
        self
    }

    /// Roll back the SQLite side after an embedding failure so a memory can
    /// never look saved while being unrecallable. A freshly created row is
    /// deleted along with its associations; a merged human anchor is
    /// restored to its pre-merge state instead — deleting it would destroy
    /// the person's accumulated profile.
    async fn compensate_embedding_failure(
        &self,
        memory: &Memory,
        replaced_anchor: Option<&Memory>,
    ) {
        let store = self.memory_search.store();
        if let Some(previous) = replaced_anchor {
            if let Err(error) = store.update(previous).await {
                tracing::error!(
                    memory_id = %memory.id,
                    %error,
                    "failed to restore human anchor after embedding error"
                );
            }
            return;
        }
        if let Err(error) = store.delete_associations_for_memory(&memory.id).await {
            tracing::error!(
                memory_id = %memory.id,
                %error,
                "compensating association delete failed after embedding error"
            );
        }
        if let Err(error) = store.delete(&memory.id).await {
            tracing::error!(
                memory_id = %memory.id,
                %error,
                "compensating delete failed after embedding error"
            );
        }
    }
}

/// Error type for memory save tool.
#[derive(Debug, thiserror::Error)]
#[error("Memory save failed: {0}")]
pub struct MemorySaveError(String);

impl From<crate::error::Error> for MemorySaveError {
    fn from(e: crate::error::Error) -> Self {
        MemorySaveError(format!("{e}"))
    }
}

/// Arguments for memory save tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemorySaveArgs {
    /// The content to save as a memory.
    pub content: String,
    /// The type of memory (fact, preference, decision, identity, event, observation).
    #[serde(default = "default_memory_type")]
    pub memory_type: String,
    /// Optional importance score (0.0-1.0). If not provided, uses type default.
    pub importance: Option<f32>,
    /// Optional source information (e.g., "user", "system", "inferred").
    pub source: Option<String>,
    /// Optional channel ID to associate this memory with the conversation it came from.
    pub channel_id: Option<String>,
    /// Required when `memory_type` is `human`: the exact participant key of
    /// the human this anchor is about. Saves merge into the existing anchor
    /// instead of creating a duplicate (3.1a).
    pub human_id: Option<String>,
    /// Optional associations to create with other memories.
    #[serde(default)]
    pub associations: Vec<AssociationInput>,
}

fn default_memory_type() -> String {
    "fact".to_string()
}

/// Input for creating an association.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssociationInput {
    /// The ID of the target memory to associate with.
    pub target_id: String,
    /// The type of relation (related_to, updates, contradicts, caused_by, result_of, part_of).
    #[serde(default = "default_relation_type")]
    pub relation_type: String,
    /// The weight of the association (0.0-1.0).
    #[serde(default = "default_weight")]
    pub weight: f32,
}

fn default_relation_type() -> String {
    "related_to".to_string()
}

fn default_weight() -> f32 {
    0.5
}

/// Output from memory save tool.
#[derive(Debug, Serialize)]
pub struct MemorySaveOutput {
    /// The ID of the saved memory.
    pub memory_id: String,
    /// Whether the save was successful.
    pub success: bool,
    /// Optional message about the result.
    pub message: String,
    /// Advisory write-time consolidation advice (near-duplicates or an
    /// over-cap partition). Present only when the save flagged something
    /// actionable; the branch decides whether to consolidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<crate::memory::consolidation::ConsolidationAdvice>,
}

impl Tool for MemorySaveTool {
    const NAME: &'static str = "memory_save";

    type Error = MemorySaveError;
    type Args = MemorySaveArgs;
    type Output = MemorySaveOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/memory_save").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The content to save as a memory. Be concise but complete."
                    },
                    "memory_type": {
                        "type": "string",
                        "description": "The type of memory being saved. Each type drives a different behavior — choose the one whose effect matches what you're storing.",
                        "oneOf": memory_type_schema_entries(),
                    },
                    "importance": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Optional importance score from 0.0 to 1.0. Higher values are recalled more easily."
                    },
                    "source": {
                        "type": "string",
                        "description": "Optional source of the information (e.g., 'user stated', 'inferred', 'system')"
                    },
                    "channel_id": {
                        "type": "string",
                        "description": "Optional channel ID to associate this memory with the conversation it came from"
                    },
                    "human_id": {
                        "type": "string",
                        "description": "Required when memory_type is 'human': the exact participant key of the human this anchor is about. Saves merge into the existing anchor."
                    },
                    "associations": {
                        "type": "array",
                        "description": "Optional associations to link this memory to other memories",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target_id": {
                                    "type": "string",
                                    "description": "The ID of the memory to associate with"
                                },
                                "relation_type": {
                                    "type": "string",
                                    "enum": ["related_to", "updates", "contradicts", "caused_by", "result_of", "part_of"],
                                    "description": "The type of relationship"
                                },
                                "weight": {
                                    "type": "number",
                                    "minimum": 0.0,
                                    "maximum": 1.0,
                                    "description": "Strength of the association (0.0-1.0)"
                                }
                            },
                            "required": ["target_id"]
                        }
                    }
                },
                "required": ["content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> std::result::Result<Self::Output, Self::Error> {
        if args.content.is_empty() {
            return Err(MemorySaveError("content must not be empty".into()));
        }

        if args.content.len() > MAX_MEMORY_CONTENT_BYTES {
            return Err(MemorySaveError(format!(
                "content exceeds maximum length of {MAX_MEMORY_CONTENT_BYTES} bytes (got {})",
                args.content.len()
            )));
        }

        if let Some(importance) = args.importance
            && !(0.0..=1.0).contains(&importance)
        {
            return Err(MemorySaveError(format!(
                "importance must be between 0.0 and 1.0 (got {importance})"
            )));
        }

        // Parse memory type; an unrecognized label is a schema violation,
        // not a fact.
        let Some(memory_type) = MemoryType::from_label(&args.memory_type) else {
            return Err(MemorySaveError(format!(
                "unknown memory_type '{}'; valid types: {}",
                args.memory_type,
                MemoryType::ALL
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };

        let mut memory = Memory::new(&args.content, memory_type);

        if let Some(importance) = args.importance {
            memory = memory.with_importance(importance);
        }

        if let Some(source) = args.source {
            memory = memory.with_source(source);
        }

        if let Some(channel_id) = args.channel_id {
            memory = memory.with_channel_id(Arc::from(channel_id.as_str()));
        }

        // Save to SQLite database
        let store = self.memory_search.store();

        // Human anchors (3.1a): a human-typed save resolves through the
        // per-human anchor mapping — merging into the existing anchor for
        // that human or creating and mapping a fresh one — then continues
        // through the same embedding/FTS/consolidation pipeline as every
        // other save, so anchors are recallable and count toward the
        // persistence contract. Persistence branches save per-session human
        // observations and they accumulate into one curated anchor per
        // person. `replaced_anchor` holds the pre-merge anchor for
        // compensation when a later pipeline step fails.
        let mut replaced_anchor: Option<Memory> = None;
        // Keep the anchor lock through embedding storage so concurrent saves
        // cannot leave the index with an earlier anchor revision.
        let _anchor_guard = if memory_type == MemoryType::Human {
            Some(self.anchor_lock.lock().await)
        } else {
            None
        };
        if memory_type == MemoryType::Human {
            let Some(human_id) = args.human_id.as_deref() else {
                return Err(MemorySaveError(
                    "human memories require human_id (the participant key of the person)".into(),
                ));
            };
            match store
                .get_human_anchor(human_id)
                .await
                .map_err(|e| MemorySaveError(format!("Failed to resolve human anchor: {e}")))?
            {
                Some(existing) => {
                    let mut merged = existing.clone();
                    merged.content = format!("{}\n{}", existing.content.trim_end(), args.content);
                    if merged.content.len() > MAX_MEMORY_CONTENT_BYTES {
                        return Err(MemorySaveError(format!(
                            "merged human anchor exceeds maximum length of {MAX_MEMORY_CONTENT_BYTES} bytes (got {})",
                            merged.content.len()
                        )));
                    }
                    merged.importance = existing.importance.max(memory.importance);
                    merged.updated_at = chrono::Utc::now();
                    store.update(&merged).await.map_err(|e| {
                        MemorySaveError(format!("Failed to merge human anchor: {e}"))
                    })?;
                    memory = merged;
                    replaced_anchor = Some(existing);
                }
                None => {
                    store
                        .save(&memory)
                        .await
                        .map_err(|e| MemorySaveError(format!("Failed to save memory: {e}")))?;
                    store
                        .set_human_anchor(human_id, &memory.id)
                        .await
                        .map_err(|e| MemorySaveError(format!("Failed to map human anchor: {e}")))?;
                }
            }
        } else {
            store
                .save(&memory)
                .await
                .map_err(|e| MemorySaveError(format!("Failed to save memory: {e}")))?;
        }

        // Create associations
        for assoc in args.associations {
            let relation_type = match assoc.relation_type.as_str() {
                "related_to" => crate::memory::types::RelationType::RelatedTo,
                "updates" => crate::memory::types::RelationType::Updates,
                "contradicts" => crate::memory::types::RelationType::Contradicts,
                "caused_by" => crate::memory::types::RelationType::CausedBy,
                "result_of" => crate::memory::types::RelationType::ResultOf,
                "part_of" => crate::memory::types::RelationType::PartOf,
                _ => crate::memory::types::RelationType::RelatedTo,
            };

            // Verify the target memory exists before creating a graph edge
            // to prevent dangling associations from LLM hallucinated IDs.
            match store.load(&assoc.target_id).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    tracing::warn!(
                        memory_id = %memory.id,
                        target_id = %assoc.target_id,
                        "skipping association to non-existent memory"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        memory_id = %memory.id,
                        target_id = %assoc.target_id,
                        %error,
                        "failed to verify target memory for association"
                    );
                    continue;
                }
            }

            let association = Association::new(&memory.id, &assoc.target_id, relation_type)
                .with_weight(assoc.weight);

            if let Err(error) = store.create_association(&association).await {
                tracing::warn!(
                    memory_id = %memory.id,
                    target_id = %assoc.target_id,
                    %error,
                    "failed to create memory association"
                );
            }
        }

        // Generate and store the embedding for the memory's final content
        // (the merged content on the anchor merge path). On failure,
        // compensate so the store and the index cannot disagree.
        let embedding = match self
            .memory_search
            .embedding_model_arc()
            .embed_one(&memory.content)
            .await
        {
            Ok(emb) => emb,
            Err(embed_err) => {
                self.compensate_embedding_failure(&memory, replaced_anchor.as_ref())
                    .await;
                return Err(MemorySaveError(format!(
                    "Failed to generate embedding: {embed_err}"
                )));
            }
        };

        match self
            .memory_search
            .embedding_table()
            .store(&memory.id, &embedding)
            .await
        {
            Ok(()) => {
                if let Some(contract_state) = &self.contract_state {
                    contract_state.record_saved_memory_id(memory.id.clone());
                }
            }
            Err(embed_err) => {
                self.compensate_embedding_failure(&memory, replaced_anchor.as_ref())
                    .await;
                return Err(MemorySaveError(format!(
                    "Failed to store embedding: {embed_err}"
                )));
            }
        }

        // The FTS5 index is maintained by triggers on `memories`, fired inside
        // the same transaction as the row write above — there is no separate
        // index to build or keep in sync here.

        // Write-time consolidation check. Advisory: the save has already
        // landed; the advice tells the branch about near-duplicates and
        // per-partition over-cap debt so it can run one atomic batch.
        let consolidation = match &self.runtime_config {
            Some(rc) => {
                let cortex_config = **rc.cortex.load();
                match crate::memory::consolidation::check_save(
                    self.memory_search.consolidation(),
                    self.memory_search.store(),
                    self.memory_search.embedding_table(),
                    &memory.id,
                    memory.memory_type,
                    cortex_config.consolidation_partition_cap,
                    cortex_config.consolidation_near_duplicate_threshold,
                )
                .await
                {
                    Ok(advice) => advice,
                    Err(error) => {
                        tracing::warn!(
                            memory_id = %memory.id,
                            %error,
                            "consolidation check failed after memory save"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        if let Some(event_context) = &self.event_context
            && event_context.memory_event_tx.receiver_count() > 0
        {
            let event = ProcessEvent::MemorySaved {
                agent_id: event_context.agent_id.clone(),
                memory_id: memory.id.clone(),
                channel_id: memory
                    .channel_id
                    .as_ref()
                    .map(|s| std::sync::Arc::from(s.as_str())),
                memory_type: memory.memory_type,
                importance: memory.importance,
                content_summary: summarize_memory_content(&memory.content),
            };
            if let Err(error) = event_context.memory_event_tx.send(event) {
                tracing::debug!(
                    memory_id = %memory.id,
                    %error,
                    "failed to emit memory-saved event"
                );
            }
        }

        if let Some(working_memory) = &self.working_memory {
            let content_preview = summarize_memory_content(&memory.content);
            let mut builder = working_memory
                .emit(
                    crate::memory::WorkingMemoryEventType::MemorySaved,
                    format!("Memory saved ({}): {content_preview}", memory.memory_type),
                )
                .importance(0.5);
            if let Some(channel_id) = &memory.channel_id {
                builder = builder.channel(channel_id.to_string());
            }
            builder.record();
        }

        #[cfg(feature = "metrics")]
        {
            let agent_id = self.memory_search.store().agent_id();
            let agent_label = if agent_id.is_empty() {
                "unknown"
            } else {
                agent_id
            };
            let metrics = crate::telemetry::Metrics::global();
            metrics
                .memory_writes_total
                .with_label_values(&[agent_label])
                .inc();
            metrics
                .memory_updates_total
                .with_label_values(&[agent_label, "save"])
                .inc();
        }

        let message = if replaced_anchor.is_some() {
            "merged into existing human anchor".to_string()
        } else if memory_type == MemoryType::Human {
            "created human anchor".to_string()
        } else {
            "Memory saved successfully".to_string()
        };

        Ok(MemorySaveOutput {
            memory_id: memory.id,
            success: true,
            message,
            consolidation,
        })
    }
}

/// Convenience function for simple fact saving.
pub async fn save_fact(
    memory_search: Arc<MemorySearch>,
    content: impl Into<String>,
    channel_id: Option<crate::ChannelId>,
) -> Result<String> {
    let tool = MemorySaveTool::new(memory_search);
    let args = MemorySaveArgs {
        content: content.into(),
        memory_type: "fact".to_string(),
        importance: None,
        source: None,
        channel_id: channel_id.map(|id| id.to_string()),
        human_id: None,
        associations: vec![],
    };

    let output = tool
        .call(args)
        .await
        .map_err(|e| crate::error::AgentError::Other(anyhow::anyhow!(e)))?;
    Ok(output.memory_id)
}

/// Seed anchor memories for configured org humans that carry a `HUMAN.md`
/// description, so participant context can resolve them in-turn before
/// reflection has observed them. Runs once at agent startup; idempotent — a
/// human with an existing anchor is left alone. Routed through the save tool
/// so seeded anchors get the same embedding and FTS treatment as any other
/// save.
pub async fn seed_org_human_anchors(
    memory_search: &Arc<MemorySearch>,
    humans: &[crate::config::HumanDef],
) {
    let tool = MemorySaveTool::new(memory_search.clone());
    for human in humans {
        let Some(description) = human.description.as_deref() else {
            continue;
        };
        if description.trim().is_empty() {
            continue;
        }
        match memory_search.store().get_human_anchor(&human.id).await {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, human_id = %human.id, "org anchor lookup failed");
                continue;
            }
        }
        let args = MemorySaveArgs {
            content: description.to_string(),
            memory_type: "human".to_string(),
            importance: Some(0.85),
            source: None,
            channel_id: None,
            human_id: Some(human.id.clone()),
            associations: vec![],
        };
        if let Err(error) = tool.call(args).await {
            tracing::warn!(%error, human_id = %human.id, "org anchor seeding failed");
        }
    }
}

fn summarize_memory_content(content: &str) -> String {
    crate::summarize_first_non_empty_line(content, crate::EVENT_SUMMARY_MAX_CHARS)
}

/// Build the `memory_type` enum entries for the tool schema from
/// [`MemoryType::ALL`], so the schema and the enum cannot drift.
fn memory_type_schema_entries() -> Vec<serde_json::Value> {
    MemoryType::ALL
        .iter()
        .map(|memory_type| {
            serde_json::json!({
                "const": memory_type.to_string(),
                "description": memory_type.schema_description(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_memory_content_prefers_first_non_empty_line() {
        let content = "\n\nFirst line summary\nSecond line details";
        assert_eq!(summarize_memory_content(content), "First line summary");
    }

    #[test]
    fn summarize_memory_content_truncates_to_max_chars() {
        let content = "a".repeat(200);
        let summary = summarize_memory_content(&content);
        assert_eq!(summary.chars().count(), crate::EVENT_SUMMARY_MAX_CHARS);
    }

    async fn memory_search_fixture() -> Arc<MemorySearch> {
        let store = crate::memory::MemoryStore::connect_in_memory().await;
        let embedding_model = crate::memory::embedding::shared_test_model();
        Arc::new(MemorySearch::new(store, embedding_model))
    }

    fn human_args(content: &str, human_id: &str) -> MemorySaveArgs {
        MemorySaveArgs {
            content: content.to_string(),
            memory_type: "human".to_string(),
            importance: None,
            source: None,
            channel_id: None,
            human_id: Some(human_id.to_string()),
            associations: vec![],
        }
    }

    #[tokio::test]
    async fn human_anchor_merge_appends_content_and_bumps_updated_at() {
        let memory_search = memory_search_fixture().await;
        let contract_state = Arc::new(
            super::super::memory_persistence_complete::MemoryPersistenceContractState::default(),
        );
        let tool =
            MemorySaveTool::new(memory_search.clone()).with_contract_state(contract_state.clone());

        let created = tool
            .call(human_args("Victor prefers direct answers.", "discord:123"))
            .await
            .expect("anchor create should succeed");
        assert_eq!(created.message, "created human anchor");

        // Backdate the anchor so a merge-time bump is observable.
        let store = memory_search.store();
        let mut anchor = store.load(&created.memory_id).await.unwrap().unwrap();
        let past = chrono::Utc::now() - chrono::Duration::days(2);
        anchor.updated_at = past;
        store.update(&anchor).await.unwrap();

        let merged = tool
            .call(human_args("Maintains the memory subsystem.", "discord:123"))
            .await
            .expect("anchor merge should succeed");
        assert_eq!(merged.memory_id, created.memory_id);
        assert_eq!(merged.message, "merged into existing human anchor");

        let anchor_after = store.load(&created.memory_id).await.unwrap().unwrap();
        assert!(
            anchor_after
                .content
                .contains("Victor prefers direct answers.")
        );
        assert!(
            anchor_after
                .content
                .contains("Maintains the memory subsystem.")
        );
        assert!(
            anchor_after.updated_at > past + chrono::Duration::days(1),
            "merge must bump updated_at to now"
        );

        // Both the create and the merge count toward the persistence contract.
        let recorded = contract_state.saved_memory_ids();
        assert!(recorded.contains(&created.memory_id));
    }

    #[tokio::test]
    async fn human_anchor_merge_rejects_content_over_the_cap() {
        let memory_search = memory_search_fixture().await;
        let tool = MemorySaveTool::new(memory_search.clone());
        let content = "a".repeat(MAX_MEMORY_CONTENT_BYTES);

        let created = tool
            .call(human_args(&content, "discord:123"))
            .await
            .expect("anchor create should succeed");
        let error = tool
            .call(human_args("b", "discord:123"))
            .await
            .expect_err("over-cap merge must fail");
        assert!(error.to_string().contains("merged human anchor exceeds"));

        let anchor = memory_search
            .store()
            .load(&created.memory_id)
            .await
            .unwrap()
            .expect("anchor should remain");
        assert_eq!(anchor.content, content);
    }
}
