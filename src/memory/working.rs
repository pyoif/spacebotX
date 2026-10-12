//! Working memory event log and temporal context assembly.
//!
//! An append-only, structured event log scoped by day. Every significant thing
//! that happens across the agent is recorded as a timestamped event. Channels
//! get a progressively compressed view: today in detail, yesterday as a summary,
//! the week as a paragraph.
//!
//! User messages and agent responses are NOT stored here — they already live in
//! `conversation_messages`. Working memory events capture what happens *around*
//! conversations: worker lifecycle, branch conclusions, cron executions,
//! decisions, errors.

use crate::error::{Error, Result};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use std::fmt;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Typed event categories for working memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingMemoryEventType {
    /// A branch completed with a conclusion.
    BranchCompleted,
    /// A worker was spawned.
    WorkerSpawned,
    /// A worker completed (success or failure).
    WorkerCompleted,
    /// A cron job executed.
    CronExecuted,
    /// A memory was saved (by any path).
    MemorySaved,
    /// A decision was made (extracted from conversation).
    Decision,
    /// The user corrected prior instructions or assumptions.
    UserCorrection,
    /// A prior decision was revised.
    DecisionRevised,
    /// A concrete deadline or due date was set.
    DeadlineSet,
    /// Progress is currently blocked on an external dependency or prerequisite.
    BlockedOn,
    /// An explicit constraint was stated.
    Constraint,
    /// A task or branch reached a terminal result.
    Outcome,
    /// An error or failure occurred.
    Error,
    /// A task was created or updated.
    TaskUpdate,
    /// Cross-agent communication.
    AgentMessage,
    /// System event (startup, config change, maintenance).
    System,
    /// Reserved for tiered memory integration — graph memory promoted to working tier.
    MemoryPromoted,
    /// Reserved for tiered memory integration — working tier memory demoted to graph.
    MemoryDemoted,
}

impl WorkingMemoryEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BranchCompleted => "branch_completed",
            Self::WorkerSpawned => "worker_spawned",
            Self::WorkerCompleted => "worker_completed",
            Self::CronExecuted => "cron_executed",
            Self::MemorySaved => "memory_saved",
            Self::Decision => "decision",
            Self::UserCorrection => "user_correction",
            Self::DecisionRevised => "decision_revised",
            Self::DeadlineSet => "deadline_set",
            Self::BlockedOn => "blocked_on",
            Self::Constraint => "constraint",
            Self::Outcome => "outcome",
            Self::Error => "error",
            Self::TaskUpdate => "task_update",
            Self::AgentMessage => "agent_message",
            Self::System => "system",
            Self::MemoryPromoted => "memory_promoted",
            Self::MemoryDemoted => "memory_demoted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "branch_completed" => Some(Self::BranchCompleted),
            "worker_spawned" => Some(Self::WorkerSpawned),
            "worker_completed" => Some(Self::WorkerCompleted),
            "cron_executed" => Some(Self::CronExecuted),
            "memory_saved" => Some(Self::MemorySaved),
            "decision" => Some(Self::Decision),
            "user_correction" => Some(Self::UserCorrection),
            "decision_revised" => Some(Self::DecisionRevised),
            "deadline_set" => Some(Self::DeadlineSet),
            "blocked_on" => Some(Self::BlockedOn),
            "constraint" => Some(Self::Constraint),
            "outcome" => Some(Self::Outcome),
            "error" => Some(Self::Error),
            "task_update" => Some(Self::TaskUpdate),
            "agent_message" => Some(Self::AgentMessage),
            "system" => Some(Self::System),
            "memory_promoted" => Some(Self::MemoryPromoted),
            "memory_demoted" => Some(Self::MemoryDemoted),
            _ => None,
        }
    }
}

impl fmt::Display for WorkingMemoryEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single working memory event.
#[derive(Debug, Clone)]
pub struct WorkingMemoryEvent {
    pub id: String,
    pub event_type: WorkingMemoryEventType,
    pub timestamp: DateTime<Utc>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub summary: String,
    pub detail: Option<String>,
    pub importance: f32,
    /// Denormalized date string (YYYY-MM-DD) in the agent's configured timezone.
    pub day: String,
}

/// A single intra-day synthesis batch (50-100 word paragraph covering a time range).
#[derive(Debug, Clone)]
pub struct IntradaySynthesis {
    pub id: String,
    pub day: String,
    pub time_range_start: DateTime<Utc>,
    pub time_range_end: DateTime<Utc>,
    pub summary: String,
    pub event_count: i64,
    pub created_at: DateTime<Utc>,
}

/// A cortex-synthesized daily narrative.
#[derive(Debug, Clone)]
pub struct DailySummary {
    pub day: String,
    pub summary: String,
    pub event_count: i64,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The append-only working memory event log.
pub struct WorkingMemoryStore {
    pool: SqlitePool,
    timezone: Tz,
}

impl fmt::Debug for WorkingMemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkingMemoryStore")
            .field("timezone", &self.timezone)
            .finish()
    }
}

impl WorkingMemoryStore {
    /// Create a new working memory store.
    ///
    /// The `timezone` is used to compute the `day` column on every event —
    /// a 23:30 UTC event for a UTC+2 agent must be recorded as the next
    /// calendar day.
    pub fn new(pool: SqlitePool, timezone: Tz) -> Arc<Self> {
        Arc::new(Self { pool, timezone })
    }

    /// Compute today's date string in the agent's timezone.
    pub fn today(&self) -> String {
        let now_local = Utc::now().with_timezone(&self.timezone);
        now_local.format("%Y-%m-%d").to_string()
    }

    /// Compute a date string for a UTC timestamp in the agent's timezone.
    fn day_for_timestamp(&self, timestamp: DateTime<Utc>) -> String {
        let local = timestamp.with_timezone(&self.timezone);
        local.format("%Y-%m-%d").to_string()
    }

    /// Compute yesterday's date string in the agent's timezone.
    pub fn yesterday(&self) -> String {
        let now_local = Utc::now().with_timezone(&self.timezone);
        let yesterday = now_local.date_naive() - chrono::Duration::days(1);
        yesterday.format("%Y-%m-%d").to_string()
    }

    /// Get the configured timezone.
    pub fn timezone(&self) -> Tz {
        self.timezone
    }

    // -----------------------------------------------------------------------
    // Fire-and-forget recording
    // -----------------------------------------------------------------------

