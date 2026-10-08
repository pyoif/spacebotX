//! Block segmentation for assembled prompts.
//!
//! A rendered system prompt is one string by the time it reaches a provider,
//! but it is assembled from named parts: identity files, fragment renders,
//! store queries, live state, and the template's own prose between them.
//! Those boundaries exist only during rendering, and they cannot be recovered
//! from the output — a fragment render carries no marker of where it began,
//! and template prose carries no heading at all.
//!
//! So they are recorded while they exist. Every injected value is wrapped in
//! sentinel characters before rendering; the render is then split on those
//! sentinels to recover exact byte ranges, and the sentinels are stripped.
//! Stripping is the only way the final text is produced, so the bytes a block
//! map describes are the bytes that were sent — see `segment`.

use serde::{Deserialize, Serialize};

/// Opens an injected value. Followed by the variable name, then `SEP`.
pub(crate) const OPEN: char = '\u{E000}';
/// Separates an injected value's name from its content.
pub(crate) const SEP: char = '\u{E001}';
/// Closes an injected value.
pub(crate) const CLOSE: char = '\u{E002}';

/// Which composition layer a block belongs to.
///
/// The vocabulary is the one already used to review prompts by hand, so a
/// block's layer means the same thing in the inspector as it does in a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockLayer {
    /// Operator-authored identity documents and learned identity memories.
    Identity,
    /// The template's own behavioral instructions.
    Contract,
    /// What the process can do this turn: skills, worker types, tools.
    Capabilities,
    /// Synthesized knowledge: memory store render, cortex synthesis.
    Knowledge,
    /// Recent activity: working memory, channel activity, participants.
    Working,
    /// Live state at send time: status block, conversation context, goals.
    Runtime,
}

/// How often a block's bytes are expected to change.
///
/// Drives the cache reading in the inspector. `Static` bytes must be identical
/// between two turns of the same process; `Epoch` bytes change only on a named
/// configuration event; `Volatile` bytes may change every turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockStability {
    Static,
    Epoch,
    Volatile,
}

/// Where a block's content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSource {
    /// Literal prose in the template itself.
    Template,
    /// An operator-authored file on disk (SOUL.md, IDENTITY.md, HUMAN.md).
    File,
    /// Rendered from a database query.
    Store,
    /// Written by an LLM process (cortex synthesis, chronicle).
    Synthesis,
    /// Read from process state at send time.
    LiveState,
    /// Derived from configuration.
    Config,
}

/// One named region of an assembled prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBlock {
    /// Template variable name, or `template:{name}` for the template's prose.
    pub id: String,
    pub layer: BlockLayer,
    pub stability: BlockStability,
    pub source: BlockSource,
    /// Byte offset of the block's first byte in the rendered prompt.
    pub start: usize,
    /// Byte offset one past the block's last byte.
    pub end: usize,
    /// Character count, which is what a reader compares against.
    pub chars: usize,
    /// Estimated tokens. The provider's usage total is authoritative for a
    /// request; this only apportions that total across blocks.
    pub tokens: usize,
}

impl PromptBlock {
    pub fn bytes(&self) -> usize {
        self.end - self.start
    }
}

/// A prompt assembled outside the template engine has no map, which is a
/// smaller claim than a wrong one — the text still renders whole.
impl From<String> for SegmentedPrompt {
    fn from(text: String) -> Self {
        Self {
            text,
            blocks: Vec::new(),
        }
    }
}

