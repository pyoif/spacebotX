# Diagnosis: "Context: 128k tokens" still shown after the precedence flip (`7e33c0cb`)

Repo: `/data/agents/main/workspace/spacebotX`
HEAD at time of diagnosis: `075a0cfd` (local == `origin/main`, verified with `git ls-remote`)
Method: read-only (`git show`, `grep`, `sed`). No commits, no edits, no cargo run.

Config fact from chi: **no `context_window` override in TOML** — so `context_window_explicit == false`
and, post-flip, discovery is supposed to outrank the built-in default.

---

## Q1 — Where "Context: 128k tokens" gets its number

**The config/fallback value. Not the resolved per-provider ceiling.**

| Step | Location |
|---|---|
| Rendered into the status block | `src/agent/status.rs:641` — `format!("{}k tokens", info.context_window / 1000)` (line `:121` is the worker variant) |
| `SystemInfo.context_window` is filled from | `src/agent/status.rs:101` — `context_window: **rc.context_window.load()` |
| `RuntimeConfig::context_window` declared | `src/config/runtime.rs:36` — `pub context_window: ArcSwap<usize>` |
| written from | `src/config/runtime.rs:246` — `self.context_window.store(Arc::new(resolved.context_window))` |
| `resolved.context_window` derives from | `src/config/types.rs:1803` — `self.context_window.unwrap_or(defaults.context_window)` |
| built-in default | `src/config/types.rs:1732` — `context_window: 128_000` |

It never calls `LlmManager::context_ceiling()` (`src/llm/manager.rs:288`), which is the only thing that
knows a discovered window.

**Verdict: cosmetic.** The line prints the same 128k on every build, pre- and post-flip. It is
structurally incapable of showing a discovered 1M. The same config number is also surfaced at
`src/api/config.rs:369` and `src/self_awareness.rs:181`.

The *behavioral* number lives elsewhere: `src/llm/model.rs:615` (`enforce_context_ceiling`), which reads
the resolved ceiling and logs it as the `ceiling=` field of the trim warning (`src/llm/model.rs:675`).

---

## Q2 — Does the `je/` prefix break discovery?

**No. The resolver never sees the prefix.**

| Step | Location |
|---|---|
| `SpacebotModel::make` splits the routing id on the first `/` | `src/llm/model.rs:1160-1165` → `provider = "je"`, `model_name = "deepseek-v4.1-flash"`, `full_model_name = "je/deepseek-v4.1-flash"` |
| resolver is handed **`&self.model_name`** (bare id) | `src/llm/model.rs:785-791` |
| result is stored under `full_model_name` | `src/llm/model.rs:803` — `note_resolved_context_window(&self.full_model_name, window)` |
| memo check, same key | `src/llm/model.rs:765` |
| enforcement reads, same key | `src/llm/model.rs:615` |

Keying is internally consistent: bare id in, `full_model_name` out, `full_model_name` read back.

The ladder resolves exactly this bare id — pinned by an existing test:
`src/llm/context_window.rs:781-788` asserts `resolve_from_models_dev("deepseek-v4.1-flash", …)` →
`deepseek/deepseek-v4.1-flash` @ `1_000_000` via `IdMatch::VendorPrefix`.

**Verdict: the "je" prefix cannot cause a mapping miss.** The earlier worry (that verification tested the
function in isolation rather than the je-specific path) is resolved: the je-specific path passes the bare
model name, which is the case the test covers.

### Gates that can still suppress discovery at runtime (all unverified without daemon logs / TOML)

1. `src/llm/model.rs:774-779` — if the je provider's `api_type` is `Anthropic`, the function returns
   immediately and **no discovery is attempted at all**. Enum at `src/config/types.rs:206-218`. Check the
   je provider block in the TOML.
2. `src/llm/model.rs:779` — needs `models_dev_cache_path()`, i.e. a set `instance_dir`
   (`src/llm/manager.rs:325-329`). `src/main.rs:1035` passes `config.instance_dir`, so this is satisfied in
   normal operation.
3. **Endpoint outranks models.dev** — `src/llm/context_window.rs:464-469`: the provider's `/models` listing
   is consulted first and its answer wins. If je's listing advertises a small window for this id, the
   resulting 128k is the *honest* answer under the new precedence, not a bug.
4. Memoisation — `src/llm/model.rs:764-769`: one answer per model per process; a wrong first answer sticks.
5. **Ordering lag**: inside `attempt_completion` (`src/llm/model.rs:735`) the ceiling is enforced at
   `:739` *before* `call_provider` (`:740`) runs `maybe_resolve_context_window` (`src/llm/model.rs:828`).
   So the **first request per model per process is sized against the fallback**; discovery lands after it.