    /// Fire-and-forget event recording. Spawns a task, never blocks the caller.
    pub fn record(&self, event: WorkingMemoryEvent) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            if let Err(error) = insert_event(&pool, &event).await {
                tracing::warn!(%error, event_type = %event.event_type, "failed to record working memory event");
            }
        });
    }

    // -----------------------------------------------------------------------
    // Event queries
    // -----------------------------------------------------------------------

    /// Get events for a specific day, ordered by timestamp.
    pub async fn get_events_for_day(&self, day: &str) -> Result<Vec<WorkingMemoryEvent>> {
        let rows = sqlx::query(
            "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
             FROM working_memory_events WHERE day = ? ORDER BY timestamp ASC, id ASC",
        )
        .bind(day)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_event).collect()
    }

    /// Get recent events for a channel, used for context injection.
    pub async fn get_events_for_channel(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkingMemoryEvent>> {
        let rows = sqlx::query(
            "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
             FROM working_memory_events WHERE channel_id = ? ORDER BY timestamp DESC, id DESC LIMIT ?",
        )
        .bind(channel_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        // Reverse so oldest-first for rendering.
        let mut events: Vec<WorkingMemoryEvent> =
            rows.iter().map(row_to_event).collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// Get recent events across all channels, with importance filter.
    pub async fn get_recent_events(
        &self,
        limit: usize,
        min_importance: f32,
    ) -> Result<Vec<WorkingMemoryEvent>> {
        let rows = sqlx::query(
            "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
             FROM working_memory_events WHERE importance >= ? ORDER BY timestamp DESC, id DESC LIMIT ?",
        )
        .bind(min_importance)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events: Vec<WorkingMemoryEvent> =
            rows.iter().map(row_to_event).collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// Get recent events for a specific user (for participant context).
    pub async fn get_user_recent_events(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkingMemoryEvent>> {
        let rows = sqlx::query(
            "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
             FROM working_memory_events WHERE user_id = ? ORDER BY timestamp DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events: Vec<WorkingMemoryEvent> =
            rows.iter().map(row_to_event).collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// Get events after a timestamp for a given day (unsynthesized events).
    pub async fn get_events_after(
        &self,
        day: &str,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<WorkingMemoryEvent>> {
        let rows = match after {
            Some(after_ts) => {
                sqlx::query(
                    "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
                     FROM working_memory_events WHERE day = ? AND timestamp > ? ORDER BY timestamp ASC, id ASC",
                )
                .bind(day)
                .bind(after_ts)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day \
                     FROM working_memory_events WHERE day = ? ORDER BY timestamp ASC, id ASC",
                )
                .bind(day)
                .fetch_all(&self.pool)
                .await?
            }
        };

        rows.iter().map(row_to_event).collect()
    }

    /// Count events for a channel since a timestamp (for density trigger).
    pub async fn count_events_since(&self, channel_id: &str, since: DateTime<Utc>) -> Result<i64> {
        let row = sqlx::query(
            "SELECT COUNT(*) as count FROM working_memory_events WHERE channel_id = ? AND timestamp > ?",
        )
        .bind(channel_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get::<i64, _>("count"))
    }

    // -----------------------------------------------------------------------
    // Intra-day synthesis
    // -----------------------------------------------------------------------

    /// Get the end timestamp of the last intra-day synthesis for a day.
    pub async fn get_last_intraday_synthesis_end(
        &self,
        day: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        let row = sqlx::query(
            "SELECT time_range_end FROM working_memory_intraday_syntheses \
             WHERE day = ? ORDER BY time_range_start DESC, id DESC LIMIT 1",
        )
        .bind(day)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.get::<DateTime<Utc>, _>("time_range_end")))
    }

    /// Save an intra-day synthesis batch.
    pub async fn save_intraday_synthesis(
        &self,
        day: &str,
        time_start: DateTime<Utc>,
        time_end: DateTime<Utc>,
        summary: &str,
        event_count: usize,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO working_memory_intraday_syntheses \
             (id, day, time_range_start, time_range_end, summary, event_count) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(day)
        .bind(time_start)
        .bind(time_end)
        .bind(summary)
        .bind(event_count as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all intra-day syntheses for a day (for context rendering and daily rollup).
    pub async fn get_intraday_syntheses(&self, day: &str) -> Result<Vec<IntradaySynthesis>> {
        let rows = sqlx::query(
            "SELECT id, day, time_range_start, time_range_end, summary, event_count, created_at \
             FROM working_memory_intraday_syntheses WHERE day = ? ORDER BY time_range_start ASC, id ASC",
        )
        .bind(day)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_intraday_synthesis).collect())
    }

    // -----------------------------------------------------------------------
    // Daily summaries
    // -----------------------------------------------------------------------

    /// Check if a daily summary exists.
    pub async fn has_daily_summary(&self, day: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT COUNT(*) as count FROM working_memory_daily_summaries WHERE day = ?",
        )
        .bind(day)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get::<i64, _>("count") > 0)
    }

    /// Save a daily summary.
    pub async fn save_daily_summary(
        &self,
        day: &str,
        summary: &str,
        event_count: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO working_memory_daily_summaries (day, summary, event_count) \
             VALUES (?, ?, ?)",
        )
        .bind(day)
        .bind(summary)
        .bind(event_count)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get daily summary for a specific day.
    pub async fn get_daily_summary(&self, day: &str) -> Result<Option<DailySummary>> {
        let row = sqlx::query(
            "SELECT day, summary, event_count, created_at \
             FROM working_memory_daily_summaries WHERE day = ?",
        )
        .bind(day)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(row_to_daily_summary))
    }

    /// Get daily summaries for a date range (for week rendering).
    pub async fn get_daily_summaries_range(
        &self,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<DailySummary>> {
        let rows = sqlx::query(
            "SELECT day, summary, event_count, created_at \
             FROM working_memory_daily_summaries \
             WHERE day >= ? AND day <= ? ORDER BY day ASC",
        )
        .bind(from_day)
        .bind(to_day)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_daily_summary).collect())
    }

    // -----------------------------------------------------------------------
    // Pruning
    // -----------------------------------------------------------------------

    /// Prune raw events and intra-day syntheses older than N days.
    /// Daily summaries are never pruned (they are small and serve as permanent history).
    pub async fn prune_old_events(&self, retention_days: i64) -> Result<u64> {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);
        let cutoff_day = self.day_for_timestamp(cutoff);

        let events_result = sqlx::query("DELETE FROM working_memory_events WHERE day < ?")
            .bind(&cutoff_day)
            .execute(&self.pool)
            .await?;

        let syntheses_result =
            sqlx::query("DELETE FROM working_memory_intraday_syntheses WHERE day < ?")
                .bind(&cutoff_day)
                .execute(&self.pool)
                .await?;

        let total = events_result.rows_affected() + syntheses_result.rows_affected();
        if total > 0 {
            tracing::info!(
                events_pruned = events_result.rows_affected(),
                syntheses_pruned = syntheses_result.rows_affected(),
                cutoff_day = %cutoff_day,
                "pruned old working memory data"
            );
        }
        Ok(total)
    }

    // -----------------------------------------------------------------------
    // Builder API
    // -----------------------------------------------------------------------

    /// Convenience builder for common event emission.
    pub fn emit(
        self: &Arc<Self>,
        event_type: WorkingMemoryEventType,
        summary: impl Into<String>,
    ) -> WorkingMemoryEventBuilder {
        WorkingMemoryEventBuilder::new(Arc::clone(self), event_type, summary.into())
    }
}

// ---------------------------------------------------------------------------
// Rendering (Layers 2 + 3)
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 characters per token (typical for English text).
///
/// The previous heuristic (`len * 3 / 4`, i.e. ~0.75 tokens per *character*)
/// over-estimated by roughly 3x, which caused budget-split rendering to
/// under-fill every section. All call sites in this module use the value only
/// for budget comparisons, so the corrected math is applied globally here.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// The most any single rendered entry may occupy, in characters.
///
/// The store keeps the full text — only the render is bounded. A summary that
/// is a pasted task brief therefore costs one bounded line instead of
/// paragraphs, which is what keeps a spawn event or a long verbatim quote from
/// bloating the block. Truncation is by `char`, never by byte, so multi-byte
/// text cannot panic, and it never changes what the disclosure counts: an entry
/// is bounded, not dropped.
const RENDERED_ENTRY_MAX_CHARS: usize = 240;

/// Bound one rendered entry to [`RENDERED_ENTRY_MAX_CHARS`].
fn bound_rendered_entry(text: &str) -> String {
    bound_chars(text, RENDERED_ENTRY_MAX_CHARS)
}

/// Bound `text` to `max_chars`, cutting on a word boundary and marking the cut
/// with an ellipsis, which counts toward the limit.
///
/// Text that already fits is returned unchanged, so a healthy entry is never
/// touched and no ellipsis appears where nothing was removed. The boundary is
/// only used when one exists at or before the limit; a single unbroken token
/// still bounds. Cutting is by `char`, so multi-byte text cannot panic.
fn bound_chars(text: &str, max_chars: usize) -> String {
    let limit = max_chars.max(1);
    if text.chars().count() <= limit {
        return text.to_string();
    }

    // Reserve one character for the ellipsis, then prefer the last word
    // boundary inside what is left so a cut never leaves half a word.
    let head: String = text.chars().take(limit - 1).collect();
    let cut = match head.rfind(char::is_whitespace) {
        Some(index) if index > 0 => head[..index].chars().count(),
        _ => limit - 1,
    };

    let mut bounded: String = text.chars().take(cut).collect();
    bounded.push('…');
    bounded
}

