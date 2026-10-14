# spacebotX debloat audit — round 2 (read-only)

Date: 2026-10-02 · HEAD `34226aba` · no edits, no commits

## 1. Baseline

| Metric | Before removals | Now |
|---|---|---|
| Cargo.lock `[[package]]` entries | ~974 | **974** |
| Unique package *names* | 837 | **873** |

Note: the "837 → 873" is not an increase in real deps — it's the
name-vs-entry counting artifact (874 entries now vs 974 lines earlier is
the reliable figure). Unique *name* count includes multiple versions of
the same crate. After chromiumoxide/twitch-irc/bollard removal the
manifest is materially smaller; the lock still carries several
duplicate-major stacks (below).

## 2. Duplicate majors — what remains and who owns them

| Crate | Versions still present | Sole owner(s) |
|---|---|---|
| reqwest | **0.12.28** + 0.13.2 | 0.12 ← `hf-hub`(fastembed) + `lance-namespace-reqwest-client`(lancedb); 0.13 ← spacebot direct, rig-core, rmcp |
| rustls | **0.22.4** + 0.23.36 | 0.22 ← `serenity` (via tokio-tungstenite 0.21 / tungstenite 0.21); 0.23 ← everything else |
| hyper | 1.8.1 only | ✅ the hyper 0.2 stack is **gone** with twitch-irc |
| rand | 0.8.5 + 0.9.2 + **0.10.0** | 0.10 ← `slack-morphism`; 0.8 ← nanoid(rig-core) + tantivy(lance); 0.9 ← datafusion(lance) + spacebot direct |
| http | 1.4.0 only | ✅ single major |

**twitch-irc removal verified:** the old hyper 0.2 / reqwest 0.11 /
rustls 0.21 / webpki-roots stack is fully gone.

**Remaining duplicate owners are all "hard" chains:**