---

## Q3 — Which log lines to grep

INFO (default level):
- `resolved model context window` — `src/llm/model.rs:793-800`. Fields: `model` (= `full_model_name`,
  e.g. `je/deepseek-v4.1-flash`), `window`, `source` (`"endpoint /models"` or `"models.dev"`).
  **This is the definitive line.**
- `no models.dev match for …, using default …` — `src/llm/model.rs:805-814`. Presence means both sources
  failed and the fallback stands.

DEBUG (needs `RUST_LOG=debug`):
- `context window from endpoint /models` — `src/llm/context_window.rs:467`
- `context window from models.dev` — `src/llm/context_window.rs:473`
- `models listing did not answer, skipping context-window lookup` — `src/llm/context_window.rs:378-382`
- `models.dev unreachable, falling back to the configured window` — `src/llm/context_window.rs:408`

Tracing target is `spacebot::llm::model` for the two INFO lines (matches the target on chi's historical
log line).

Suggested: `grep -E "resolved model context window|no models.dev match for" <daemon log>`

---

## Empirical cross-check — correcting the assumption in the brief

The R7-lite JSONL's ~115k branch inputs **prove nothing**: 115k is below 128k, so neither ceiling would
trim. That threshold was wrong.

The trim fires when `history + overhead > ceiling − reserve − overhead`, where
`reserve = RESPONSE_RESERVE × ceiling` and `RESPONSE_RESERVE = 0.15` (`src/llm/model.rs:154`, `:164-167`).
So the discriminator is **a request whose provider-reported `input` exceeds ~128k**:

- `cache_stats.jsonl` shows `input > 128000` **and** no `trimmed request history to fit the model's context
  ceiling` warning for it → the ceiling in force is above 128k → **the flip works; only Q1's display is stale.**
- The warning reappears → its `ceiling=` field is the *resolved* value (`src/llm/model.rs:615-675`), so read
  that number, not the status block.

Note the historical warning chi quoted (`ceiling=128000`, estimated_before=185329, dropped_messages=70) is
from `2026-10-11T06:37Z` — **before** the flip shipped, so 128k was correct under the old `min()` rule.

---

## Same bug class, not fixed by the flip

The flip changed request-level trimming only. These sites still read the **config** number from
`rc.context_window`, so agent-level budgets remain on the 128k scale regardless of discovery:

| Consumer | Location |
|---|---|
| **Compaction trigger** (`usage = estimated / context_window`) | `src/agent/compactor.rs:63` |
| Branch fork precompaction | `src/agent/branch.rs:386` |
| Chronicle emergency | `src/agent/chronicle.rs:314` |
| Worker budgets | `src/agent/worker.rs:1279`, `:1323` |
| Channel / dispatch | `src/agent/channel.rs:3807`, `src/agent/channel_dispatch.rs:1072` |

Consequence: even with discovery working, compaction still aims at 128k, so the
"70 messages dropped" class of silent context loss can recur via compaction.

---

## Fix proposal (NOT implemented — for chi to approve)

1. **Display** — `src/agent/status.rs:101`: report the resolved ceiling (fall back to config when
   unresolved). Same for `src/api/config.rs:369`, `src/self_awareness.rs:181`.
2. **Propagate** — when `note_resolved_context_window` lands, publish the window into
   `RuntimeConfig::context_window` so compaction / chronicle / branch / worker budgets scale with it.
   Keep the config value as the fallback for unresolved models.
3. **Ordering** — resolve before first enforcement (move the call to the top of `attempt_completion`, or
   resolve eagerly at model construction / warmup) so the first request per model is not trimmed at 128k.
4. **Precedence decision** — `src/llm/context_window.rs:464-469` makes the endpoint listing outrank
   models.dev. If je's `/models` advertises a small window, either accept it (honest) or add a per-provider
   knob so models.dev can win.

---

## Commands for chi to settle the remaining unknowns

```bash
# 1. Did discovery run, and what did it find?
grep -E "resolved model context window|no models.dev match for" <daemon log>

# 2. What does je itself advertise? (endpoint outranks models.dev)
curl -s "$JE_BASE_URL/models" -H "Authorization: Bearer $JE_KEY" | head -c 2000

# 3. je provider api_type must not be "anthropic" (suppresses discovery entirely)
grep -A 12 'name = "je"' <config>/spacebot.toml

# 4. Post-deploy behaviour test: any request with input > 128k and no trim warning?
#    (cache_stats.jsonl is at <data_dir>/cache_stats.jsonl)
```