/// Render Layer 2: Working Memory section for the channel system prompt.
///
/// Produces a markdown block with today's raw event tail, today's intra-day
/// synthesis paragraphs, yesterday's summary, and this week's summaries. All
/// rendering is programmatic — no LLM calls on this path.
///
/// The raw events are written before any synthesis claims the day's budget, so a
/// summary can never crowd out the events it summarises. When a day does not
/// fit, detail degrades with age (full line → one-liner → counted) and the
/// section discloses coverage rather than scarcity.
pub async fn render_working_memory(
    store: &WorkingMemoryStore,
    channel_id: &str,
    config: &crate::config::WorkingMemoryConfig,
    timezone: chrono_tz::Tz,
) -> Result<String> {
    use std::fmt::Write;

    if !config.enabled {
        return Ok(String::new());
    }

    let today = store.today();
    let yesterday = store.yesterday();
    let budget = config.context_token_budget;

    let mut output = String::with_capacity(2048);
    let mut tokens_used: usize = 0;
    let today_budget = budget * 60 / 100; // 60% for today
    let yesterday_budget = budget * 20 / 100; // next 20% for yesterday
    let week_budget = budget * 20 / 100; // remaining 20% for this week

    // --- Today header ---
    let now_local = Utc::now().with_timezone(&timezone);
    let day_name = now_local.format("%A, %B %-d").to_string();
    writeln!(output, "## Working Memory\n").ok();
    writeln!(output, "### Today ({day_name})").ok();

    // The day's ground truth, fetched once: the event log, the syntheses derived
    // from it, and the boundary between the two.
    let syntheses = store.get_intraday_syntheses(&today).await?;
    let last_synthesis_end = store.get_last_intraday_synthesis_end(&today).await?;
    let all_today = store.get_events_for_day(&today).await?;
    let unsynthesized = store.get_events_after(&today, last_synthesis_end).await?;
    let max_tail = config.today_max_unsynthesized_events;

    // 1. The raw tail renders first. A synthesis is a compression of these
    // events, so letting it spend the day budget ahead of them would let the
    // summary crowd out the thing it summarises — which is how a dense day came
    // to render "(0 of N events shown)".
    let mut events_raw = 0usize;
    let mut events_compressed = 0usize;
    let mut events_digested = 0usize;
    let mut lines: Vec<(String, usize)> = Vec::new();

    if !unsynthesized.is_empty() {
        // The label belongs to the raw events, so it leads them.
        if !syntheses.is_empty() {
            let since_label = last_synthesis_end
                .map(|t| t.with_timezone(&timezone).format("%H:%M").to_string())
                .unwrap_or_else(|| "start".to_string());
            writeln!(output, "\n**Since {since_label}:**").ok();
        }

        let detail_lines: Vec<String> = unsynthesized
            .iter()
            .map(|event| format_event_line(event, channel_id))
            .collect();
        let compact_lines: Vec<String> = unsynthesized
            .iter()
            .map(|event| format_event_one_liner(event, channel_id))
            .collect();

        // Spend what the day has left from the newest event backwards, so detail
        // degrades with age: the latest events keep full lines, older ones fall
        // back to a trimmed line, and the oldest are counted rather than
        // dropped. `today_max_unsynthesized_events` bounds how many keep a full
        // line; everything past that is still represented, at a coarser grain.
        let mut granularity = vec![EventGranularity::Digested; unsynthesized.len()];
        let mut remaining = today_budget.saturating_sub(tokens_used);
        let detail_limit = max_tail.max(1);
        for idx in (0..unsynthesized.len()).rev() {
            let age_rank = unsynthesized.len() - 1 - idx; // 0 is the newest
            let detail_cost = estimate_tokens(&detail_lines[idx]);
            let compact_cost = estimate_tokens(&compact_lines[idx]);
            if age_rank < detail_limit && detail_cost <= remaining {
                granularity[idx] = EventGranularity::Detail;
                remaining -= detail_cost;
            } else if compact_cost <= remaining {
                granularity[idx] = EventGranularity::OneLiner;
                remaining -= compact_cost;
            } else {
                granularity[idx] = EventGranularity::Digested;
            }
        }

        // Write chronologically, collapsing runs of identical detail lines
        // ("Agent started ×7"). Collapsing only ever shortens the section, so
        // the budget reserved above stays an upper bound.
        for idx in 0..unsynthesized.len() {
            match granularity[idx] {
                EventGranularity::Detail => {
                    events_raw += 1;
                    let line = detail_lines[idx].clone();
                    let repeats = matches!(lines.last(), Some((last, _)) if *last == line);
                    if repeats {
                        if let Some((_, count)) = lines.last_mut() {
                            *count += 1;
                        }
                    } else {
                        lines.push((line, 1));
                    }
                }
                EventGranularity::OneLiner => {
                    events_compressed += 1;
                    lines.push((compact_lines[idx].clone(), 1));
                }
                EventGranularity::Digested => events_digested += 1,
            }
        }

        for (line, count) in &lines {
            let rendered = if *count > 1 {
                format!("{line} ×{count}")
            } else {
                line.clone()
            };
            let line_tokens = estimate_tokens(&rendered);
            if tokens_used + line_tokens > today_budget {
                break;
            }
            writeln!(output, "- {rendered}").ok();
            tokens_used += line_tokens;
        }
        if events_digested > 0 {
            // Counted, not listed: the events past the day's budget are still
            // accounted for, so the disclosure below can stay complete.
            let counted = format!("- (+{events_digested} more events, counted not listed)");
            tokens_used += estimate_tokens(&counted);
            writeln!(output, "{counted}").ok();
        }
    }

    // 2. The day's syntheses then take what the raw events left. When they no
    // longer fit, they merge into one digest paragraph, so the events they cover
    // stay represented at a coarser grain instead of disappearing. Each
    // paragraph is bounded, so one long synthesis cannot spend the day on its
    // own verbosity — a synth paragraph is also cut, not the store.
    let mut syntheses_rendered = 0usize;
    for synthesis in &syntheses {
        let time_label = synthesis
            .time_range_start
            .with_timezone(&timezone)
            .format("%H:%M")
            .to_string();
        let bounded = bound_rendered_entry(&synthesis.summary);
        let block = format!("[{time_label}] {bounded}\n");
        let block_tokens = estimate_tokens(&block);
        if tokens_used + block_tokens > today_budget {
            break;
        }
        if syntheses_rendered == 0 && !unsynthesized.is_empty() {
            writeln!(output, "\n**Earlier today, synthesized:**").ok();
        }
        write!(output, "{block}").ok();
        tokens_used += block_tokens;
        syntheses_rendered += 1;
    }
    if syntheses_rendered < syntheses.len() {
        let digest = render_synthesis_digest(&syntheses[syntheses_rendered..], &timezone);
        if !digest.is_empty() {
            if syntheses_rendered == 0 && !unsynthesized.is_empty() {
                writeln!(output, "\n**Earlier today, synthesized:**").ok();
            }
            // Not added to `tokens_used`: the digest is the day's final render,
            // so a store here is dead — nothing past this point reads the
            // counter in a way that affects output — and dead stores are fatal
            // under CI's `-Dwarnings`.
            write!(output, "{digest}").ok();
        }
    }

    // 3. Disclose coverage, not scarcity. Every event of the day is represented
    // at one of the three grains, so the counts below account for all of them
    // and "(0 of N events shown)" is unrepresentable rather than unlikely.
    let total_today = all_today.len();
    let events_summarized = total_today.saturating_sub(events_raw + events_compressed);
    let syntheses_in_digest = syntheses_rendered < syntheses.len();
    if total_today > 0 && (events_compressed > 0 || events_digested > 0 || syntheses_in_digest) {
        let mut grains: Vec<String> = Vec::new();
        if events_raw > 0 {
            grains.push(format!("{events_raw} raw"));
        }
        if events_compressed > 0 {
            grains.push(format!("{events_compressed} compressed"));
        }
        if events_summarized > 0 {
            grains.push(format!("{events_summarized} in digest"));
        }
        writeln!(
            output,
            "({total_today} of {total_today} covered · {})",
            grains.join(" + ")
        )
        .ok();
    }

    // If today is completely empty, note it.
    if syntheses.is_empty() && unsynthesized.is_empty() {
        writeln!(output, "*No activity yet today.*").ok();
    }

    // --- Yesterday ---
    let yesterday_summary = store.get_daily_summary(&yesterday).await?;
    if let Some(summary) = yesterday_summary {
        let summary_tokens = estimate_tokens(&summary.summary);
        if summary_tokens <= yesterday_budget {
            let yesterday_local = (Utc::now() - chrono::Duration::days(1)).with_timezone(&timezone);
            let yesterday_name = yesterday_local.format("%A, %B %-d").to_string();
            writeln!(output, "\n### Yesterday ({yesterday_name})").ok();
            writeln!(output, "{}", summary.summary).ok();
            #[allow(unused_assignments)]
            {
                tokens_used += summary_tokens;
            }
        }
    }

    // --- This week (past 5 days, excluding today and yesterday) ---
    let week_start_local = now_local.date_naive() - chrono::Duration::days(6);
    let yesterday_date = now_local.date_naive() - chrono::Duration::days(1);
    // Only fetch days before yesterday.
    let week_end = (yesterday_date - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let week_start = week_start_local.format("%Y-%m-%d").to_string();

    if week_start <= week_end {
        let week_summaries = store
            .get_daily_summaries_range(&week_start, &week_end)
            .await?;
        if !week_summaries.is_empty() {
            let mut week_text = String::new();
            for summary in week_summaries.iter().rev() {
                // Most recent first within the week section.
                let candidate = format!("**{}:** {}\n", summary.day, summary.summary);
                let candidate_tokens = estimate_tokens(&candidate);
                if estimate_tokens(&week_text) + candidate_tokens > week_budget {
                    break;
                }
                week_text.push_str(&candidate);
            }
            if !week_text.is_empty() {
                writeln!(output, "\n### Earlier This Week").ok();
                write!(output, "{week_text}").ok();
            }
        }
    }

    Ok(output)
}

/// Render Layer 3: Channel Activity Map for the system prompt.
///
/// Shows what's happening in other channels. Uses a single SQL query with a
/// correlated subquery to get the last message per channel, plus a batch
/// query for topic hints from working memory events.
pub async fn render_channel_activity_map(
    pool: &sqlx::SqlitePool,
    _working_memory: &WorkingMemoryStore,
    exclude_channel_id: &str,
    config: &crate::config::WorkingMemoryConfig,
    _timezone: chrono_tz::Tz,
) -> Result<String> {
    use std::fmt::Write;

    let inactive_threshold = format!("-{} hours", config.channel_map_inactive_hours);
    let max_channels = config.channel_map_max_channels as i64;

    // Single query: last message per channel, excluding current channel.
    let rows = sqlx::query(
        "SELECT \
            c.id, \
            c.display_name, \
            c.platform, \
            m.sender_name AS last_sender_name, \
            m.created_at AS last_message_at, \
            (SELECT COUNT(*) FROM conversation_messages cm WHERE cm.channel_id = c.id) AS message_count \
         FROM channels c \
         LEFT JOIN conversation_messages m ON m.id = ( \
            SELECT id FROM conversation_messages \
            WHERE channel_id = c.id \
            ORDER BY created_at DESC, id DESC \
            LIMIT 1 \
         ) \
         WHERE c.id != ? \
           AND c.is_active = 1 \
           AND (m.created_at IS NULL OR m.created_at > datetime('now', ?)) \
         ORDER BY m.created_at DESC NULLS LAST, c.id ASC \
         LIMIT ?",
    )
    .bind(exclude_channel_id)
    .bind(&inactive_threshold)
    .bind(max_channels)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(String::new());
    }

    // Collect channel IDs for batch topic hint query.
    let channel_ids: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("id"))
        .collect();

    // Batch query: most recent BranchCompleted event per channel for topic hints.
    let topic_hints = get_topic_hints(pool, &channel_ids).await?;

    let now = Utc::now();
    let mut output = String::with_capacity(512);
    writeln!(output, "## Channel Activity\n").ok();

    for row in &rows {
        let channel_id: String = row.get("id");
        let display_name: Option<String> = row.get("display_name");
        let platform: String = row.get("platform");
        let last_sender: Option<String> = row.get("last_sender_name");
        let last_message_at: Option<DateTime<Utc>> = row.get("last_message_at");
        let message_count: i64 = row.get("message_count");

        let name = display_name.as_deref().unwrap_or(&channel_id);

        let time_ago = match last_message_at {
            Some(at) => format_time_ago(now, at),
            None => "no messages".to_string(),
        };

        let sender = last_sender.as_deref().unwrap_or("unknown");
        let topic = topic_hints.get(&channel_id);

        let mut line = format!(
            "- {name} ({platform}) — {time_ago} · {} messages · last: {sender}",
            group_thousands(message_count)
        );
        if let Some(topic_summary) = topic {
            // Truncate topic to keep the map compact.
            let truncated = if topic_summary.len() > 80 {
                let boundary = topic_summary.floor_char_boundary(80);
                format!("{}...", &topic_summary[..boundary])
            } else {
                topic_summary.clone()
            };
            write!(line, ": {truncated}").ok();
        }
        writeln!(output, "{line}").ok();
    }

    Ok(output)
}

