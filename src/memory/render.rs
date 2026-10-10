//! Deterministic, LLM-free render of the memory store for the channel's
//! knowledge-context slot.
//!
//! Replaces read-time LLM knowledge synthesis: the render is a pure function
//! of the store, so identical store state produces identical bytes — a memory
//! write is the only thing that changes the block. The task board is rendered
//! as a separate block (`render_active_tasks`) so its independent churn does
//! not dilute this block's cache anchor. No LLM call exists
//! anywhere in this path. See `docs/design-docs/memory-first-knowledge-context.md`.

use crate::memory::{MemoryStore, MemoryType};
use crate::tasks::{TaskListFilter, TaskStatus, TaskStore};
use anyhow::{Context, Result};

/// Typed sections in render order, with per-type entry caps sized for the
/// default word budget. Facts first — the grounding section — then the
/// taxonomy order the `memory_save` tool teaches. Identity is excluded
/// (identity files own that layer), events live on the chronicle/working-memory
/// spine, concrete todos live on the task board, and human anchors are
/// scoped — they surface through the Participants block only for humans
/// present, never in the global render.
const SECTIONS: &[(MemoryType, &str, usize)] = &[
    (MemoryType::Fact, "Facts", 10),
    (MemoryType::Decision, "Decisions", 10),
    (MemoryType::Preference, "Preferences", 10),
    (MemoryType::Goal, "Goals", 10),
    (MemoryType::Observation, "Observations", 8),
];

/// The word budget the `SECTIONS` entry caps are sized for (the
/// `memory_render_max_words` default). A configured budget scales each cap
/// linearly against this baseline, so raising the budget deepens the render.
const BASELINE_BUDGET_WORDS: usize = 500;

/// Render the global memory-store view for the knowledge-context slot.
///
/// `max_words` caps the memory sections. The active-task board is rendered
/// separately ([`render_active_tasks`]) as its own prompt block, so this block's
/// change frequency is exactly "a memory was written" — it no longer shifts when
/// the task board does. Shown-of-total counts report what was
/// actually rendered against what the store holds, so the model knows when
/// branch recall into the full store is worth it. When the word budget
/// exhausts mid-render, the render stops — no section header is ever emitted
/// without at least one entry under it.
///
/// R3 strict append-only: entries render oldest-first in insertion order and a
/// new memory lands at the END of its section, so the block's earlier bytes are
/// a stable prefix across writes. The block is byte-identical between writes
/// and only grows when a memory is actually written. When a section outgrows
/// its cap, the front is evicted as a single batched watermark drop
/// (`crate::memory::store::watermark_eviction_window`), so the
/// cache-invalidating truncation is rare and amortized.
pub async fn render_memory_store(store: &MemoryStore, max_words: usize) -> Result<String> {
    let mut output = String::from("## Memory Store\n\nScope: global\n");
    let mut word_budget = max_words;

    for (memory_type, label, baseline_cap) in SECTIONS {
        if word_budget == 0 {
            break;
        }

        let total = store.count_by_type(*memory_type).await?;
        if total == 0 {
            continue;
        }

        // Scale the per-type entry cap with the configured budget, integer
        // math against the baseline, at least one entry per section.
        let entry_cap = (baseline_cap * max_words / BASELINE_BUDGET_WORDS).max(1);

        // Batched watermark eviction: once a section holds more than `entry_cap`
        // memories, the front drops back to the eviction watermark (~80% of the
        // cap) in ONE batch, instead of the oldest entry falling off on every
        // write. That keeps the append-only window in [watermark, cap] and
        // amortizes the cache-invalidating eviction across the resulting append
        // headroom, so a deep prefix-truncating miss is rare instead of a drip.
        // See `crate::memory::store::watermark_eviction_window`.
        let window = crate::memory::store::watermark_eviction_window(total, entry_cap as i64);

        let entries = store
            .get_by_type_append_only(*memory_type, window)
            .await?;
        // Strict append-only ordering (R3): entries render oldest-first in
        // insertion order, so a new memory adds bytes at the END of the block
        // and every earlier byte stays identical — the block keeps a stable,
        // growing cacheable prefix. No importance/updated_at re-sort here:
        // re-sorting could move an existing entry and shift the bytes after it,
        // truncating the prefix cache even though nothing before it changed.
        // Eviction drops the OLDEST entries off the front and only ever as a
        // whole batch (see `watermark_eviction_window`), which is the only
        // eviction that preserves prefix stability — and batching it keeps that
        // truncation rare instead of a per-write shave.

        // Render entries against the remaining budget before emitting the
        // header, so the shown-of-total count reflects what actually
        // rendered and an exhausted budget never leaves an empty section.
        let mut section = String::new();
        let mut shown = 0usize;
        for memory in &entries {
            let line = format!(
                "- {} ({})\n",
                first_line(&memory.content),
                memory.updated_at.format("%Y-%m-%d")
            );
            let words = line.split_whitespace().count();
            if words > word_budget {
                word_budget = 0;
                break;
            }
            word_budget -= words;
            section.push_str(&line);
            shown += 1;
        }

        if shown == 0 {
            break;
        }

        output.push('\n');
        if (shown as i64) < total {
            output.push_str(&format!("### {label} — {shown} of {total}\n"));
        } else {
            output.push_str(&format!("### {label}\n"));
        }
        output.push_str(&section);

        if shown < entries.len() {
            break;
        }
    }

    Ok(output)
}

