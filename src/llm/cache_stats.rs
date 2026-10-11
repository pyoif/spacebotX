//! Cache-usage logging (R7-lite): measure provider prompt-cache behaviour.
//!
//! This is measurement only. It never touches prompt layout, request
//! assembly, or the order anything is sent in — an audit of the cache
//! optimisation work needs data, and taking the data must not itself be able
//! to perturb what it measures.
//!
//! Three pieces:
//!
//! 1. [`CacheUsage`] normalizes the cache fields of every provider we speak
//!    into one `input` / `cached` / `output` triple, where `input` is the
//!    whole prompt (not just its uncached part), so `fresh = input - cached`
//!    is the number of tokens the provider actually had to read.
//! 2. [`tag`] records a mutation event — a memory write, a chronicle write, a
//!    task transition, a worker spawn. These events rewrite bytes inside the
//!    cached prefix, so a dip in hit rate cannot be attributed without them.
//!    Events accumulate and are attached to the *next* recorded request.
//! 3. [`record`] appends one JSON line per LLM call to
//!    `<data_dir>/cache_stats.jsonl`, carrying the usage triple, the gap since
//!    the previous request on the same surface, and the events that landed in
//!    between.
//!
//! Why the noise floor: a provider reports a cache miss for a handful of
//! tokens constantly (the tail of the epilogue, a rounding at the boundary).
//! Only a fresh count at or above [`FRESH_NOISE_FLOOR`] is treated as a real
//! miss.
//!
//! Why the gap: a full miss after a long idle gap is expected (entries
//! expire); a full miss seconds after a hit is a provider-side artifact we
//! have seen (upstream rotation). Separating the two by time is the whole
//! point of logging the gap.
//!
//! Only successful responses are logged. A failed request reports no usage at
//! all, and a line with `input: null` is indistinguishable at a glance from a
//! provider that genuinely stopped reporting — the confusion this log exists
//! to remove.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Fresh (uncached) tokens below this are provider noise, not a real miss.
pub const FRESH_NOISE_FLOOR: u64 = 1024;

/// The log file's name, relative to the agent data directory.
pub const LOG_FILE_NAME: &str = "cache_stats.jsonl";

/// Cap on the live log before it is rotated to `<name>.1`.
///
/// One rotated generation is kept: enough to inspect the tail of yesterday's
/// data after a rotation, without the log ever growing unbounded.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// A mutation that rewrites bytes inside the provider's cached prefix.
///
/// Each of these changes a block that sits *above* the conversation history —
/// the memory store, the chronicle anchor, the task list — or appends to the
/// history itself, so each one is a candidate explanation for a cache dip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEvent {
    /// A memory was written to the store (rewrites the store block).
    MemoryWrite,
    /// A chronicle checkpoint was committed (rewrites the chronicle anchor).
    ChronicleWrite,
    /// A task's status changed (rewrites the active-tasks block).
    TaskTransition,
    /// A worker was admitted (the channel's own prompt records the spawn).
    WorkerSpawn,
}

impl CacheEvent {
    /// The label this event carries in the log.
    pub fn label(self) -> &'static str {
        match self {
            CacheEvent::MemoryWrite => "memory_write",
            CacheEvent::ChronicleWrite => "chronicle_write",
            CacheEvent::TaskTransition => "task_transition",
            CacheEvent::WorkerSpawn => "worker_spawn",
        }
    }
}

/// Read a token count defensively.
///
/// Missing and unparseable both yield `None` — "unknown", never zero. A zero
/// is a claim that the provider reported no tokens, and conflating the two
/// would make an unmeasured request look like a measured cache miss.
fn token_count(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
}

/// The cache-relevant part of a provider's usage block.
///
/// `input` is the total prompt size in tokens, `cached` the portion the
/// provider served from its prefix cache, and `output` the completion. Every
/// field is optional because a provider may simply not report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub output: Option<u64>,
}

