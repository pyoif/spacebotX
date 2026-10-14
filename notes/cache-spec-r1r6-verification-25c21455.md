# R1–R6 verification audit — HEAD 25c21455 (line-level, source-verified)

Scope: verify the cache-optimization chain (b21af4b2 → 2dc36a4f → d4212bf6 → c1315028/8f8c4345/25c21455)
implements the R1–R6 spec from `research/pi-opencode/cache-optimization-report.md`.
Method: source reads only, no cargo. Block ORDER was already established by prior audits; this pass
verifies the remaining claims line-level.

## Verdict table

| Rule | Verdict | Evidence |
|---|---|---|
| **R1** volatile blocks last | **IMPLEMENTED** | `prompts/en/channel.md.j2`: emission order is available_channels/org/link/project (epoch) → session_chronicle → backfill_transcript (accruing) → knowledge_synthesis → working_memory → channel_activity_map → participant_context → active_goals → active_tasks → conversation_context (volatile) → status_text → current_time_line. All per-turn blocks sit after the chronicle. `classify()` in `src/prompts/blocks.rs:196-239` labels each. |
| **R2** working memory out of the cached prefix | **PARTIAL (as-designed tail)** | Working memory/status are still blocks IN the system prompt, but emitted at the tail (`channel.md.j2` volatile region + `## Current Status` last). The spec's preferred form is append-as-message. Implementation instead keeps them in-prompt *below every cacheable byte*, so the cacheable prefix is unaffected. `build_system_prompt_segmented()` `src/agent/channel.rs:3333-3485`; the per-turn time line is appended **after** everything as the final bytes (`:3478-3483`). |
| **R3** memory store append-only | **PARTIAL (deterministic, but sorted re-render)** | `src/memory/render.rs:render_memory_store`. Re-renders the whole block each time, with a deterministic sort (importance desc → updated_at desc → id). Byte-stable BETWEEN writes, but a new high-importance entry can insert near the TOP of a section and shift all following bytes. Also has per-type caps (Facts 10, Decisions 10, …) and a word budget that can drop entries — so it is a bounded sorted view, not a strict append log. Cache-friendly between memory writers; not strictly append-only on insertion. |
| **R4** tool schemas byte-stable | **IMPLEMENTED** | `stable_tool_definitions()` `src/llm/model.rs:45-51` name-sorts. Called on all OpenAI paths (`model.rs:1485,1594,1718,2032,2128`) and Anthropic (`src/llm/anthropic/params.rs:169`). Property-key order deterministic: `serde_json::Map` is BTreeMap because `preserve_order` is OFF (serde_json in `Cargo.lock` has no `indexmap` dep; only `itoa/memchr/serde/serde_core/zmij`). `json!({...})` literals therefore emit sorted keys → byte-stable. |
| **R5** no volatile artifacts in stable region | **IMPLEMENTED** | Wall-clock is the very last block (`channel.rs:3478-3483`, section id `current_time_line`), appended after tool-use enforcement. Stable-region builders (identity/capabilities/skills) contain no timestamp/date/random-id injection (grep clean). Block rendering uses deterministic structures; `classify()` default is Volatile but emitted in the right region regardless. |
| **R6** prior turns byte-identical | **IMPLEMENTED** | History is `Arc<RwLock<Vec<rig::message::Message>>>` (`channel.rs:476`) mutated only via `push()` (append) at `:543,2370,2769,2857,2928,3674,3691,4499`. Trimming is front-only: `history.drain(..remove_count)` (`src/agent/compactor.rs:232,284,494`) — drops oldest, never rewrites middle/tail bytes. |

## Completeness by rule
R1 ✅, R2 ⚠️ partial-by-design (tail-in-prompt, not message-append), R3 ⚠️ partial (deterministic sorted
re-render, bounded+evicting, not strict append), R4 ✅, R5 ✅, R6 ✅.

## Single biggest remaining gap
**R3** — the memory-store block is a bounded, sorted, re-rendered view. Between memory writes it is
byte-stable (good), but each *write* re-renders the section and can shift bytes below the insertion
point, so the cacheable prefix past that block breaks on every memory write rather than only on an
append. It is already split from the task board (c1315028) so task churn no longer touches it.
Closing it fully would require an append-ordered render (newest appended at the end, no mid-section
insert), which conflicts with the current importance-first presentation — a design trade-off, not a bug.

## Notes
- `available_channels` is classified `Volatile` (`blocks.rs:207`) though it changes only on config
  events; metadata-only imprecision, emitted in the correct region.
- The per-turn time line rides BOTH the system-prompt tail and the user-message envelope
  (`with_time_envelope`, `channel.rs:2466,2736`) — the envelope copy costs no cache (it is new bytes
  every turn); the prompt-tail copy is the R5-sanctioned position.