impl From<&str> for SegmentedPrompt {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

impl SegmentedPrompt {
    /// Adopt a version of this prompt with text appended, recording the
    /// appended region as a block of its own.
    ///
    /// The caller passes the whole replacement because the append helpers on
    /// `PromptEngine` return whole prompts. A replacement that is not an
    /// extension of the current text has rewritten mapped bytes, so the map is
    /// dropped rather than left describing content that moved.
    pub fn adopt_appended(&mut self, replacement: String, id: &str) {
        let start = self.text.len();

        // Appending to a prompt that was never mapped would produce a map that
        // starts partway through the text. Blocks would no longer tile it, and
        // the inspector would label the appended region while leaving the bytes
        // above it silently unaccounted for.
        if self.blocks.is_empty() && !self.text.is_empty() {
            self.text = replacement;
            return;
        }

        if !replacement.starts_with(self.text.as_str()) {
            tracing::warn!(
                block = id,
                "prompt append rewrote existing bytes; dropping the block map"
            );
            self.text = replacement;
            self.blocks.clear();
            return;
        }

        if replacement.len() == start {
            return;
        }

        let suffix = &replacement[start..];
        let chars = suffix.chars().count();
        let tokens = estimate_tokens(suffix);
        let end = replacement.len();

        let (layer, stability, source) = classify(id);
        self.blocks.push(PromptBlock {
            id: id.to_string(),
            layer,
            stability,
            source,
            start,
            end,
            chars,
            tokens,
        });
        self.text = replacement;
    }
}

/// A rendered prompt and the map of what it is made of.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentedPrompt {
    /// The prompt exactly as it will be sent.
    pub text: String,
    pub blocks: Vec<PromptBlock>,
}

impl SegmentedPrompt {
    /// Append a named section, separated by a blank line.
    ///
    /// Empty sections are skipped rather than recorded as zero-width blocks.
    pub fn append_section(&mut self, id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let separator = if self.text.is_empty() { "" } else { "\n\n" };
        self.adopt_appended(format!("{}{separator}{text}", self.text), id);
    }
}

/// Classify a template variable. Unknown names are reported so a new prompt
/// input shows up as an unclassified block rather than vanishing from the map.
fn classify(name: &str) -> (BlockLayer, BlockStability, BlockSource) {
    use BlockLayer::*;
    use BlockSource::*;
    use BlockStability::*;

    match name {
        "identity_context" => (Identity, Epoch, File),
        "execution_mode" | "authority" => (Contract, Epoch, Config),
        "adapter_prompt" => (Contract, Epoch, Config),
        "skills_prompt" => (Capabilities, Epoch, Config),
        "worker_capabilities" => (Capabilities, Epoch, Config),
        "available_channels" => (Capabilities, Volatile, Store),
        "org_context" | "link_context" => (Capabilities, Epoch, Store),
        "project_context" => (Capabilities, Epoch, Store),
        "knowledge_synthesis" => (Knowledge, Volatile, Synthesis),
        "working_memory" => (Working, Volatile, Synthesis),
        "channel_activity_map" => (Working, Volatile, Store),
        "participant_context" => (Working, Volatile, Store),
        // The chronicle is the channel prompt's cache anchor: it is append-only
        // within a session and rolls over only on a named event, so it belongs
        // in the cached prefix, not in the per-turn volatile region. Classified
        // Epoch (changes only on a named event) rather than Volatile — the
        // Volatile label is what previously allowed it to be emitted below the
        // per-turn blocks and lose its role as the prefix boundary.
        "session_chronicle" => (Working, Epoch, Synthesis),
        "backfill_transcript" => (Working, Epoch, Store),
        "active_goals" => (Runtime, Volatile, Store),
        "conversation_context" => (Runtime, Epoch, LiveState),
        "status_text" => (Runtime, Volatile, LiveState),
        // R5: the wall-clock line is the last block of the system prompt and
        // changes every second — always Volatile, always Runtime.
        "current_time_line" => (Runtime, Volatile, LiveState),
        "tool_use_enforcement" => (Contract, Epoch, Config),
        "required_skills" => (Capabilities, Epoch, Config),
        "agents_manifest" | "runtime_config_snapshot" => (Capabilities, Epoch, Config),
        "changelog_highlights" => (Knowledge, Epoch, Config),
        "channel_transcript" => (Working, Volatile, Store),
        "task_state" | "active_workers" => (Runtime, Volatile, Store),
        _ => (Runtime, Volatile, LiveState),
    }
}