impl CacheUsage {
    /// Nothing known — the shape for a provider that reported no usage.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Tokens the provider had to read rather than serve from cache.
    ///
    /// Unknown when either half is unknown: a difference needs both terms, and
    /// guessing the missing one would invent a hit rate.
    pub fn fresh(&self) -> Option<u64> {
        match (self.input, self.cached) {
            (Some(input), Some(cached)) => Some(input.saturating_sub(cached)),
            _ => None,
        }
    }

    /// Whether the uncached portion exceeds the noise floor.
    ///
    /// An unknown fresh count is *not* over the floor — an unmeasured request
    /// must never be counted as a real miss.
    pub fn fresh_over_floor(&self) -> bool {
        self.fresh().is_some_and(|fresh| fresh >= FRESH_NOISE_FLOOR)
    }

    /// Parse an OpenAI-compatible usage block.
    ///
    /// Covers DeepSeek (`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`),
    /// GLM and any OpenAI-compatible server (`prompt_tokens_details.cached_tokens`),
    /// and falls back to deriving one field from the others when a provider
    /// reports only a subset.
    pub fn from_openai_body(body: &serde_json::Value) -> Self {
        let usage = &body["usage"];

        let hit = token_count(usage.get("prompt_cache_hit_tokens"));
        let miss = token_count(usage.get("prompt_cache_miss_tokens"));

        let mut input = token_count(usage.get("prompt_tokens"));
        if input.is_none()
            && let (Some(hit), Some(miss)) = (hit, miss)
        {
            // DeepSeek's two halves are the whole prompt when the total is absent.
            input = Some(hit + miss);
        }

        let mut cached =
            hit.or_else(|| token_count(usage.pointer("/prompt_tokens_details/cached_tokens")));
        if cached.is_none()
            && let (Some(input), Some(miss)) = (input, miss)
        {
            cached = Some(input.saturating_sub(miss));
        }

        Self {
            input,
            cached,
            output: token_count(usage.get("completion_tokens")),
        }
    }

    /// Parse an Anthropic usage block.
    ///
    /// Anthropic splits the prompt across `input_tokens` (the part it read),
    /// `cache_read_input_tokens` and `cache_creation_input_tokens`, so the
    /// total is their sum and only the read half was served from cache.
    pub fn from_anthropic_body(body: &serde_json::Value) -> Self {
        let usage = &body["usage"];

        let uncached = token_count(usage.get("input_tokens"));
        let read = token_count(usage.get("cache_read_input_tokens"));
        let written = token_count(usage.get("cache_creation_input_tokens"));

        let input = if uncached.is_none() && read.is_none() && written.is_none() {
            None
        } else {
            Some(uncached.unwrap_or(0) + read.unwrap_or(0) + written.unwrap_or(0))
        };

        Self {
            input,
            cached: read,
            output: token_count(usage.get("output_tokens")),
        }
    }

    /// Parse whichever shape the named provider uses.
    pub fn from_body(provider: &str, body: &serde_json::Value) -> Self {
        if provider == "anthropic" {
            Self::from_anthropic_body(body)
        } else {
            Self::from_openai_body(body)
        }
    }
}

/// One logged LLM call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatsLine {
    pub ts: DateTime<Utc>,
    /// Which process sent the request: channel, worker, branch, cortex, …
    pub surface: String,
    pub model: String,
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub output: Option<u64>,
    /// `input - cached`, when both are known.
    pub fresh: Option<u64>,
    /// Seconds since the previous request on this surface; absent for the first.
    pub gap_secs: Option<u64>,
    /// Mutations that landed since the previous request on this surface,
    /// as label → count. Empty when nothing changed.
    pub events: BTreeMap<String, u32>,
    /// Whether `fresh` is at or above [`FRESH_NOISE_FLOOR`].
    pub fresh_over_floor: bool,
}

