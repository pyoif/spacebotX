# Can the reqwest 0.12 vs 0.13 duplicate be resolved?

Date: 2026-10-15
Branch: feat/fk-cascade-cleanup
Toolchain: pinned cargo via `source build-env.sh`

## TL;DR

**No — not resolvable today, and not by any `[patch]`/`[replace]` trick.**
The duplicate is caused by third-party crates on the **older end** of the
split (fastembed/hf-hub, opentelemetry, serenity, teloxide-core) that pin
`reqwest ^0.12` and have **no released version that moved to 0.13** except
upstreams we cannot reach. Meanwhile our *own* edge plus rig-core 0.33 and
rmcp 1.1 are anchored on **0.13**. Neither direction can collapse to one.

- Full unification **now**: **impossible.**
- Partial unification (kill the 0.12 side): **impossible** — the 0.12 consumers
  have no 0.13-capable release we can get to.
- Downgrade our edge to 0.12 (so *everything* is 0.12): **resolves cleanly**
  (`cargo generate-lockfile` → single `reqwest v0.12.28`, aws-lc absent), **but**
  it is blocked by **rig-core 0.33** and **rmcp 1.1**, which hard-pin
  `reqwest ^0.13` / `^0.13.2`. Downgrading those two is not viable (breaks the
  LLM/MCP stacks). So even the fallback is blocked by our own 0.13-edge
  dependencies, not just the 0.12 ones.

**Verdict: not until upstream updates.** Wait on fastembed/hf-hub,
opentelemetry, serenity, teloxide-core for 0.13.

---

## 1. Exact reqwest-0.12 dependents (`cargo tree -i reqwest@0.12 -e normal`)

```
reqwest v0.12.28
├── hf-hub v0.4.3
│   └── fastembed v4.9.1
│       └── spacebot v0.5.0
├── opentelemetry-http v0.31.0
│   └── opentelemetry-otlp v0.31.0
│       └── spacebot v0.5.0
├── opentelemetry-otlp v0.31.0 (*)
├── serenity v0.12.5
│   └── spacebot v0.5.0
└── teloxide-core v0.13.0
    └── teloxide v0.17.0
        └── spacebot v0.5.0
```

reqwest 0.13 side (`cargo tree -i reqwest@0.13.2 -e normal`):

```
reqwest v0.13.2
├── rig-core v0.33.0
├── rmcp v1.1.0
└── spacebot v0.5.0        (our own edge)
```

Five third-party roots pull 0.12: **hf-hub (via fastembed), opentelemetry-http
/ opentelemetry-otlp, serenity, teloxide-core (via teloxide)**.

## 2. Do newer versions exist that use reqwest 0.13?

Queried crates.io API (`/api/v1/crates/<name>` and `.../dependencies`).

| Crate (current) | Latest released | Reqwest in latest | 0.13-capable? |
|---|---|---|---|
| hf-hub 0.4.3 | **1.0.0** | `^0.13` (non-optional) | **yes** — but unreachable, see below |
| fastembed 4.9.1 | **7.1.1** | via `hf-hub ^0.5.0` → `reqwest ^0.12.2` | **no** |
| opentelemetry-http 0.31.0 | **0.33.1** | `^0.13.1` | **yes** — but needs bumping the whole otel stack |
| opentelemetry-otlp 0.31.0 | **0.33.1** | (otel-http 0.33 → 0.13) | **yes** — same stack bump |
| serenity 0.12.5 | 0.12.5 (max; no 0.13 exists) | `>=0.11.22, <...` range, resolves 0.12 | **no** |
| teloxide-core 0.13.0 | 0.13.0 (max) | `^0.12.7` | **no** |
| teloxide 0.17.0 | 0.17.0 (max) | (teloxide-core 0.13 → 0.12) | **no** |

### The hf-hub trap (the one that looks fixable but isn't)

- `hf-hub 1.0.0` **is** reqwest-0.13 (`reqwest ^0.13`, non-optional). ✔
- But the only reqwest-0.13 *consumer path into our tree* is fastembed, and
  **no published fastembed depends on hf-hub 1.0**:
  - fastembed 4.9.1 → `hf-hub ^0.4.1`
  - fastembed 5.17.x → `hf-hub ^0.4.1`
  - fastembed 6.x / 7.x → `hf-hub ^0.5.0`
  - `hf-hub 0.5.0` → `reqwest ^0.12.2` (still 0.12)
  - So even on fastembed 7.1.1 we land on **hf-hub 0.5 → reqwest 0.12**.
  - hf-hub 1.0 is **outside fastembed's `^0.5` requirement** and cannot be forced
    in without patching fastembed itself.

`opentelemetry-http/otlp 0.33.1` *do* use reqwest 0.13, so the otel branch is
fixable by bumping the otel stack (0.31 → 0.33) — but that's an unrelated
opentelemetry migration, and it would only remove **one** of the five roots;
the duplicate would remain via hf-hub/serenity/teloxide.

## 3. Can `[patch]` or `[replace]` force 0.12 crates onto 0.13?

**No. Confirmed empirically, two independent ways:**

**(a) `[patch.crates-io]` cannot widen a semver range.** A patch *replaces a
source*, it does not change what a dependent's `version = "^0.12"` accepts. Tested:

```
[patch.crates-io]
reqwest = { version = "=0.13.5" }
# error: patch for `reqwest` points to the same source, but patches must
# point to different sources
```
(Patching the same registry to a different version is rejected outright — a
patch must point at a *different source*: git, path, alternative registry.)

**(b) `[replace]` cannot change the version key.** Tested:
```
[replace]
"reqwest:0.12.28" = { version = "0.13.5" }
# error: replacements cannot specify a version requirement
```
`[replace]` maps `pkg:version` → another *source of the same version*; it
cannot substitute a different major. And `hf-hub ^0.12` consumers would still
demand a 0.12-compatible crate.

Cargo unifies only **semver-compatible** requirements. 0.12 and 0.13 are
separate semver ranges (0.x minor bumps are breaking), so **two reqwest
majors in one tree is the correct, expected resolution** — not a bug, and not
patchable.

## 4. Fallback: downgrade OUR edge to reqwest 0.12

**Feature check — reqwest 0.12.28 DOES support rustls without aws-lc.** ✔
0.12.28 exposes:
- `rustls-tls-no-provider` (= `rustls-tls-manual-roots-no-provider` → `__rustls`)
- `__rustls-ring` → `hyper-rustls?/ring`, `tokio-rustls?/ring`, `rustls?/ring`
- `rustls-tls-webpki-roots` / `rustls-tls-native-roots` (ring-backed)
- `default-tls` / `native-tls` (openssl)

So 0.12 can be ring-only, no aws-lc. This is the same trick as our current
0.13 config: install the ring provider in `src/main.rs` and use the
no-provider feature.

**Empirical test (standalone manifest mirroring our real consumers):**
`version = "=0.12.28"` + `rustls-tls-no-provider` together with
hf-hub 0.4, fastembed 4, opentelemetry-otlp 0.31, serenity 0.12, teloxide 0.17
→ `cargo generate-lockfile` succeeded:

```
Locking 471 packages
Adding reqwest v0.12.28 (available: v0.13.5)
grep -A1 'name = "reqwest"' Cargo.lock  ->  version = "0.12.28"   (single entry)
grep -c aws-lc Cargo.lock -> 0
```

**A naive downgrade of our edge alone resolves the 0.12 side to one copy — but
it breaks our 0.13-edge deps:**

- `rig-core 0.33.0` → `reqwest ^0.13` (**non-optional**) — cannot use 0.12.
- `rmcp 1.1.0` → `reqwest ^0.13.2` (optional, but we enable it via
  `reqwest-tls-no-provider` / `transport-streamable-http-client-reqwest`).

So the fallback is **blocked by rig-core and rmcp**, not just the 0.12 side.
To make it real you would also have to downgrade rig-core to a pre-0.13 release
and rmcp likewise — a large, separate migration that changes the LLM and MCP
HTTP stacks. **Not recommended.**

**Source cost of the downgrade itself (if it were reachable):** feature-name
deltas — `form` and `query` are **0.13-only** request-builder features. Our
code uses `.form(...)` at `src/openai_auth.rs:41` and `:325` (2 sites), and
`.query(...)` at `src/opencode/server.rs:343,372,401,423,452` (5 sites, on a
RequestBuilder). Those would each need rewriting for 0.12. Low count, but
non-zero, and it's churn on auth paths.

## Recommendation

1. **Do not** attempt `[patch]`/`[replace]` — proven impossible, would only
   produce a confusing broken lock.
2. **Keep our edge on reqwest 0.13 + `rustls-no-provider`** (the aws-lc
   removal already landed). It's the correct side of the split — it's where
   rig/rmcp live and where the ecosystem is heading.
3. **Leave the duplicate.** It costs one extra `reqwest 0.12.28` + its
   `hyper-tls`/`native-tls` subtree; it does not pull aws-lc (0.12.28 default
   features are not enabled by our consumers — hf-hub/serenity/teloxide set
   `default-features = false`, so they don't drag `default-tls`). Verified: our
   real tree has no `aws-lc` from the 0.12 side.
4. **Revisit when upstream moves.** The realistic unblock order:
   - fastembed → hf-hub 1.0 (then hf-hub drops off the 0.12 list);
   - serenity/teloxide ship reqwest-0.13 releases;
   - then only opentelemetry remains, fixable by the otel 0.31 → 0.33 bump.
5. **Optional partial win now:** bumping opentelemetry 0.31 → 0.33 flips
   `opentelemetry-http`/`opentelemetry-otlp` to reqwest 0.13. It removes 2 of
   the 5 roots but **does not eliminate the duplicate** (hf-hub/serenity/
   teloxide still hold 0.12), so it's not worth doing for dedup alone — only
   if you're touching otel anyway.

## Appendix — commands used

```
source build-env.sh
cargo tree -i reqwest@0.12 -e normal
cargo tree -i reqwest@0.13.2 -e normal
curl -sS -H 'User-Agent: dep-audit' https://crates.io/api/v1/crates/<crate>
curl -sS -H 'User-Agent: dep-audit' https://crates.io/api/v1/crates/<crate>/<ver>/dependencies
```