/// Estimate tokens for a slice of prompt text.
///
/// Deliberately the same heuristic the compactor budgets with, so a block's
/// reported size and the budget that admitted it never disagree.
fn estimate_tokens(text: &str) -> usize {
    crate::agent::compactor::estimate_text_tokens(text)
}

/// True when a value would collide with the sentinels used to mark it.
///
/// Instrumentation is skipped entirely for such a render rather than editing
/// the value, because the captured bytes must be the sent bytes.
pub(crate) fn collides_with_sentinels(value: &str) -> bool {
    value.contains(OPEN) || value.contains(SEP) || value.contains(CLOSE)
}

/// Wrap a value so `segment` can find it in the render.
pub(crate) fn mark(name: &str, value: &str) -> String {
    format!("{OPEN}{name}{SEP}{value}{CLOSE}")
}

/// Split an instrumented render into the final text and its block map.
///
/// The returned text is the instrumented render with every sentinel removed,
/// which is byte-identical to rendering the same template with unmarked
/// values — asserted in `strip_matches_plain_render`.
pub fn segment(instrumented: &str, template_name: &str) -> SegmentedPrompt {
    let template_id = format!("template:{template_name}");
    let mut text = String::with_capacity(instrumented.len());
    let mut blocks: Vec<PromptBlock> = Vec::new();

    // Byte offset in `text` where the current run of template prose began.
    let mut literal_start = 0usize;
    let mut rest = instrumented;

    while let Some(open_at) = rest.find(OPEN) {
        // Everything before the marker is the template's own prose.
        text.push_str(&rest[..open_at]);
        rest = &rest[open_at + OPEN.len_utf8()..];

        let Some(sep_at) = rest.find(SEP) else {
            // Malformed marker: keep the remaining bytes verbatim so the text
            // stays faithful, and stop mapping.
            text.push_str(rest);
            rest = "";
            break;
        };
        let name = &rest[..sep_at];
        rest = &rest[sep_at + SEP.len_utf8()..];

        let Some(close_at) = rest.find(CLOSE) else {
            text.push_str(rest);
            rest = "";
            break;
        };
        let content = &rest[..close_at];
        rest = &rest[close_at + CLOSE.len_utf8()..];

        // An empty injection contributes no bytes and gets no block; the
        // template's `{%- if %}` guard already elided its surrounding prose.
        if content.is_empty() {
            continue;
        }

        push_literal(&mut blocks, &text, literal_start, &template_id);

        let start = text.len();
        text.push_str(content);
        let (layer, stability, source) = classify(name);
        blocks.push(PromptBlock {
            id: name.to_string(),
            layer,
            stability,
            source,
            start,
            end: text.len(),
            chars: content.chars().count(),
            tokens: estimate_tokens(content),
        });
        literal_start = text.len();
    }

    text.push_str(rest);
    push_literal(&mut blocks, &text, literal_start, &template_id);

    SegmentedPrompt { text, blocks }
}