impl CacheStatsLine {
    /// Serialize to a single line, without a trailing newline.
    ///
    /// `serde_json::to_string` never emits a raw newline, which is what makes
    /// one-record-per-line safe even if a field ever carries odd bytes.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Events and per-surface timing between requests.
///
/// Held behind a process-global mutex because the mutation sites are all over
/// the codebase and threading a handle to each of them would be a far larger
/// change than the measurement is worth.
#[derive(Debug, Default)]
struct Registry {
    pending: BTreeMap<String, u32>,
    last_request: HashMap<String, DateTime<Utc>>,
}

impl Registry {
    /// Note that `label` happened, to be attributed to the next request.
    fn tag(&mut self, label: &str) {
        *self.pending.entry(label.to_string()).or_insert(0) += 1;
    }

    /// Take the pending events and report the gap since this surface's last
    /// request, then remember this request as the newest.
    fn observe(
        &mut self,
        surface: &str,
        now: DateTime<Utc>,
    ) -> (BTreeMap<String, u32>, Option<u64>) {
        let events = std::mem::take(&mut self.pending);
        let gap = self
            .last_request
            .insert(surface.to_string(), now)
            // A clock that stepped backwards would otherwise overflow the cast.
            .map(|previous| (now - previous).num_seconds().max(0) as u64);
        (events, gap)
    }
}

/// The registry, created on the first [`init`].
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// Where the log lives. Absent means logging is off and every call is a no-op.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Serializes appends so two concurrent writes cannot interleave or race the
/// rotation check.
static WRITE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Start logging into `data_dir`, returning whether this call enabled it.
///
/// Deliberately infallible and idempotent: the first call for the process wins
/// and later ones are ignored, so a second agent sharing a process cannot
/// redirect the first one's log. A failure to open the file happens per
/// append and is logged, never surfaced — measurement must not be able to fail
/// a turn.
pub fn init(data_dir: &Path) -> bool {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()));
    LOG_PATH.set(data_dir.join(LOG_FILE_NAME)).is_ok()
}

/// Whether logging has been enabled for this process.
pub fn is_enabled() -> bool {
    LOG_PATH.get().is_some()
}

/// Record that something rewrote the cached prefix.
///
/// A free function because the alternative — plumbing a handle from boot to
/// four unrelated subsystems — would dwarf the measurement it carries. It is a
/// no-op until [`init`] runs, so nothing accumulates in memory before logging
/// is on.
pub fn tag(event: CacheEvent) {
    if !is_enabled() {
        return;
    }
    let registry = REGISTRY.get_or_init(|| Mutex::new(Registry::default()));
    if let Ok(mut registry) = registry.lock() {
        registry.tag(event.label());
    }
}

/// Log one completed LLM request.
///
/// The write is spawned, never awaited: it is a diagnostic, and it must not sit
/// in front of a turn. Returns immediately when logging is off.
pub fn record(surface: &str, model: &str, usage: CacheUsage) {
    let Some(path) = LOG_PATH.get().cloned() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    let now = Utc::now();
    let (events, gap_secs) = REGISTRY
        .get()
        .and_then(|registry| {
            registry
                .lock()
                .ok()
                .map(|mut held| held.observe(surface, now))
        })
        .unwrap_or_default();

    let line = CacheStatsLine {
        ts: now,
        surface: surface.to_string(),
        model: model.to_string(),
        input: usage.input,
        cached: usage.cached,
        output: usage.output,
        fresh: usage.fresh(),
        gap_secs,
        events,
        fresh_over_floor: usage.fresh_over_floor(),
    };

    handle.spawn(async move { append_line(&path, &line).await });
}

/// Append one line, rotating the file first when it has reached its cap.
async fn append_line(path: &Path, line: &CacheStatsLine) {
    let payload = match line.to_jsonl() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(%error, "failed to serialize cache stats line");
            return;
        }
    };

    let lock = WRITE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;

    if let Err(error) = rotate_if_full(path).await {
        tracing::warn!(path = %path.display(), %error, "failed to rotate cache stats log");
        return;
    }

    let mut body = payload;
    body.push('\n');
    if let Err(error) = write_append(path, body.as_bytes()).await {
        tracing::warn!(path = %path.display(), %error, "failed to append cache stats line");
    }
}