/// The active-task board for this agent, as a standalone section.
///
/// Rendered separately from [`render_memory_store`] so the memory-store block's
/// change frequency is exactly "a memory was written": the task board changes on
/// task transitions, which are frequent and independent of memory writes. Gluing
/// the two together made the semi-volatile memory block change whenever the board
/// did, truncating the cacheable prefix far more often than its content required.
///
/// Non-done tasks assigned to this agent, as a standing section.
pub async fn render_active_tasks(task_store: &TaskStore, agent_id: &str) -> Result<String> {
    let mut all_tasks = Vec::new();
    for status in &[
        TaskStatus::InProgress,
        TaskStatus::Ready,
        TaskStatus::Backlog,
        TaskStatus::PendingApproval,
    ] {
        let tasks = task_store
            .list(TaskListFilter {
                assigned_agent_id: Some(agent_id.to_string()),
                status: Some(*status),
                limit: Some(20),
                ..Default::default()
            })
            .await
            .with_context(|| format!("failed to list {status} tasks for memory render"))?;
        all_tasks.extend(tasks);
    }

    let mut output = String::from("\n### Active Tasks\n");
    if all_tasks.is_empty() {
        output.push_str("- No active tasks.\n");
        return Ok(output);
    }
    for task in &all_tasks {
        let subtask_progress = if task.subtasks.is_empty() {
            String::new()
        } else {
            let done = task.subtasks.iter().filter(|s| s.completed).count();
            format!(" [{}/{}]", done, task.subtasks.len())
        };
        output.push_str(&format!(
            "- #{} [{}] ({}) {}{}\n",
            task.task_number, task.status, task.priority, task.title, subtask_progress,
        ));
    }
    Ok(output)
}