/// Record the run of template prose ending at the current end of `text`.
///
/// Runs that are only whitespace are kept. They carry no meaning to a reader,
/// but keeping them means the blocks tile the prompt exactly — every byte
/// belongs to one block — so a size map over them is accurate rather than
/// approximate.
fn push_literal(blocks: &mut Vec<PromptBlock>, text: &str, start: usize, template_id: &str) {
    if text.len() <= start {
        return;
    }
    let content = &text[start..];
    blocks.push(PromptBlock {
        id: template_id.to_string(),
        layer: BlockLayer::Contract,
        stability: BlockStability::Static,
        source: BlockSource::Template,
        start,
        end: text.len(),
        chars: content.chars().count(),
        tokens: estimate_tokens(content),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_marked_values_and_template_prose() {
        let instrumented = format!(
            "# Header\n\n{}\n\nMiddle prose\n\n{}\n",
            mark("identity_context", "I am Orion."),
            mark("status_text", "Nothing running."),
        );

        let segmented = segment(&instrumented, "channel");

        assert_eq!(
            segmented.text,
            "# Header\n\nI am Orion.\n\nMiddle prose\n\nNothing running.\n"
        );

        let ids: Vec<&str> = segmented.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "template:channel",
                "identity_context",
                "template:channel",
                "status_text",
                "template:channel",
            ]
        );

        // Every block's range must address its own bytes in the final text.
        for block in &segmented.blocks {
            assert_eq!(block.bytes(), block.end - block.start);
            assert!(block.end <= segmented.text.len());
        }
        let identity = &segmented.blocks[1];
        assert_eq!(&segmented.text[identity.start..identity.end], "I am Orion.");
    }

    #[test]
    fn empty_injection_contributes_no_block() {
        let instrumented = format!("A{}B", mark("status_text", ""));
        let segmented = segment(&instrumented, "channel");

        assert_eq!(segmented.text, "AB");
        assert_eq!(segmented.blocks.len(), 1);
        assert_eq!(segmented.blocks[0].id, "template:channel");
    }

    #[test]
    fn ranges_are_byte_exact_for_multibyte_content() {
        let instrumented = format!("→ {} ←", mark("identity_context", "héllo 🌍"));
        let segmented = segment(&instrumented, "channel");

        assert_eq!(segmented.text, "→ héllo 🌍 ←");
        let block = segmented
            .blocks
            .iter()
            .find(|b| b.id == "identity_context")
            .expect("identity block");
        assert_eq!(&segmented.text[block.start..block.end], "héllo 🌍");
        assert_eq!(block.chars, "héllo 🌍".chars().count());
    }

    #[test]
    fn blocks_tile_the_prompt_without_gaps() {
        let instrumented = format!(
            "{}\n\n{}",
            mark("identity_context", "a"),
            mark("status_text", "b")
        );
        let segmented = segment(&instrumented, "channel");

        assert_eq!(segmented.text, "a\n\nb");
        let ids: Vec<&str> = segmented.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["identity_context", "template:channel", "status_text"]);

        let mut cursor = 0usize;
        for block in &segmented.blocks {
            assert_eq!(block.start, cursor);
            cursor = block.end;
        }
        assert_eq!(cursor, segmented.text.len());
    }

    #[test]
    fn repeated_variable_yields_one_block_per_occurrence() {
        let instrumented = format!("{} x {}", mark("authority", "A"), mark("authority", "A"));
        let segmented = segment(&instrumented, "channel");

        assert_eq!(segmented.text, "A x A");
        assert_eq!(
            segmented
                .blocks
                .iter()
                .filter(|b| b.id == "authority")
                .count(),
            2
        );
    }

    /// A prompt with no map must stay unmapped rather than gain a map that
    /// starts partway through it — blocks that do not tile the text make every
    /// range and proportion downstream describe the wrong bytes.
    #[test]
    fn appending_to_an_unmapped_prompt_records_no_block() {
        let mut prompt = SegmentedPrompt::from("assembled elsewhere".to_string());
        prompt.append_section("skills_prompt", "## Available Skills");

        assert_eq!(prompt.text, "assembled elsewhere\n\n## Available Skills");
        assert!(
            prompt.blocks.is_empty(),
            "a partial map is worse than no map"
        );
    }

    #[test]
    fn appending_after_a_dropped_map_records_no_block() {
        let mut prompt = segment(&mark("identity_context", "a"), "channel");
        assert_eq!(prompt.blocks.len(), 1);

        // A replacement that rewrites mapped bytes drops the map.
        prompt.adopt_appended("rewritten".to_string(), "tool_use_enforcement");
        assert!(prompt.blocks.is_empty());

        prompt.append_section("skills_prompt", "## Available Skills");
        assert!(prompt.blocks.is_empty(), "the map must stay dropped");
    }

    #[test]
    fn detects_sentinel_collision() {
        assert!(collides_with_sentinels(&format!("text{OPEN}")));
        assert!(!collides_with_sentinels("ordinary prompt text"));
    }
}