/// Render Layer 4: participant context for the most recently active users in
/// the current channel.
pub async fn render_participant_context(
    working_memory: &WorkingMemoryStore,
    participants: &[crate::conversation::ActiveParticipant],
    channel_id: &str,
    config: &crate::config::ParticipantContextConfig,
    memory_store: &crate::memory::MemoryStore,
    anchor_cache: &tokio::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
) -> Result<String> {
    use std::fmt::Write;

    if !config.enabled || participants.len() < config.min_participants {
        return Ok(String::new());
    }

    let now = Utc::now();
    let mut output = String::new();
    let mut shown = 0usize;

    for participant in participants {
        shown += 1;
        write!(output, "**{}**", participant.display_name).ok();
        if let Some(role) = participant.role.as_deref() {
            write!(output, " -- {role}").ok();
        }
        writeln!(output).ok();

        if let Some(profile) = participant.profile_summary.as_deref() {
            writeln!(output, "  {profile}").ok();
        }

        let recent_line = participant_recent_activity(
            working_memory,
            &participant.participant_key,
            participant.last_message_at,
            now,
            channel_id,
        )
        .await?;
        writeln!(output, "  Recent: {recent_line}.").ok();

        // Human anchor (3.1a): the curated anchor memory for this person,
        // resolved exactly by participant key and cached per session. Only
        // the first line is shown inline — the anchor is a summary, not a
        // dump.
        if let Some(anchor) =
            resolve_human_anchor(memory_store, anchor_cache, &participant.participant_key).await?
        {
            let first_line = anchor.lines().next().unwrap_or("").trim();
            if !first_line.is_empty() {
                writeln!(output, "  Anchor: {first_line}").ok();
            }
        }
        writeln!(output).ok();

        if estimate_tokens(&output) >= config.token_budget {
            break;
        }
    }

    let header = if shown < participants.len() {
        format!("## Participants — {shown} of {}\n\n", participants.len())
    } else {
        "## Participants\n\n".to_string()
    };
    let output = format!("{header}{output}");

    if estimate_tokens(&output) > config.token_budget {
        let mut trimmed = String::new();
        let mut tokens_used = 0usize;
        for line in output.lines() {
            let candidate = format!("{line}\n");
            let candidate_tokens = estimate_tokens(&candidate);
            if tokens_used + candidate_tokens > config.token_budget {
                break;
            }
            trimmed.push_str(&candidate);
            tokens_used += candidate_tokens;
        }
        return Ok(trimmed.trim_end().to_string());
    }

    Ok(output.trim_end().to_string())
}

/// In-turn human anchor resolution with a per-session cache. Exact match on
/// the participant key; misses are cached too, so each human costs at most
/// one query per session (3.1a). The cache is shared and mutated in place —
/// the lock is held only around map access, never across the store query —
/// so concurrent prompt builds accumulate entries instead of clobbering
/// each other.
async fn resolve_human_anchor(
    store: &crate::memory::MemoryStore,
    cache: &tokio::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    human_id: &str,
) -> Result<Option<String>> {
    if let Some(cached) = cache.lock().await.get(human_id) {
        return Ok(cached.clone());
    }
    let resolved = store
        .get_human_anchor(human_id)
        .await?
        .map(|memory| memory.content);
    cache
        .lock()
        .await
        .insert(human_id.to_string(), resolved.clone());
    Ok(resolved)
}

/// Format an integer with `,` thousands separators for display.
fn group_thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Format a single event as a one-line summary for the raw tail.
fn format_event_line(event: &WorkingMemoryEvent, current_channel_id: &str) -> String {
    let type_label = match event.event_type {
        WorkingMemoryEventType::BranchCompleted => "Branch completed",
        WorkingMemoryEventType::WorkerSpawned => "Worker spawned",
        WorkingMemoryEventType::WorkerCompleted => "Worker completed",
        WorkingMemoryEventType::CronExecuted => "Cron executed",
        WorkingMemoryEventType::MemorySaved => "Memory saved",
        WorkingMemoryEventType::Decision => "Decision",
        WorkingMemoryEventType::UserCorrection => "User correction",
        WorkingMemoryEventType::DecisionRevised => "Decision revised",
        WorkingMemoryEventType::DeadlineSet => "Deadline set",
        WorkingMemoryEventType::BlockedOn => "Blocked on",
        WorkingMemoryEventType::Constraint => "Constraint",
        WorkingMemoryEventType::Outcome => "Outcome",
        WorkingMemoryEventType::Error => "Error",
        WorkingMemoryEventType::TaskUpdate => "Task update",
        WorkingMemoryEventType::AgentMessage => "Agent message",
        WorkingMemoryEventType::System => "System",
        WorkingMemoryEventType::MemoryPromoted => "Memory promoted",
        WorkingMemoryEventType::MemoryDemoted => "Memory demoted",
    };

    // Prefix with channel name if the event is from a different channel.
    let channel_prefix = match &event.channel_id {
        Some(cid) if cid != current_channel_id => format!("[{cid}] "),
        _ => String::new(),
    };

    // The label leads the line, but only when the summary does not already
    // carry it: some event sites store their own label in the text ("Worker
    // completed: Done. …"), and prepending it again rendered
    // "Worker completed: Worker completed: Done. …". The dedupe runs before
    // the bound, so the character budget is spent on the text rather than on a
    // repeated label.
    let body = if opens_with_label(&event.summary, type_label) {
        event.summary.trim_start().to_string()
    } else {
        format!("{type_label}: {}", event.summary)
    };

    // The line is bounded: a summary that is a pasted brief renders as one
    // bounded entry with the cut marked, never as paragraphs. The stored
    // summary is untouched.
    bound_rendered_entry(&format!("{channel_prefix}{body}"))
}

/// Whether `text` already opens with its own `label`.
///
/// The event sites are not uniform: some store a bare summary and some store
/// the label inside the text ("Worker completed: Done. 1b71b123 is on main.").
/// Both have to render as one labelled line, so the render asks first.
///
/// Matching ignores case and tolerates whitespace before the label. The colon
/// is required, which keeps the dedupe narrow: a summary that merely opens with
/// the same word it is labelled by ("Error while compiling" under an `Error`
/// label) still gets its label, since that text is not a label already.
fn opens_with_label(text: &str, label: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = strip_prefix_ignore_case(trimmed, label) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with(':')
}

/// [`str::strip_prefix`] without the case requirement.
///
/// A label that does not land on a character boundary yields `None`, so
/// multi-byte text cannot panic the comparison.
fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// How much of an event the render could afford.
///
/// Detail degrades with age: the newest events keep a full line, older ones fall
/// back to a trimmed one, and the oldest are counted in the disclosure. Every
/// grain still represents its event, which is what lets the coverage line be
/// complete — `Digested` means "counted", never "missing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventGranularity {
    /// The full [`format_event_line`] rendering.
    Detail,
    /// [`format_event_one_liner`]: the label plus the head of the summary.
    OneLiner,
    /// Counted in the coverage disclosure rather than listed.
    Digested,
}

/// The trimmed form used when the day's budget cannot afford a full line.
///
/// The event's label is kept, so a degraded line stays identifiable as the same
/// kind of event it always was; only the summary is cut, on a word boundary and
/// with the cut marked. Truncation is by `char`, so a summary full of multi-byte
/// text cannot panic.
fn format_event_one_liner(event: &WorkingMemoryEvent, current_channel_id: &str) -> String {
    const MAX_CHARS: usize = 80;
    bound_chars(&format_event_line(event, current_channel_id), MAX_CHARS)
}

/// Merge syntheses that no longer fit the day budget into one digest paragraph.
///
/// Dropping them would leave the events they cover with no representation at
/// all, so the paragraph is bounded instead: each synthesis keeps its time label
/// and the head of its summary, and the whole paragraph is capped. Every cut
/// lands on a word boundary and is marked with an ellipsis: a head ending
/// mid-word (`output-token audit, co ·`) reads as corruption rather than as a
/// summary that was trimmed.
fn render_synthesis_digest(syntheses: &[IntradaySynthesis], timezone: &chrono_tz::Tz) -> String {
    // Per-synthesis head inside the digest.
    const HEAD_MAX_CHARS: usize = 80;
    // Whole-paragraph bound.
    const DIGEST_MAX_CHARS: usize = 1200;
    if syntheses.is_empty() {
        return String::new();
    }
    let mut digest = String::from("In digest: ");
    for (idx, synthesis) in syntheses.iter().enumerate() {
        if idx > 0 {
            digest.push_str(" · ");
        }
        let time_label = synthesis
            .time_range_start
            .with_timezone(timezone)
            .format("%H:%M")
            .to_string();
        let head = bound_chars(&synthesis.summary, HEAD_MAX_CHARS);
        digest.push_str(&format!("[{time_label}] {head}"));
    }
    let mut bounded = bound_chars(&digest, DIGEST_MAX_CHARS);
    bounded.push('\n');
    bounded
}

