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

        let mut entries = store.get_by_type(*memory_type, entry_cap as i64).await?;
        // Stable ordering: importance desc, then updated_at desc, then id —
        // byte-identical between store writes even when SQLite's natural
        // ordering does not tie-break identically.
        entries.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.id.cmp(&b.id))
        });

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
        let (store, task_store) = render_fixture().await;
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
        let (store, task_store) = render_fixture().await;
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
        let (store, task_store) = render_fixture().await;
        for index in 0..12 {
            save_three_word_memory(
                &store,
                MemoryType::Fact,
                &format!("fact number {index:02}"),
                0.9,
            )
            .await;
        }

        // At the baseline budget the Facts cap is 10 of the 12 stored.
        let baseline = render_memory_store(&store, 500)
            .await
            .unwrap();
        assert!(baseline.contains("### Facts — 10 of 12"));

        // Doubling the budget doubles the cap, so every entry renders.
        let doubled = render_memory_store(&store, 1000)
            .await
            .unwrap();
        assert!(doubled.contains("### Facts\n"));
        assert!(!doubled.contains("### Facts —"));
    }

    #[tokio::test]
    async fn human_anchors_are_excluded_from_the_global_render() {
        let (store, task_store) = render_fixture().await;
        let anchor =
            Memory::new("Victor prefers direct answers", MemoryType::Human).with_importance(1.0);
        store.save(&anchor).await.unwrap();

        let rendered = render_memory_store(&store, 500)
            .await
            .unwrap();

        assert!(!rendered.contains("People"));
        assert!(!rendered.contains("Victor prefers direct answers"));
    }
}
