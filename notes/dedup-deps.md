# Duplicate dependency audit — spacebotX

**Checkout:** `/data/agents/main/workspace/spacebotX` @ `feat/fk-cascade-cleanup`
**Crate:** `spacebot v0.5.0` · **Toolchain:** rustc/cargo 1.97.1 (pinned)
**Date:** 2026-10-15 · **Status:** ✅ low-risk consolidations applied; `cargo check` passes; edits left **uncommitted** (shared with the aws-lc-sys removal commit/PR).

> Tooling note (same as `notes/aws-lc-sys-removal.md`): the sandbox `cargo` shim is broken (`mise`: "No version is set for shim: cargo").
> Use the pinned toolchain binary directly and source the build env:
> ```bash
> source build-env.sh
> export PATH="$HOME/.rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/bin:$PATH"
> ```

## Result

| metric | before | after |
|---|---|---|
| unique crates appearing as duplicates (`cargo tree -d --depth 0`, names de-versioned) | **66** | **64** |
| raw duplicate entries (incl. same-version / different-feature builds) | **115** | **111** |
| crates fully de-duplicated | — | **`toml`, `serde_spanned`** (and `toml_edit` lost its 0.22 entry) |

## What was unified (and how)

### `toml` 0.8.23 + 0.9.12 → single `toml` 0.9.12  (also unifies `toml_edit` and `serde_spanned`)

**Why duplicated:** spacebot pinned `toml = "0.8"` (→ 0.8.23) while the `config` crate (our direct dep, `config = "0.15"`) already resolves `toml = "0.9"` (→ 0.9.12). Two copies of the toml parser plus its helpers were compiled.

**Fix — one line each in our own `Cargo.toml`:**
```diff
-toml = "0.8"
-toml_edit = "0.22"
+toml = "0.9"
+toml_edit = "0.23"
```
`toml_edit` is bumped in lockstep because `toml 0.9` re-exports/uses `toml_edit 0.23`; we consume `toml_edit::DocumentMut` directly in `src/api/bindings.rs`, so keeping our edge on the same 0.23 line removes the 0.22 copy.

**Zero source changes required.** The API surface we use is identical across the bump:
- `toml::from_str` — `src/config/load.rs`, `src/config/toml_schema.rs`, `src/factory/presets.rs`, `src/sandbox.rs`
- `toml::to_string` / `to_string_pretty` — `src/sandbox.rs`, `src/config/onboarding.rs`
- `toml::Value` / `toml::Table` / `Value::deserialize` — `src/config/types.rs`, `src/config/load.rs`, `src/config/toml_schema.rs`
- `toml_edit::DocumentMut`, `Item`, `Table`, `value()` — `src/api/bindings.rs`

`cargo check` passes with no edits to `src/`.

**Net build effect:** removes the 0.8 copy of `toml` + the paired copies of `toml_datetime 0.6.11`(one edge) and `serde_spanned 0.6.9`. Note `toml 0.9` pulls a *different* parser stack (`toml_parser`, `toml_writer`) so the win is one whole parser copy removed, not just a version bump. `serde_spanned` fell out of the duplicate list entirely (0.6.9-only edge removed).

## What was considered and deliberately NOT changed

### `zip` 2.4.2 (ours) vs 3.0.0 (build-dep of `utoipa-swagger-ui`)
Our direct `zip = "2"`; `utoipa-swagger-ui v9.0.2` uses `zip 3.0.0` as a **build-dependency**. Bumping ours to `zip = "3"` **compiles cleanly with zero source changes** (`cargo check` passed; API — `ZipArchive`, `ZipWriter`, `CompressionMethod`, `write::SimpleFileOptions` — is unchanged) and would remove a further 2 unique duplicates (66→62 / 115→108), also collapsing `flate2`.

**Deferred on risk grounds:** `zip 3.0` is a *major* rewrite (new `compression-codecs` backend) and spacebot uses it to parse **untrusted skill/archive input** (`src/skills/installer.rs`, `src/api/system.rs`). The task's bar is "low-risk"; a silent behavioural change in an untrusted-input parser is not that. Recommendation: do this as its own change with a dedicated round-trip test (writer → archive → reader) against real skill archives before merging. One-line Cargo.toml edit if approved.

### Everything else — third-party-rooted, **not fixable from our side**

The bulk of the 64 remaining duplicates are old majors pinned by *transitive* dependencies. Traced with `cargo tree -i <crate>@<ver>`:

| duplicated crate | versions | who pins the old one (the edge we don't control) | class |
|---|---|---|---|
| `reqwest` | 0.12.28 / 0.13.2 | 0.12 ← `hf-hub`(fastembed), `opentelemetry-http`, `opentelemetry-otlp`, `serenity`, `teloxide-core`; 0.13 ← ours, `rig-core`, `rmcp` | third-party |
| `rustls` | 0.22.4 / 0.23.36 | 0.22 ← `tokio-tungstenite 0.25`/`serenity`; 0.23 ← ours | third-party |
| `tokio-rustls` | 0.25.0 / 0.26.4 | same as rustls | third-party |
| `tokio-tungstenite` / `tungstenite` | 0.21 / 0.23 / 0.28 | `serenity`(0.21), `rig-core`(0.23), ours(0.28) | third-party |
| `webpki-roots` | 0.26.11 / 1.0.6 | 0.26 ← old `tokio-tungstenite`, `ureq`(hf-hub) | third-party |
| `openssl-probe` | 0.1.6 / 0.2.1 | 0.1 ← `native-tls`; 0.2 ← `rustls-native-certs` | third-party |
| `nom` | 5.1.3 / 7.1.3 / 8.0.0 | 5←`imap`; 7←`cron`/`eventsource-stream`(rig); 8←`lettre`/`lopdf` | third-party |
| `base64` | 0.13.1 / 0.22.1 | 0.13 ← `imap`, `spm_precompiled`(tokenizers) | third-party |
| `rand` / `rand_core` / `rand_chacha` / `getrandom` | 0.8/0.9/0.10 ecosystems | old ← `serenity`, `rig-core`(nanoid), `slack-morphism`(0.10), `ahash`(getrandom 0.3) | third-party |
| `thiserror` | 1.0.69 / 2.0.18 | 1 ← `dialoguer 0.11`, `tungstenite 0.21/0.23` | third-party |
| `syn` | 1.0.109 / 2.0.114 | 1 ← `rsb_derive`/`rvs_derive`(slack-morphism) | third-party |
| `darling` | 0.20.11 / 0.21.3 / 0.23.0 | three proc-macro consumers (tokenizers, serde_with, rmcp) | third-party |
| `itertools` | 0.10.5 / 0.14.0 | 0.10 ← `aquamarine`(teloxide) | third-party |
| `hashbrown` | 0.14.5 / 0.15.5 / 0.16.1 | 0.14 ← `chumsky`/`dashmap`/`ordered-multimap`; 0.15 ← `hashlink`(sqlx) | third-party |
| `bitflags` | 1.3.2 / 2.10.0 | 1 ← `inotify`(notify), `lexical-core`(nom 5) | third-party |
| `toml_datetime` | 0.6.11 / 0.7.5 | 0.6 ← `toml_edit 0.19` ← `proc-macro-crate 1.3.1` ← `deluxe`(rig-derive) | third-party |
| `toml_edit` | 0.19.15 / 0.23.10 | 0.19 ← same chain as above | third-party |
| `phf`/`phf_shared` | 0.12.1 / 0.13.1 | `chrono-tz` vs `emojis` | third-party |
| `convert_case` | 0.6.0 / 0.10.0 | `config` vs `rig-derive` | third-party |
| `heck` | 0.4.1 / 0.5.0 | `deluxe-macros`(rig-derive) vs clap/sqlx | third-party |
| `strsim` | 0.10.0 / 0.11.1 | `deluxe-core`(rig-derive) vs clap/darling | third-party |
| `cpufeatures` | 0.2.17 / 0.3.0 | `aes`/`sha2` vs `chacha20`(rand 0.10) | third-party |
| `zune-core`/`zune-jpeg` | 0.4.x / 0.5.x | `image`(fastembed) vs `ravif` | third-party |
| `winnow` | 0.5.40 / 0.7.14 | old ← `toml_edit 0.19` chain | third-party |
| `arrayvec` | 0.5.2 / 0.7.6 | 0.5 ← `lexical-core`(nom 5) | third-party |
| `smallvec`,`digest`,`crypto-common`,`indexmap`,`uuid`,`tokio`,`futures-*`,`libc`,`log`,`memchr`,`miniz_oxide`,`mio`,`rustix`,`serde_json`,`serde_core`,`simd-adler32`,`sqlx-sqlite`,`flate2`,`idna`,`num-traits`,`pastey`,`chrono` | *same version twice* | build-dep vs normal-dep split, or per-target/per-feature splits | **not real version duplicates** |

**The same-version-twice entries are noise:** `cargo tree -d` lists a crate once per distinct (version, feature-set, build/normal-dep-kind) resolution. e.g. `tokio v1.49.0` ×2 = same crate built for the host-tool and target graph; `flate2 1.1.9` ×2 = normal dep + `utoipa-swagger-ui` build-dep. These cannot and should not be "merged" — they are separate compilation units by design.

## `cargo check` result

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.25s
warning: the following packages contain code that will be rejected by a future version of Rust: proc-macro-error2 v2.0.1
```
Passes. Pre-existing warning (`proc-macro-error2`, transitive via a proc-macro dep) is unrelated.

## Constraints honoured

- ✅ No source changes required by any applied bump.
- ✅ No reintroduction of `aws-lc-sys` or `cmake` — `cargo tree | grep -i aws-lc` → **absent**.
- ✅ `cargo check` passes.
- ✅ Edits uncommitted on `feat/fk-cascade-cleanup`, sharing the eventual commit/PR with the aws-lc-sys change.

## Recommendation

1. **Merge the `toml`/`toml_edit` bump** with the aws-lc change — it is a clean, zero-source-change win that removes a whole second copy of the TOML parser stack.
2. **Do the `zip` 2→3 bump separately**, gated on a skill-archive round-trip test (writer→reader), because it changes a parser that handles untrusted input. It is a further 2-crate dedup if it passes.
3. **Accept the rest.** Every remaining version duplicate is pinned by a transitive dependency (fastembed/tokenizers, serenity, teloxide, slack-morphism, rig-core, imap, opentelemetry). They resolve automatically as those upstreams adopt modern majors; there is no lever in our `Cargo.toml`. The wide `reqwest 0.12/0.13` split is the biggest structural one and is entirely upstream-driven.