/// Fetch the most recent BranchCompleted topic hint per channel.
async fn get_topic_hints(
    pool: &sqlx::SqlitePool,
    channel_ids: &[String],
) -> Result<std::collections::HashMap<String, String>> {
    use std::collections::HashMap;

    if channel_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Build a parameterized IN clause. sqlx doesn't support binding Vec<String>
    // to IN directly, so we construct the query dynamically.
    let placeholders: Vec<&str> = (0..channel_ids.len()).map(|_| "?").collect();
    let in_clause = placeholders.join(", ");

    // Semantic branch conclusions are stored by event type, so keep topic hints
    // scoped to summaries emitted from branches.
    let query = format!(
        "SELECT channel_id, summary FROM ( \
            SELECT channel_id, summary, \
                   ROW_NUMBER() OVER (PARTITION BY channel_id ORDER BY timestamp DESC, id DESC) AS rn \
            FROM working_memory_events \
            WHERE (event_type = 'branch_completed' \
                   OR (event_type IN ('outcome', 'blocked_on', 'constraint', 'deadline_set') \
                       AND summary LIKE 'Branch %')) \
              AND channel_id IN ({in_clause}) \
        ) WHERE rn = 1"
    );

    let mut query_builder = sqlx::query(&query);
    for id in channel_ids {
        query_builder = query_builder.bind(id);
    }

    let rows = query_builder.fetch_all(pool).await?;

    let mut hints = HashMap::new();
    for row in &rows {
        let channel_id: String = row.get("channel_id");
        let summary: String = row.get("summary");
        hints.insert(channel_id, summary);
    }

    Ok(hints)
}

/// Format a duration as a human-readable "time ago" string.
fn format_time_ago(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now - then;
    let minutes = delta.num_minutes();
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 1440 {
        let hours = minutes / 60;
        format!("{hours}h ago")
    } else {
        let days = minutes / 1440;
        format!("{days}d ago")
    }
}