/// First non-empty line of a memory's content, for a single-line bullet.
fn first_line(content: &str) -> &str {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;
    use crate::tasks::{CreateTaskInput, TaskPriority};

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line("\n\nFirst line\nSecond"), "First line");
        assert_eq!(first_line("single"), "single");
        assert_eq!(first_line("   \n  \n"), "");
    }

    async fn render_fixture() -> (std::sync::Arc<MemoryStore>, TaskStore) {
        let store = MemoryStore::connect_in_memory().await;

        // Tasks live in the global database, so the task store gets its own
        // pool migrated with the global schema.
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .in_memory(true)
            .create_if_missing(true);
        let task_pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("in-memory SQLite");
        sqlx::migrate!("./migrations/global")
            .run(&task_pool)
            .await
            .expect("global migrations");

        (store, TaskStore::new(task_pool))
    }

    /// Save a memory whose rendered bullet is exactly five words:
    /// `- <three content words> (<date>)`.
    async fn save_three_word_memory(
        store: &MemoryStore,
        memory_type: MemoryType,
        content: &str,
        importance: f32,
    ) {
        assert_eq!(content.split_whitespace().count(), 3);
        let memory = Memory::new(content, memory_type).with_importance(importance);
        store.save(&memory).await.unwrap();
    }

    /// Content whose first line renders as a 60-word bullet:
    /// `-` + two prefix words + 56 filler words + the date.
    fn sixty_word_content(prefix: &str) -> String {
        format!("{prefix} {}", "lorem ".repeat(56).trim_end())
    }

    #[tokio::test]
    async fn exhausted_budget_stops_render_without_empty_headers() {
        let (store, _task_store) = render_fixture().await;
        for (prefix, importance) in [
            ("alpha fact", 0.9),
            ("bravo fact", 0.8),
            ("charlie fact", 0.7),
        ] {
            let memory = Memory::new(sixty_word_content(prefix), MemoryType::Fact)
                .with_importance(importance);
            store.save(&memory).await.unwrap();
        }
        save_three_word_memory(&store, MemoryType::Decision, "delta decision one", 0.9).await;

        // A 150-word budget keeps the Facts entry cap at 3 but only fits two
        // 60-word bullets; the third fact and the whole Decisions section do
        // not render.
        let rendered = render_memory_store(&store, 150)
            .await
            .unwrap();

        assert!(rendered.contains("### Facts — 2 of 3"));
        assert!(rendered.contains("alpha fact"));
        assert!(rendered.contains("bravo fact"));
        assert!(!rendered.contains("charlie fact"));
        assert!(
            !rendered.contains("### Decisions"),
            "an exhausted budget must not emit further section headers"
        );
        assert!(
            !rendered.contains("### Active Tasks"),
            "the task board is a separate block now, not part of the memory-store render"
        );
    }

    /// The task board is its own block: the memory-store render must NOT carry
    /// it (so the memory block's cacheable prefix survives task churn), and
    /// `render_active_tasks` must carry it independently.
    #[tokio::test]
    async fn active_tasks_are_split_out_of_the_memory_store_render() {
        let (store, task_store) = render_fixture().await;
        save_three_word_memory(&store, MemoryType::Fact, "alpha fact one", 0.9).await;

        task_store
            .create(CreateTaskInput {
                owner_agent_id: "agent".to_string(),
                assigned_agent_id: Some("agent".to_string()),
                title: "Ship the cache fix".to_string(),
                status: TaskStatus::InProgress,
                priority: TaskPriority::Medium,
                created_by: "agent".to_string(),
                ..Default::default()
            })
            .await
            .expect("task create");

        let memory = render_memory_store(&store, 500).await.unwrap();
        assert!(
            !memory.contains("Active Tasks") && !memory.contains("Ship the cache fix"),
            "the memory-store render must not embed the task board"
        );

        let tasks = render_active_tasks(&task_store, "agent").await.unwrap();
        assert!(tasks.contains("## Active Tasks"));
        assert!(tasks.contains("Ship the cache fix"));

        // A different agent's board is empty for this render.
        let other = render_active_tasks(&task_store, "someone-else").await.unwrap();
        assert!(other.contains("No active tasks."));
    }

    #[tokio::test]
    async fn count_is_omitted_when_every_entry_renders() {
        let (store, _task_store) = render_fixture().await;
        save_three_word_memory(&store, MemoryType::Fact, "alpha fact one", 0.9).await;
        save_three_word_memory(&store, MemoryType::Fact, "bravo fact two", 0.8).await;

        let rendered = render_memory_store(&store, 500)
            .await
            .unwrap();

        assert!(rendered.contains("### Facts\n"));
        assert!(!rendered.contains("### Facts —"));
    }

    #[tokio::test]
    async fn per_type_caps_scale_with_the_configured_budget() {
        let (store, _task_store) = render_fixture().await;
        for index in 0..12 {
            save_three_word_memory(
                &store,
                MemoryType::Fact,
                &format!("fact number {index:02}"),
                0.9,
            )
            .await;
        }

        // At the baseline budget the Facts cap is 10; with 12 stored, batched
        // watermark eviction has already trimmed the front, so 9 of the 12
        // render (the window band is [8, 10]).
        let baseline = render_memory_store(&store, 500)
            .await
            .unwrap();
        assert!(baseline.contains("### Facts — 9 of 12"));

        // Doubling the budget doubles the cap, so every entry renders.
        let doubled = render_memory_store(&store, 1000)
            .await
            .unwrap();
        assert!(doubled.contains("### Facts\n"));
        assert!(!doubled.contains("### Facts —"));
    }

    #[tokio::test]
    async fn human_anchors_are_excluded_from_the_global_render() {
        let (store, _task_store) = render_fixture().await;
        let anchor =
            Memory::new("Victor prefers direct answers", MemoryType::Human).with_importance(1.0);
        store.save(&anchor).await.unwrap();

        let rendered = render_memory_store(&store, 500)
            .await
            .unwrap();

        assert!(!rendered.contains("People"));
        assert!(!rendered.contains("Victor prefers direct answers"));
    }

    /// R3 strict append-only: after a memory is written, the previously
    /// rendered bytes must remain a PREFIX of the new render. A new entry
    /// appends at the end of its section and shifts nothing before it, so the
    /// block's cacheable prefix grows instead of being truncated.
    #[tokio::test]
    async fn new_memory_appends_and_preserves_prior_bytes_as_a_prefix() {
        let (store, _task_store) = render_fixture().await;
        save_three_word_memory(&store, MemoryType::Fact, "alpha fact one", 0.5).await;
        save_three_word_memory(&store, MemoryType::Fact, "bravo fact two", 0.9).await;

        let before = render_memory_store(&store, 500).await.unwrap();

        // A new memory — deliberately HIGHER importance than everything already
        // present. Under the old importance-sorted render this would insert
        // mid-block and shift the existing bytes; append-only ordering must
        // leave the prior bytes untouched and append at the end.
        save_three_word_memory(&store, MemoryType::Fact, "charlie fact three", 1.0).await;

        let after = render_memory_store(&store, 500).await.unwrap();

        assert!(
            after.starts_with(&before),
            "prior memory-store bytes must remain an exact prefix after a write\nbefore: {before:?}\nafter:  {after:?}"
        );
        assert!(
            after.contains("- charlie fact three ("),
            "the new memory must render at the end of the block\nafter: {after:?}"
        );
        // The new entry lands at the tail: nothing after it but its own date
        // suffix and the block's trailing newline.
        let charlie_line = after
            .rfind("- charlie fact three (")
            .expect("charlie line present");
        let tail = &after[charlie_line..];
        assert!(
            tail.trim_end().ends_with(')'),
            "nothing renders after the newest entry: {tail:?}"
        );
        // The insertion order is preserved regardless of importance.
        let alpha = after.find("alpha fact one").expect("alpha present");
        let charlie = after.find("charlie fact three").expect("charlie present");
        assert!(alpha < charlie, "entries render oldest-first, not by importance");
    }

    /// Eviction drops the OLDEST entries off the front of the block, so the
    /// surviving entries are the newest and their bytes stay a suffix of the
    /// larger render. Under batched watermark eviction the front drops in whole
    /// batches, so 12 stored against a cap of 10 leaves the window at 9.
    #[tokio::test]
    async fn cap_eviction_drops_the_oldest_entries_off_the_front() {
        let (store, _task_store) = render_fixture().await;
        for index in 0..12 {
            save_three_word_memory(
                &store,
                MemoryType::Fact,
                &format!("fact number {index:02}"),
                0.5,
            )
            .await;
        }

        // Baseline cap is 10; batched eviction has dropped the three oldest
        // (00, 01, 02) off the front in one batch, so the newest 9 render.
        let rendered = render_memory_store(&store, 500).await.unwrap();
        assert!(rendered.contains("### Facts — 9 of 12"));
        assert!(!rendered.contains("fact number 00"));
        assert!(!rendered.contains("fact number 01"));
        assert!(!rendered.contains("fact number 02"));
        assert!(rendered.contains("fact number 03"));
        assert!(rendered.contains("fact number 11"));
    }

    /// Batched watermark eviction: the write that CROSSES the cap drops the
    /// front straight back to the watermark in one batch, instead of shaving
    /// the single oldest entry. That is what amortizes the deep,
    /// cache-invalidating truncation: one eviction buys the append headroom
    /// from the watermark back to the cap.
    #[tokio::test]
    async fn cap_cross_evicts_a_batch_down_to_the_watermark() {
        let (store, _task_store) = render_fixture().await;

        // Fill exactly to the Facts cap (10): nothing is evicted yet, so every
        // entry renders and the shown-of-total count is omitted.
        for index in 0..10 {
            save_three_word_memory(
                &store,
                MemoryType::Fact,
                &format!("fact number {index:02}"),
                0.5,
            )
            .await;
        }
        let at_cap = render_memory_store(&store, 500).await.unwrap();
        assert!(at_cap.contains("fact number 00"), "at the cap: {at_cap}");
        assert!(at_cap.contains("fact number 09"));
        assert!(
            !at_cap.contains("### Facts —"),
            "all 10 of 10 render at the cap: {at_cap}"
        );

        // The first write PAST the cap evicts a batch: the front drops from 10
        // straight to the 8-entry watermark, so three entries go at once — not
        // the one entry a plain min(total, cap) window would drop.
        save_three_word_memory(&store, MemoryType::Fact, "fact number 10", 0.5).await;
        let past_cap = render_memory_store(&store, 500).await.unwrap();
        assert!(
            past_cap.contains("### Facts — 8 of 11"),
            "crossing the cap must drop to the watermark in one batch: {past_cap}"
        );
        for evicted in ["fact number 00", "fact number 01", "fact number 02"] {
            assert!(
                !past_cap.contains(evicted),
                "{evicted} should have been batch-evicted: {past_cap}"
            );
        }
        assert!(past_cap.contains("fact number 03"));
        assert!(
            past_cap.contains("fact number 10"),
            "the newest memory renders at the tail: {past_cap}"
        );

        // The window then grows one entry per write (append-only) until the next
        // batched eviction, so the block keeps a stable growing prefix.
        save_three_word_memory(&store, MemoryType::Fact, "fact number 11", 0.5).await;
        let grown = render_memory_store(&store, 500).await.unwrap();
        assert!(grown.contains("### Facts — 9 of 12"), "{grown}");
        assert!(
            grown.contains("fact number 03"),
            "the window only ever grows between evictions: {grown}"
        );
    }
}