- `reqwest 0.12` — pinned by **lancedb/lance** and **fastembed/hf-hub**
  (both core-locked to the fork's memory subsystem). Can't dedupe without
  upstream bumps.
- `rustls 0.22 + tungstenite 0.21` — **owned *solely* by `serenity`
  (Discord)**. This is the largest *removable* duplicate left: removing
  Discord collapses 0.22/0.21 and their rustls/tokio-rustls/tungstenite twins.
- `rand 0.10` — **owned solely by `slack-morphism`**. Removing Slack kills
  one of the three rand major lines outright.
- `rand 0.8` — tangles rig-core + lancedb; not independently removable.

## 3. Tier-2 adapters — ranked cut table

Subtree sizes are forward `cargo tree -p <crate>` line counts (approximate
node counts incl. shared/dup). None of these adapters is feature-gated —
all are hard-compiled.

| Rank | Item | Primary dep & subtree | Files touched | Func. loss |
|---|---|---|---|---|
| 1 | **Discord** | `serenity` ~467 · owns rustls 0.22 dup | `src/messaging/discord.rs` (2031 ln) + 51 ref files | Discord entirely |
| 2 | **Slack** | `slack-morphism` ~383 · owns rand 0.10 | `slack.rs` (1913 ln) + 37 ref files | Slack entirely |
| 3 | **Email/IMAP** | `lettre` ~183 + `imap` 56 + `mailparse` 7 | `email.rs` (1921 ln) + 29 ref files | Email send/recv |
| 4 | **Mattermost** | shares `reqwest` (no dedicated crate) | `mattermost.rs` (1947 ln) + 19 refs | Mattermost |

**Dependency-overlap findings:**
- **Discord (serenity)** is the only puller of rustls 0.22.4,
  tungstenite 0.21, tokio-rustls 0.25, tokio-tungstenite 0.21, and
  rand 0.8-via-tungstenite. Highest dedup payoff of the remaining cuts.
- **Slack (slack-morphism)** is the only puller of `rand 0.10`. 
- **Email** — `lettre` uses **native-tls**; but native-tls is *also*
  pulled by `reqwest 0.12` (lancedb/fastembed) via hyper-tls, so
  removing email does **not** free native-tls. `imap` is dedicated
  (plus its vendored `imap-proto`); `mailparse` is tiny/dedicated.
- **Mattermost** adds no dedicated crate — it reuses `reqwest`/`serde_json`,
  so its cut frees ~0 dep nodes; only source+wiring shrinks (~1947 lines).
- **Signal** (`signal.rs`, 2193 ln) has **no Rust crate** — it talks to an
  external `signal-cli` daemon over HTTP. Cutting it frees 0 crates.

**Estimated crates freed (unique, dedup'd):**
Discord ≈ 120–160 · Slack ≈ 90–120 · Email ≈ 60–80 · Mattermost ≈ 0.
All four = full adapter-count cleanup (~1900 lines removed across the
4 files + wiring in ~100 referencing files).

## 4. Feature / default-features trims (evidence-based)

| Dep | Current | Evidence | Candidate |
|---|---|---|---|
| `tokio` | `features=["full"]` | `spawn_blocking`=0 uses; `net`=5, `signal`=2, `process`=9, `fs`=34 | Replace `full` with explicit: `rt-multi-thread, macros, fs, process, net, time, sync, signal, io-util`. Drops unused `full`-only bits — **small** win but free |
| `sqlx` | `runtime-tokio, sqlite, migrate, chrono, uuid` | all used (`sqlx::migrate` 36×) | **nothing trimmable** — feature set already minimal |
| `teloxide` | `default-features=false, features=["rustls"]` | — | already trimmed |
| `serenity` | default-features=false + explicit | — | already trimmed; only cut is removing the dep |
| `reqwest` | 6 features | json/stream/form/query/gzip/multipart | check `query`/`form` usage; possible micro-trim |
| `notify` | `"7"` default | watcher used | default-features ok |
| `lancedb`/`fastembed` | default | core | **not trimmable** |
| `utoipa`+`utoipa-swagger-ui` | default on | Swagger UI ~236 subtree; only if API docs unused | possible cut if OpenAPI UI not needed |

**Highest-value trimmable feature:** none dramatic — the manifest is already
disciplined (most heavy deps already `default-features=false`).

## 5. Build profile

```toml
[profile.release]
lto = "thin"
strip = true
```

- No `[profile.dev]` / `[profile.dev.package.*]` overrides.
- **Absent levers:** 
  - `[profile.dev] opt-level` / `debug = false` for faster dev builds
  - `[profile.dev.package."*"] opt-level = 3` (speed at runtime, faster
    than debug for heavy crates like lancedb/datafusion)
  - `codegen-units` tuning
  - `[profile.release] incremental = true` (if you iterate on release)
- **Heaviest remaining deps (forward subtree size):**

| Dep | subtree | Removable? |
|---|---|---|
| `lancedb` (+lance/datafusion) | ~1766 | ❌ core (memory) |
| `fastembed` (+hf-hub) | ~644 | ❌ core (embeddings) |
| `rig-core` | ~478 | ❌ core (LLM) |
| `teloxide` | ~456 | ❌ core (Telegram) |
| `serenity` | ~467 | ✅ **removable** (Discord) |
| `slack-morphism` | ~383 | ✅ **removable** (Slack) |
| `sqlx` | ~304 | ❌ core (store) |
| `lettre`+`imap` | ~239 | ✅ removable (Email) |

The single biggest *removable* subtree is **Discord/serenity**, and it also
carries the rustls-0.22 duplicate — the best next cut on both axes
(compile time + dedup).

## 6. Verdict / recommended next round

1. **Discord** — biggest removable subtree (~467) *and* collapses the
   rustls 0.22 duplicate + tungstenite 0.21 + their tokio-* twins.
2. **Slack** — second (`~383`), kills the rand 0.10 line.
3. **Email/IMAP** — moderate (`~239` across lettre+imap+mailparse), but
   note native-tls survives via reqwest 0.12, so payoff is smaller than
   the raw subtree suggests.
4. **Mattermost** — 0 crate win, pure source/wiring cleanup.
5. **tokio `full` → explicit features** — small but free, do it anytime.
6. **Profile levers** — add `[profile.dev.package."*"] opt-level=3` and
   consider `[profile.dev] debug=false` for faster incremental rebuilds;
   the current profile is release-only.

Two "no-cut" findings worth stating: **Signal** frees nothing (external
binary), **Mattermost** frees nothing (shared reqwest). Keep pdf-extract as
ordered; it's small (~70) and you want it.