async fn participant_recent_activity(
    working_memory: &WorkingMemoryStore,
    user_id: &str,
    last_message_at: DateTime<Utc>,
    now: DateTime<Utc>,
    channel_id: &str,
) -> Result<String> {
    let recent_events = working_memory.get_user_recent_events(user_id, 3).await?;
    if recent_events.is_empty() {
        return Ok(format!(
            "active in this channel {}",
            format_time_ago(now, last_message_at)
        ));
    }

    let parts: Vec<String> = recent_events
        .iter()
        .rev()
        .map(|event| {
            let channel_prefix = match &event.channel_id {
                Some(event_channel_id) if event_channel_id != channel_id => {
                    format!("in {event_channel_id} ")
                }
                _ => String::new(),
            };
            format!(
                "{channel_prefix}{} {}",
                event.summary,
                format_time_ago(now, event.timestamp)
            )
        })
        .collect();

    Ok(parts.join(", "))
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Ergonomic builder for constructing and recording working memory events.
pub struct WorkingMemoryEventBuilder {
    store: Arc<WorkingMemoryStore>,
    event_type: WorkingMemoryEventType,
    summary: String,
    channel_id: Option<String>,
    user_id: Option<String>,
    detail: Option<String>,
    importance: f32,
}

impl WorkingMemoryEventBuilder {
    fn new(
        store: Arc<WorkingMemoryStore>,
        event_type: WorkingMemoryEventType,
        summary: String,
    ) -> Self {
        Self {
            store,
            event_type,
            summary,
            channel_id: None,
            user_id: None,
            detail: None,
            importance: 0.5,
        }
    }

    pub fn channel(mut self, channel_id: impl Into<String>) -> Self {
        self.channel_id = Some(channel_id.into());
        self
    }

    pub fn user(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn importance(mut self, importance: f32) -> Self {
        self.importance = importance;
        self
    }

    /// Fire-and-forget: record the event without blocking.
    pub fn record(self) {
        let now = Utc::now();
        let day = self.store.day_for_timestamp(now);

        let event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: self.event_type,
            timestamp: now,
            channel_id: self.channel_id,
            user_id: self.user_id,
            summary: self.summary,
            detail: self.detail,
            importance: self.importance,
            day,
        };

        self.store.record(event);
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_event(row: &sqlx::sqlite::SqliteRow) -> Result<WorkingMemoryEvent> {
    let id: String = row.get("id");
    let event_type_str: String = row.get("event_type");
    let Some(event_type) = WorkingMemoryEventType::parse(&event_type_str) else {
        return Err(Error::Other(anyhow::anyhow!(
            "unknown working memory event_type '{event_type_str}' for event {id}"
        )));
    };

    Ok(WorkingMemoryEvent {
        id,
        event_type,
        timestamp: row.get("timestamp"),
        channel_id: row.get("channel_id"),
        user_id: row.get("user_id"),
        summary: row.get("summary"),
        detail: row.get("detail"),
        importance: row.get("importance"),
        day: row.get("day"),
    })
}

fn row_to_intraday_synthesis(row: &sqlx::sqlite::SqliteRow) -> IntradaySynthesis {
    IntradaySynthesis {
        id: row.get("id"),
        day: row.get("day"),
        time_range_start: row.get("time_range_start"),
        time_range_end: row.get("time_range_end"),
        summary: row.get("summary"),
        event_count: row.get("event_count"),
        created_at: row.get("created_at"),
    }
}

fn row_to_daily_summary(row: &sqlx::sqlite::SqliteRow) -> DailySummary {
    DailySummary {
        day: row.get("day"),
        summary: row.get("summary"),
        event_count: row.get("event_count"),
        created_at: row.get("created_at"),
    }
}

async fn insert_event(pool: &SqlitePool, event: &WorkingMemoryEvent) -> Result<()> {
    sqlx::query(
        "INSERT INTO working_memory_events \
         (id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event.id)
    .bind(event.event_type.as_str())
    .bind(event.timestamp)
    .bind(&event.channel_id)
    .bind(&event.user_id)
    .bind(&event.summary)
    .bind(&event.detail)
    .bind(event.importance)
    .bind(&event.day)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_store() -> Arc<WorkingMemoryStore> {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        WorkingMemoryStore::new(pool, Tz::UTC)
    }

    #[tokio::test]
    async fn test_record_and_query_events() {
        let store = setup_test_store().await;

        let today = store.today();

        // Record a few events directly (bypassing fire-and-forget for test determinism).
        let event1 = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::WorkerSpawned,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Worker spawned: compile project".to_string(),
            detail: None,
            importance: 0.6,
            day: today.clone(),
        };
        insert_event(&store.pool, &event1).await.unwrap();

        let event2 = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::BranchCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: Some("user-1".to_string()),
            summary: "Branch concluded: auth module needs refactor".to_string(),
            detail: Some("Full analysis of the auth module...".to_string()),
            importance: 0.8,
            day: today.clone(),
        };
        insert_event(&store.pool, &event2).await.unwrap();

        // Query by day.
        let events = store.get_events_for_day(&today).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, WorkingMemoryEventType::WorkerSpawned);
        assert_eq!(
            events[1].event_type,
            WorkingMemoryEventType::BranchCompleted
        );

        // Query by channel.
        let channel_events = store.get_events_for_channel("chan-1", 10).await.unwrap();
        assert_eq!(channel_events.len(), 2);

        // Query by user.
        let user_events = store.get_user_recent_events("user-1", 10).await.unwrap();
        assert_eq!(user_events.len(), 1);
        assert_eq!(
            user_events[0].event_type,
            WorkingMemoryEventType::BranchCompleted
        );

        // Count since.
        let count = store
            .count_events_since("chan-1", Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_event_type_roundtrip() {
        let store = setup_test_store().await;
        let today = store.today();

        let inserted = [
            WorkingMemoryEventType::BranchCompleted,
            WorkingMemoryEventType::WorkerSpawned,
            WorkingMemoryEventType::WorkerCompleted,
            WorkingMemoryEventType::CronExecuted,
            WorkingMemoryEventType::MemorySaved,
            WorkingMemoryEventType::Decision,
            WorkingMemoryEventType::UserCorrection,
            WorkingMemoryEventType::DecisionRevised,
            WorkingMemoryEventType::DeadlineSet,
            WorkingMemoryEventType::BlockedOn,
            WorkingMemoryEventType::Constraint,
            WorkingMemoryEventType::Outcome,
            WorkingMemoryEventType::Error,
            WorkingMemoryEventType::TaskUpdate,
            WorkingMemoryEventType::AgentMessage,
            WorkingMemoryEventType::System,
            WorkingMemoryEventType::MemoryPromoted,
            WorkingMemoryEventType::MemoryDemoted,
        ];

        for event_type in inserted {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type,
                timestamp: Utc::now(),
                channel_id: None,
                user_id: None,
                summary: format!("test {}", event_type.as_str()),
                detail: None,
                importance: 0.5,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let events = store.get_events_for_day(&today).await.unwrap();
        assert_eq!(events.len(), inserted.len());

        // Verify all types survived the roundtrip.
        let types: Vec<WorkingMemoryEventType> = events.iter().map(|e| e.event_type).collect();
        assert!(types.contains(&WorkingMemoryEventType::BranchCompleted));
        assert!(types.contains(&WorkingMemoryEventType::MemoryDemoted));
    }

    #[tokio::test]
    async fn unknown_event_type_returns_decode_error() {
        let store = setup_test_store().await;
        let today = store.today();

        sqlx::query(
            "INSERT INTO working_memory_events \
             (id, event_type, timestamp, channel_id, user_id, summary, detail, importance, day) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("wm_unknown")
        .bind("unknown_type")
        .bind(Utc::now())
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind("unknown event")
        .bind(Option::<String>::None)
        .bind(0.5_f32)
        .bind(&today)
        .execute(&store.pool)
        .await
        .unwrap();

        let error = store.get_events_for_day(&today).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown working memory event_type 'unknown_type' for event wm_unknown")
        );
    }

    #[tokio::test]
    async fn topic_hints_include_branch_semantic_terminal_events() {
        let store = setup_test_store().await;
        let today = store.today();
        let base = Utc::now();

        insert_event(
            &store.pool,
            &WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::Outcome,
                timestamp: base,
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: "Branch outcome: selected the task-store transaction fix".to_string(),
                detail: None,
                importance: 0.7,
                day: today.clone(),
            },
        )
        .await
        .unwrap();
        insert_event(
            &store.pool,
            &WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::Outcome,
                timestamp: base + chrono::Duration::minutes(1),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: "Task #12 completed".to_string(),
                detail: None,
                importance: 0.7,
                day: today,
            },
        )
        .await
        .unwrap();

        let channel_ids = vec!["chan-1".to_string()];
        let hints = get_topic_hints(&store.pool, &channel_ids).await.unwrap();

        assert_eq!(
            hints.get("chan-1").map(String::as_str),
            Some("Branch outcome: selected the task-store transaction fix")
        );
    }

    #[tokio::test]
    async fn test_daily_summary_crud() {
        let store = setup_test_store().await;

        assert!(!store.has_daily_summary("2026-03-18").await.unwrap());

        store
            .save_daily_summary("2026-03-18", "Busy day. Shipped 3 features.", 42)
            .await
            .unwrap();

        assert!(store.has_daily_summary("2026-03-18").await.unwrap());

        let summary = store
            .get_daily_summary("2026-03-18")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.summary, "Busy day. Shipped 3 features.");
        assert_eq!(summary.event_count, 42);

        // Idempotent — save again should replace.
        store
            .save_daily_summary("2026-03-18", "Updated summary.", 43)
            .await
            .unwrap();
        let updated = store
            .get_daily_summary("2026-03-18")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.summary, "Updated summary.");
    }

    #[tokio::test]
    async fn test_intraday_synthesis() {
        let store = setup_test_store().await;
        let today = store.today();

        let start = Utc::now() - chrono::Duration::hours(2);
        let end = Utc::now() - chrono::Duration::hours(1);

        store
            .save_intraday_synthesis(&today, start, end, "Morning batch: compiled, tested.", 8)
            .await
            .unwrap();

        let syntheses = store.get_intraday_syntheses(&today).await.unwrap();
        assert_eq!(syntheses.len(), 1);
        assert_eq!(syntheses[0].event_count, 8);

        let last_end = store.get_last_intraday_synthesis_end(&today).await.unwrap();
        assert!(last_end.is_some());
    }

    #[tokio::test]
    async fn test_builder_api() {
        let store = setup_test_store().await;

        // Use the builder to emit an event. Since `record()` is fire-and-forget
        // via tokio::spawn, we need a small sleep for the write to complete.
        store
            .emit(WorkingMemoryEventType::System, "Agent started")
            .importance(0.3)
            .record();

        store
            .emit(
                WorkingMemoryEventType::WorkerSpawned,
                "Worker: compile check",
            )
            .channel("test-channel")
            .importance(0.6)
            .record();

        // Allow the spawned tasks to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let today = store.today();
        let events = store.get_events_for_day(&today).await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_pruning() {
        let store = setup_test_store().await;

        // Insert an event with an old day.
        let old_event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::System,
            timestamp: Utc::now() - chrono::Duration::days(60),
            channel_id: None,
            user_id: None,
            summary: "Old event".to_string(),
            detail: None,
            importance: 0.5,
            day: "2026-01-01".to_string(),
        };
        insert_event(&store.pool, &old_event).await.unwrap();

        // Insert a recent event.
        let recent_event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::System,
            timestamp: Utc::now(),
            channel_id: None,
            user_id: None,
            summary: "Recent event".to_string(),
            detail: None,
            importance: 0.5,
            day: store.today(),
        };
        insert_event(&store.pool, &recent_event).await.unwrap();

        let pruned = store.prune_old_events(30).await.unwrap();
        assert!(pruned >= 1);

        // Recent event should survive.
        let today_events = store.get_events_for_day(&store.today()).await.unwrap();
        assert_eq!(today_events.len(), 1);
    }

    #[tokio::test]
    async fn test_get_events_after() {
        let store = setup_test_store().await;
        let today = store.today();

        let t1 = Utc::now() - chrono::Duration::minutes(30);
        let t2 = Utc::now() - chrono::Duration::minutes(15);
        let t3 = Utc::now();

        for (i, ts) in [t1, t2, t3].iter().enumerate() {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::System,
                timestamp: *ts,
                channel_id: None,
                user_id: None,
                summary: format!("event {i}"),
                detail: None,
                importance: 0.5,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        // All events for today (no after filter).
        let all = store.get_events_after(&today, None).await.unwrap();
        assert_eq!(all.len(), 3);

        // Events after t1 should be t2 and t3.
        let after_t1 = store.get_events_after(&today, Some(t1)).await.unwrap();
        assert_eq!(after_t1.len(), 2);
    }

    #[tokio::test]
    async fn test_daily_summaries_range() {
        let store = setup_test_store().await;

        store
            .save_daily_summary("2026-03-15", "Day 1", 10)
            .await
            .unwrap();
        store
            .save_daily_summary("2026-03-16", "Day 2", 20)
            .await
            .unwrap();
        store
            .save_daily_summary("2026-03-17", "Day 3", 30)
            .await
            .unwrap();
        store
            .save_daily_summary("2026-03-18", "Day 4", 40)
            .await
            .unwrap();

        let range = store
            .get_daily_summaries_range("2026-03-16", "2026-03-18")
            .await
            .unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].day, "2026-03-16");
        assert_eq!(range[2].day, "2026-03-18");
    }

    fn test_config() -> crate::config::WorkingMemoryConfig {
        crate::config::WorkingMemoryConfig::default()
    }

    #[tokio::test]
    async fn test_render_working_memory_empty() {
        let store = setup_test_store().await;
        let config = test_config();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(rendered.contains("## Working Memory"), "should have header");
        assert!(
            rendered.contains("No activity yet today"),
            "should note empty day"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_with_events() {
        let store = setup_test_store().await;
        let config = test_config();
        let today = store.today();

        // Insert some events.
        for i in 0..3 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now(),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Worker {i} completed successfully"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(rendered.contains("## Working Memory"), "should have header");
        assert!(
            rendered.contains("Worker 0 completed"),
            "should contain events"
        );
        assert!(
            rendered.contains("Worker 2 completed"),
            "should contain all events"
        );
        // Should NOT say "No activity" since we have events.
        assert!(!rendered.contains("No activity yet today"));
    }

    #[tokio::test]
    async fn test_render_working_memory_with_synthesis_and_tail() {
        let store = setup_test_store().await;
        let config = test_config();
        let today = store.today();

        // Add an intra-day synthesis.
        let start = Utc::now() - chrono::Duration::hours(3);
        let end = Utc::now() - chrono::Duration::hours(2);
        store
            .save_intraday_synthesis(
                &today,
                start,
                end,
                "Morning: deployed v1.2.0, fixed 3 bugs, ran full test suite.",
                8,
            )
            .await
            .unwrap();

        // Add an unsynthesized event after the synthesis.
        let event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::BranchCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Analyzed auth module for refactoring".to_string(),
            detail: None,
            importance: 0.7,
            day: today.clone(),
        };
        insert_event(&store.pool, &event).await.unwrap();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(
            rendered.contains("Morning: deployed v1.2.0"),
            "should contain synthesis"
        );
        assert!(
            rendered.contains("auth module"),
            "should contain tail event"
        );
        assert!(
            rendered.contains("Since"),
            "should have 'Since' label for tail"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_with_yesterday() {
        let store = setup_test_store().await;
        let config = test_config();

        let yesterday = store.yesterday();
        store
            .save_daily_summary(&yesterday, "Quiet day. Fixed auth bug. Reviewed 3 PRs.", 12)
            .await
            .unwrap();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(
            rendered.contains("### Yesterday"),
            "should have yesterday section"
        );
        assert!(
            rendered.contains("auth bug"),
            "should contain yesterday content"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_respects_token_budget() {
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 200; // Very tight budget.

        // Add many events to exceed the budget.
        for i in 0..50 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now(),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Worker {i} completed with a long description of what was done"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        // Should not contain all 50 events — budget should cap it.
        let event_lines = rendered.lines().filter(|l| l.starts_with("- ")).count();
        assert!(
            event_lines < 50,
            "should be capped by token budget, got {event_lines}"
        );
    }

    /// Parse the coverage disclosure: `(N of N covered · A raw + B compressed +
    /// C in digest)`. Returns the headline counts and each grain, so a test can
    /// check that the grains account for every event rather than trusting the
    /// headline alone.
    #[allow(clippy::type_complexity)]
    fn parse_coverage(rendered: &str) -> Option<(usize, usize, Vec<(usize, String)>)> {
        let line = rendered.lines().find(|l| l.contains(" covered · "))?;
        let inner = line.trim().trim_start_matches('(').trim_end_matches(')');
        let (head, grains) = inner.split_once(" covered · ")?;
        let (covered, total) = head.split_once(" of ")?;
        let grains = grains
            .split(" + ")
            .filter_map(|grain| {
                let (count, label) = grain.trim().split_once(' ')?;
                Some((count.parse::<usize>().ok()?, label.to_string()))
            })
            .collect();
        Some((
            covered.trim().parse().ok()?,
            total.trim().parse().ok()?,
            grains,
        ))
    }

    #[tokio::test]
    async fn test_render_working_memory_tail_survives_synthesis_starvation() {
        // Regression: intra-day synthesis paragraphs used to render before the
        // raw unsynthesized tail and share the whole day budget, so a large
        // enough synthesis block consumed it entirely and the tail loop broke on
        // its first line — rendering "(0 of N events shown)". The tail now
        // renders first, so the dense day must disclose full coverage instead.
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 400; // today budget = 240

        // Fill (and overflow) the synthesis region with long paragraphs.
        for i in 0..5 {
            let start = Utc::now() - chrono::Duration::hours(6 - i);
            let end = Utc::now() - chrono::Duration::hours(5 - i);
            let long_summary = format!(
                "Batch {i}: {}",
                "completed a large amount of work across many files ".repeat(20)
            );
            store
                .save_intraday_synthesis(&today, start, end, &long_summary, 8)
                .await
                .unwrap();
        }

        // Events after the last synthesis — these are the raw tail.
        for i in 0..5 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now(),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Tail event {i}"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        let event_lines = rendered.lines().filter(|l| l.starts_with("- ")).count();
        assert!(
            event_lines >= 1,
            "raw tail must render at least one event even when syntheses fill the budget, got {event_lines}\n{rendered}"
        );
        assert!(
            !rendered.contains("(0 of "),
            "must not disclose zero events shown while events exist\n{rendered}"
        );
        // Every event of the day is represented, and the disclosure says so.
        let (covered, total, grains) = parse_coverage(&rendered).expect("coverage disclosed");
        assert_eq!((covered, total), (5, 5), "coverage must be complete");
        assert_eq!(
            grains.iter().map(|(count, _)| count).sum::<usize>(),
            total,
            "the grains must account for every event\n{rendered}"
        );
        // The syntheses no longer fit, so they degrade to the digest rather than
        // being dropped — the earlier events keep a representation.
        assert!(
            rendered.contains("Earlier today, synthesized:"),
            "overflowing syntheses must still be represented\n{rendered}"
        );
        assert!(
            rendered.contains("In digest: "),
            "overflowing syntheses must merge into one digest\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_covers_every_event_on_a_saturated_day() {
        // A day where detail, one-liners and the digest are all needed at once.
        // The point of the change is that this renders complete coverage: no
        // event is excluded, so no disclosure can report a shortfall.
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 300; // today budget = 180

        for i in 0..3 {
            let start = Utc::now() - chrono::Duration::hours(9 - i);
            let end = Utc::now() - chrono::Duration::hours(8 - i);
            store
                .save_intraday_synthesis(
                    &today,
                    start,
                    end,
                    &"a whole batch of work summarised at length ".repeat(12),
                    8,
                )
                .await
                .unwrap();
        }

        for i in 0..40 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now(),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Worker {i} completed a long description of its work"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        let (covered, total, grains) = parse_coverage(&rendered).expect("coverage disclosed");
        assert_eq!(total, 40, "all of the day's events are accounted for");
        assert_eq!(covered, total, "nothing may be excluded\n{rendered}");
        assert_eq!(
            grains.iter().map(|(count, _)| count).sum::<usize>(),
            total,
            "the grains must add up to the day\n{rendered}"
        );
        let labels: Vec<&str> = grains.iter().map(|(_, label)| label.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.contains("digest")),
            "events past the budget must be counted, got {labels:?}\n{rendered}"
        );
        assert!(
            !rendered.contains("(0 of "),
            "the starvation state must be unrepresentable\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_renders_the_raw_tail_before_syntheses() {
        // Order is the mechanism: the events are written before any synthesis
        // claims budget, so a summary can never crowd out what it summarises.
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 400; // today budget = 240

        for i in 0..4 {
            let start = Utc::now() - chrono::Duration::hours(8 - i);
            let end = Utc::now() - chrono::Duration::hours(7 - i);
            store
                .save_intraday_synthesis(
                    &today,
                    start,
                    end,
                    &"a long synthesized batch of afternoon work ".repeat(12),
                    6,
                )
                .await
                .unwrap();
        }

        let event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::BranchCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Analyzed auth module".to_string(),
            detail: None,
            importance: 0.7,
            day: today.clone(),
        };
        insert_event(&store.pool, &event).await.unwrap();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        let tail_at = rendered
            .find("auth module")
            .expect("the raw event must render");
        let syntheses_at = rendered
            .find("Earlier today, synthesized:")
            .expect("the syntheses must render");
        assert!(
            tail_at < syntheses_at,
            "raw events come first, got tail at {tail_at} and syntheses at {syntheses_at}\n{rendered}"
        );
        assert_eq!(
            rendered.matches("In digest:").count(),
            1,
            "syntheses that overflow merge into a single digest, never one each\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_normal_day_stays_plain() {
        // A day that fits must read exactly as before: every event as a raw line,
        // the synthesis in full, and no coverage disclosure at all.
        let store = setup_test_store().await;
        let today = store.today();
        let config = test_config(); // 1500-token budget

        let start = Utc::now() - chrono::Duration::hours(3);
        let end = Utc::now() - chrono::Duration::hours(2);
        store
            .save_intraday_synthesis(
                &today,
                start,
                end,
                "Morning: deployed v1.2.0 and fixed 3 bugs.",
                8,
            )
            .await
            .unwrap();

        for i in 0..3 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now(),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Worker {i} completed successfully"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        for i in 0..3 {
            assert!(
                rendered.contains(&format!(
                    "- Worker completed: Worker {i} completed successfully"
                )),
                "every event renders in full on a normal day\n{rendered}"
            );
        }
        assert!(
            rendered.contains("Morning: deployed v1.2.0 and fixed 3 bugs."),
            "the synthesis renders in full when it fits\n{rendered}"
        );
        assert!(
            !rendered.contains("In digest:"),
            "a day that fits must not be digested\n{rendered}"
        );
        assert!(
            !rendered.contains(" covered · "),
            "a day that fits must not disclose coverage\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_render_bounds_a_pasted_brief_without_losing_coverage() {
        // Regression: a worker-spawn event whose summary was the whole task
        // brief — instructions, tool syntax and all — rendered verbatim and
        // bloated the block by paragraphs. The store keeps the brief; the render
        // bounds it, and the event still counts as covered.
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 400; // today budget = 240
        config.today_max_unsynthesized_events = 1;

        let brief =
            "Spawned a worker to bound rendered entries: read the file, edit, commit, push. "
                .repeat(40);

        // Older tail events: they overflow the day, so the disclosure appears.
        for i in 0..6 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now() - chrono::Duration::minutes(10 - i),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Worker {i} finished a short job"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        // The newest event carries the brief, so it keeps a full (bounded) line.
        let spawn = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::WorkerSpawned,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: brief.clone(),
            detail: None,
            importance: 0.7,
            day: today.clone(),
        };
        insert_event(&store.pool, &spawn).await.unwrap();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        let spawn_line = rendered
            .lines()
            .find(|line| line.contains("Worker spawned:"))
            .expect("the spawn event must render");
        assert!(
            spawn_line.chars().count() <= RENDERED_ENTRY_MAX_CHARS + 2,
            "a pasted brief must render as one bounded entry, got {} chars\n{spawn_line}",
            spawn_line.chars().count()
        );
        assert!(
            spawn_line.ends_with('…'),
            "the cut must be marked\n{spawn_line}"
        );
        assert!(
            !rendered.contains(&brief),
            "the brief must never be rendered verbatim"
        );

        // Nothing was dropped from the store.
        let stored = store.get_events_for_day(&today).await.unwrap();
        assert_eq!(stored.len(), 7);
        assert!(
            stored.iter().any(|event| event.summary == brief),
            "the store keeps the full text"
        );

        // Bounding is not dropping: the disclosure still accounts for every
        // event of the day, the bounded one included.
        let (covered, total, grains) = parse_coverage(&rendered).expect("coverage disclosed");
        assert_eq!(
            (covered, total),
            (7, 7),
            "the bounded event is still covered"
        );
        assert_eq!(
            grains.iter().map(|(count, _)| count).sum::<usize>(),
            7,
            "the grains must add up to the day\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_render_bounds_a_long_synthesis_without_losing_coverage() {
        // The synthesis side of the same bug: one paragraph of pasted narrative
        // typed out at length. The paragraph renders bounded; the store keeps it
        // whole; the event counts are untouched because truncation happens at
        // render time, after the grains are decided.
        let store = setup_test_store().await;
        let today = store.today();
        let mut config = test_config();
        config.context_token_budget = 400; // today budget = 240
        config.today_max_unsynthesized_events = 1;

        let long_summary =
            "Compiled the migration, audited every consumer, and pushed a green build. ".repeat(30);
        let start = Utc::now() - chrono::Duration::hours(3);
        let end = Utc::now() - chrono::Duration::hours(2);
        store
            .save_intraday_synthesis(&today, start, end, &long_summary, 8)
            .await
            .unwrap();

        for i in 0..6 {
            let event = WorkingMemoryEvent {
                id: Uuid::new_v4().to_string(),
                event_type: WorkingMemoryEventType::WorkerCompleted,
                timestamp: Utc::now() - chrono::Duration::minutes(10 - i),
                channel_id: Some("chan-1".to_string()),
                user_id: None,
                summary: format!("Tail event {i}"),
                detail: None,
                importance: 0.6,
                day: today.clone(),
            };
            insert_event(&store.pool, &event).await.unwrap();
        }

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        let paragraph = rendered
            .lines()
            .find(|line| line.starts_with('['))
            .expect("the synthesis paragraph must render");
        assert!(
            paragraph.chars().count() <= RENDERED_ENTRY_MAX_CHARS + 8,
            "a synthesis paragraph must stay bounded, got {} chars\n{paragraph}",
            paragraph.chars().count()
        );
        assert!(
            paragraph.ends_with('…'),
            "the cut must be marked\n{paragraph}"
        );
        assert!(
            !rendered.contains(&long_summary),
            "the full summary must not be rendered"
        );

        // The store still holds the whole thing — only the render is bounded.
        let stored = store.get_intraday_syntheses(&today).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].summary, long_summary);

        let (covered, total, grains) = parse_coverage(&rendered).expect("coverage disclosed");
        assert_eq!(
            (covered, total),
            (6, 6),
            "counts are independent of truncation"
        );
        assert_eq!(grains.iter().map(|(count, _)| count).sum::<usize>(), 6);
    }

    #[tokio::test]
    async fn test_render_leaves_a_short_entry_untouched() {
        // The bound is a backstop, not a haircut: an entry that fits renders
        // exactly as written, with no ellipsis marking a cut that never happened.
        let store = setup_test_store().await;
        let today = store.today();
        let config = test_config();

        let event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::WorkerCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Built the project".to_string(),
            detail: None,
            importance: 0.6,
            day: today.clone(),
        };
        insert_event(&store.pool, &event).await.unwrap();

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(
            rendered.contains("- Worker completed: Built the project"),
            "a healthy entry renders exactly as written\n{rendered}"
        );
        assert!(
            !rendered.contains('…'),
            "nothing was cut, so nothing may be marked as cut\n{rendered}"
        );
    }

    #[test]
    fn test_synthesis_digest_cuts_its_head_on_a_word_boundary() {
        // Regression: digest heads were cut at 80 chars with no ellipsis, so an
        // entry read `output-token audit, co ·` — a half word that reads as
        // corruption rather than as a trimmed summary.
        let summary = "discovery-propagation fix; output-token audit completed; \
                       worker spawn receipts verified across the whole day and more";
        let syntheses = vec![IntradaySynthesis {
            id: "s1".to_string(),
            day: "2026-10-12".to_string(),
            time_range_start: Utc::now(),
            time_range_end: Utc::now(),
            summary: summary.to_string(),
            event_count: 3,
            created_at: Utc::now(),
        }];

        let digest = render_synthesis_digest(&syntheses, &Tz::UTC);

        let head = digest
            .lines()
            .next()
            .and_then(|line| line.split_once("] "))
            .map(|(_, head)| head.to_string())
            .expect("the digest renders its head");
        assert!(head.ends_with('…'), "a cut head must be marked\n{digest}");

        let kept = head.trim_end_matches('…');
        assert!(kept.chars().count() < 80, "the head must stay bounded");
        assert!(
            summary.starts_with(kept),
            "the head is a prefix of the summary\n{digest}"
        );
        let rest = &summary[kept.len()..];
        assert!(
            rest.chars().next().is_some_and(char::is_whitespace),
            "the cut must land on a word boundary, rest started with {rest:?}"
        );
    }

    #[tokio::test]
    async fn test_render_working_memory_disabled() {
        let store = setup_test_store().await;
        let mut config = test_config();
        config.enabled = false;

        let rendered = render_working_memory(&store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        assert!(rendered.is_empty(), "should be empty when disabled");
    }

    #[tokio::test]
    async fn test_render_channel_activity_map_empty() {
        let store = setup_test_store().await;
        let config = test_config();

        let rendered = render_channel_activity_map(&store.pool, &store, "chan-1", &config, Tz::UTC)
            .await
            .unwrap();

        // No channels in DB, so should be empty.
        assert!(rendered.is_empty(), "should be empty with no channels");
    }

    #[tokio::test]
    async fn test_render_participant_context_uses_humans_and_recent_activity() {
        let store = setup_test_store().await;
        let config = crate::config::ParticipantContextConfig {
            max_participants: 3,
            ..crate::config::ParticipantContextConfig::default()
        };

        sqlx::query(
            "INSERT INTO conversation_messages \
             (id, channel_id, role, sender_name, sender_id, content) \
             VALUES (?, ?, 'user', ?, ?, ?)",
        )
        .bind("message-1")
        .bind("discord:chan-1")
        .bind("Victor")
        .bind("12345")
        .bind("Can you review this?")
        .execute(&store.pool)
        .await
        .unwrap();

        let event = WorkingMemoryEvent {
            id: Uuid::new_v4().to_string(),
            event_type: WorkingMemoryEventType::Decision,
            timestamp: Utc::now() - chrono::Duration::minutes(10),
            channel_id: Some("discord:chan-2".to_string()),
            user_id: Some("victor".to_string()),
            summary: "approved the migration approach".to_string(),
            detail: None,
            importance: 0.8,
            day: store.today(),
        };
        insert_event(&store.pool, &event).await.unwrap();

        let participants = vec![crate::conversation::ActiveParticipant {
            participant_key: "victor".to_string(),
            platform: "discord".to_string(),
            sender_id: "12345".to_string(),
            display_name: "Victor".to_string(),
            role: Some("Maintainer".to_string()),
            profile_summary: Some("Prefers direct, technical responses.".to_string()),
            last_message_at: Utc::now(),
        }];

        let rendered = render_participant_context(
            &store,
            &participants,
            "discord:chan-1",
            &config,
            &*crate::memory::MemoryStore::connect_in_memory().await,
            &tokio::sync::Mutex::new(std::collections::HashMap::new()),
        )
        .await
        .unwrap();

        assert!(rendered.contains("## Participants"));
        assert!(rendered.contains("**Victor** -- Maintainer"));
        assert!(rendered.contains("Prefers direct, technical responses."));
        assert!(rendered.contains("approved the migration approach"));
    }

    #[test]
    fn test_format_time_ago() {
        let now = Utc::now();
        assert_eq!(format_time_ago(now, now), "just now");
        assert_eq!(
            format_time_ago(now, now - chrono::Duration::minutes(5)),
            "5m ago"
        );
        assert_eq!(
            format_time_ago(now, now - chrono::Duration::hours(2)),
            "2h ago"
        );
        assert_eq!(
            format_time_ago(now, now - chrono::Duration::days(3)),
            "3d ago"
        );
    }

    #[test]
    fn test_format_event_line() {
        let event = WorkingMemoryEvent {
            id: "test".to_string(),
            event_type: WorkingMemoryEventType::WorkerCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Built the project".to_string(),
            detail: None,
            importance: 0.6,
            day: "2026-03-18".to_string(),
        };

        // Same channel — no prefix.
        let line = format_event_line(&event, "chan-1");
        assert_eq!(line, "Worker completed: Built the project");

        // Different channel — prefix with channel ID.
        let line = format_event_line(&event, "chan-2");
        assert_eq!(line, "[chan-1] Worker completed: Built the project");
    }

    /// The production rendering bug: a summary that already carries its own
    /// label must not be labelled twice.
    #[test]
    fn test_format_event_line_does_not_repeat_a_label_the_summary_carries() {
        let event = WorkingMemoryEvent {
            id: "test".to_string(),
            event_type: WorkingMemoryEventType::WorkerCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Worker completed: Done. 1b71b123 is on main.".to_string(),
            detail: None,
            importance: 0.6,
            day: "2026-03-18".to_string(),
        };

        assert_eq!(
            format_event_line(&event, "chan-1"),
            "Worker completed: Done. 1b71b123 is on main."
        );
        // The channel prefix still applies to the deduped line.
        assert_eq!(
            format_event_line(&event, "chan-2"),
            "[chan-1] Worker completed: Done. 1b71b123 is on main."
        );
    }

    /// The dedupe keys on the label, not on a word that happens to open the
    /// text: "Error while compiling" is a summary, not a stored label, so it
    /// keeps one.
    #[test]
    fn test_format_event_line_labels_a_summary_that_only_opens_like_its_label() {
        let event = WorkingMemoryEvent {
            id: "test".to_string(),
            event_type: WorkingMemoryEventType::Error,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: "Error while compiling".to_string(),
            detail: None,
            importance: 0.6,
            day: "2026-03-18".to_string(),
        };

        assert_eq!(
            format_event_line(&event, "chan-1"),
            "Error: Error while compiling"
        );
    }

    /// The dedupe runs before the bound, so a self-labelled summary spends the
    /// character budget on the text rather than on a repeated label.
    #[test]
    fn test_format_event_line_dedupes_before_bounding() {
        let event = WorkingMemoryEvent {
            id: "test".to_string(),
            event_type: WorkingMemoryEventType::WorkerCompleted,
            timestamp: Utc::now(),
            channel_id: Some("chan-1".to_string()),
            user_id: None,
            summary: format!("Worker completed: {}", "paste ".repeat(120)),
            detail: None,
            importance: 0.6,
            day: "2026-03-18".to_string(),
        };

        let line = format_event_line(&event, "chan-1");
        assert!(
            line.chars().count() <= RENDERED_ENTRY_MAX_CHARS,
            "a bounded entry must fit the budget, got {} chars",
            line.chars().count()
        );
        assert_eq!(
            line.matches("Worker completed:").count(),
            1,
            "the label must appear exactly once, got {line}"
        );
        assert!(line.ends_with('…'), "a cut line marks the cut, got {line}");
    }
}