/// Move the log aside once it reaches [`MAX_LOG_BYTES`].
///
/// The rotated file replaces any previous one, so disk use is bounded at twice
/// the cap no matter how long the process runs.
async fn rotate_if_full(path: &Path) -> std::io::Result<()> {
    let length = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if length < MAX_LOG_BYTES {
        return Ok(());
    }

    let rotated = path.with_extension(format!("{}.1", file_extension(path)));
    // `rename` over an existing file is atomic on the platforms this runs on.
    tokio::fs::rename(path, rotated).await
}

/// The extension of `path` as a string, without the leading dot.
fn file_extension(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_else(|| "log".to_string())
}

/// Open for append, creating the file and its parent if needed.
async fn write_append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
    tokio::io::AsyncWriteExt::flush(&mut file).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_usage_reports_total_and_cached() {
        let body = serde_json::json!({
            "usage": {
                "prompt_tokens": 61000,
                "prompt_cache_hit_tokens": 60400,
                "prompt_cache_miss_tokens": 600,
                "completion_tokens": 120
            }
        });

        let usage = CacheUsage::from_openai_body(&body);
        assert_eq!(usage.input, Some(61000));
        assert_eq!(usage.cached, Some(60400));
        assert_eq!(usage.output, Some(120));
        assert_eq!(usage.fresh(), Some(600));
        assert!(!usage.fresh_over_floor());
    }

    #[test]
    fn glm_usage_reads_nested_cached_tokens() {
        let body = serde_json::json!({
            "usage": {
                "prompt_tokens": 90000,
                "completion_tokens": 300,
                "prompt_tokens_details": { "cached_tokens": 88000 }
            }
        });

        let usage = CacheUsage::from_openai_body(&body);
        assert_eq!(usage.input, Some(90000));
        assert_eq!(usage.cached, Some(88000));
        assert_eq!(usage.fresh(), Some(2000));
        assert!(usage.fresh_over_floor());
    }

    #[test]
    fn missing_fields_are_unknown_not_zero() {
        // A provider that reports nothing at all.
        let empty = CacheUsage::from_openai_body(&serde_json::json!({}));
        assert_eq!(empty, CacheUsage::unknown());
        assert_eq!(empty.fresh(), None);
        assert!(
            !empty.fresh_over_floor(),
            "an unmeasured request must not count as a real miss"
        );

        // A total with no cache breakdown: the input is known, the split is not.
        let partial = CacheUsage::from_openai_body(&serde_json::json!({
            "usage": { "prompt_tokens": 5000, "completion_tokens": 10 }
        }));
        assert_eq!(partial.input, Some(5000));
        assert_eq!(partial.cached, None);
        assert_eq!(
            partial.fresh(),
            None,
            "a difference needs both terms rather than an assumed zero"
        );
    }

    #[test]
    fn deepseek_miss_field_derives_the_cached_half() {
        // `prompt_cache_hit_tokens` absent, but the miss half plus the total
        // are enough to recover how much was cached.
        let body = serde_json::json!({
            "usage": { "prompt_tokens": 10000, "prompt_cache_miss_tokens": 250 }
        });

        let usage = CacheUsage::from_openai_body(&body);
        assert_eq!(usage.cached, Some(9750));
        assert_eq!(usage.fresh(), Some(250));
    }

    #[test]
    fn anthropic_prompt_total_is_the_sum_of_its_parts() {
        let body = serde_json::json!({
            "usage": {
                "input_tokens": 400,
                "cache_read_input_tokens": 30000,
                "cache_creation_input_tokens": 2000,
                "output_tokens": 90
            }
        });

        let usage = CacheUsage::from_anthropic_body(&body);
        assert_eq!(usage.input, Some(32400));
        assert_eq!(usage.cached, Some(30000));
        assert_eq!(usage.fresh(), Some(2400));
        assert_eq!(usage.output, Some(90));
    }

    #[test]
    fn from_body_dispatches_on_the_provider_label() {
        let openai = serde_json::json!({
            "usage": { "prompt_tokens": 1000, "prompt_cache_hit_tokens": 900 }
        });
        let anthropic = serde_json::json!({
            "usage": { "input_tokens": 100, "cache_read_input_tokens": 900 }
        });

        assert_eq!(CacheUsage::from_body("openai", &openai).cached, Some(900));
        assert_eq!(
            CacheUsage::from_body("anthropic", &anthropic).cached,
            Some(900)
        );
        assert_eq!(
            CacheUsage::from_body("anthropic", &anthropic).input,
            Some(1000)
        );
    }

    #[test]
    fn registry_counts_events_and_hands_them_to_the_next_request() {
        let mut registry = Registry::default();
        let start = Utc::now();

        registry.tag(CacheEvent::MemoryWrite.label());
        registry.tag(CacheEvent::MemoryWrite.label());
        registry.tag(CacheEvent::WorkerSpawn.label());

        let (events, gap) = registry.observe("channel", start);
        assert_eq!(events.get("memory_write"), Some(&2));
        assert_eq!(events.get("worker_spawn"), Some(&1));
        assert_eq!(gap, None, "the first request on a surface has no gap");

        // Events are drained, not remembered.
        let (events, gap) = registry.observe("channel", start + chrono::Duration::seconds(45));
        assert!(events.is_empty());
        assert_eq!(gap, Some(45));
    }

    #[test]
    fn registry_tracks_gaps_per_surface() {
        let mut registry = Registry::default();
        let start = Utc::now();

        registry.observe("channel", start);
        registry.observe("worker", start + chrono::Duration::seconds(10));

        // A worker's first request is not measured against the channel's.
        let (_, worker_gap) = registry.observe("worker", start + chrono::Duration::seconds(30));
        assert_eq!(worker_gap, Some(20));
    }

    #[test]
    fn jsonl_line_is_one_line_and_round_trips() {
        let line = CacheStatsLine {
            ts: Utc::now(),
            surface: "channel".to_string(),
            model: "deepseek/deepseek-v4.1-flash".to_string(),
            input: Some(61000),
            cached: Some(60400),
            output: Some(120),
            fresh: Some(600),
            gap_secs: Some(7),
            events: BTreeMap::from([("memory_write".to_string(), 3)]),
            fresh_over_floor: false,
        };

        let jsonl = line.to_jsonl().expect("serialize");
        assert!(!jsonl.contains('\n'), "a record must never span two lines");

        let parsed: CacheStatsLine = serde_json::from_str(&jsonl).expect("parse");
        assert_eq!(parsed, line);
    }

    #[test]
    fn fresh_over_floor_uses_the_noise_floor_inclusively() {
        let at_floor = CacheUsage {
            input: Some(2000),
            cached: Some(2000 - FRESH_NOISE_FLOOR),
            output: None,
        };
        let under = CacheUsage {
            input: Some(2000),
            cached: Some(2000 - FRESH_NOISE_FLOOR + 1),
            output: None,
        };

        assert!(at_floor.fresh_over_floor());
        assert!(!under.fresh_over_floor());
    }

    #[tokio::test]
    async fn append_line_writes_one_line_per_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOG_FILE_NAME);

        for index in 0..3 {
            let line = CacheStatsLine {
                ts: Utc::now(),
                surface: "channel".to_string(),
                model: "m".to_string(),
                input: Some(1000 + index),
                cached: Some(900),
                output: Some(5),
                fresh: Some(100 + index),
                gap_secs: None,
                events: BTreeMap::new(),
                fresh_over_floor: false,
            };
            append_line(&path, &line).await;
        }

        let contents = tokio::fs::read_to_string(&path).await.expect("read");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            let parsed: CacheStatsLine = serde_json::from_str(line).expect("parse");
            assert_eq!(parsed.surface, "channel");
        }
    }
}
