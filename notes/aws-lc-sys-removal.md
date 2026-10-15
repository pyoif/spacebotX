# aws-lc-sys removal — feasibility, change, verification

**Checkout:** `/data/agents/main/workspace/spacebotX` @ branch `feat/fk-cascade-cleanup`
**Crate:** `spacebot v0.5.0` · **Toolchain:** rustc/cargo 1.97.1 (pinned)
**Date:** 2026-10-15 · **Status:** ✅ removal is clean; `cargo check` passes; edits left **uncommitted** for review.

> Note on tooling: the `cargo` shim in this sandbox is broken (`mise` reports "No version is set for shim: cargo").
> Use the toolchain binary directly: `export PATH="$HOME/.rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/bin:$PATH"`.
> Builds also need `source build-env.sh` (C toolchain + OPENSSL_DIR + protoc from pixi envs).

## Answer: yes — aws-lc-sys can be removed cleanly.

## 1. Who pulls aws-lc-sys in (the single root cause)

`cargo tree -i aws-lc-sys` gives exactly one parent chain:

```
aws-lc-sys v0.37.1
└── aws-lc-rs v1.15.4
    └── rustls v0.23.36  (feature "aws_lc_rs")
```

And `cargo tree -e features -i aws-lc-rs` shows **one** feature edge that turns it on:

```
aws-lc-rs ← rustls feature "aws_lc_rs"
          ← hyper-rustls/tokio-rustls feature "aws-lc-rs"
          ← reqwest feature "__rustls-aws-lc-rs"
          ← reqwest feature "rustls"
```

**The sole source is `reqwest 0.13`'s `rustls` feature.** reqwest 0.13.2 defines:

```toml
default-tls = ["rustls"]
rustls = ["__rustls-aws-lc-rs", "dep:rustls-platform-verifier", "__rustls"]
__rustls-aws-lc-rs = ["hyper-rustls?/aws-lc-rs", "tokio-rustls?/aws-lc-rs",
                      "rustls?/aws-lc-rs", "quinn?/rustls-aws-lc-rs"]
```

So in reqwest 0.13, **`default-tls` (and therefore `default`) hard-enables `rustls/aws-lc-rs`.**
There is *no* ring-only TLS feature in reqwest 0.13 (unlike 0.12, where `default-tls = ["native-tls"]`
and the rustls-* features use `__rustls-ring`). This is the trap the audit flagged.

Three independent edges activated `reqwest(0.13)/rustls`:

| # | Edge | Path |
|---|------|------|
| 1 | spacebot's own `reqwest = "0.13"` (default features on) | `default → default-tls → rustls → __rustls-aws-lc-rs` |
| 2 | spacebot's `rig` (default on) | `rig-core default → reqwest-rustls → reqwest/rustls` |
| 3 | spacebot's `rmcp` `reqwest` feature | `rmcp reqwest → reqwest?/rustls` |

## 2. Crates that depend on aws-lc-rs / aws-lc-sys (transitively)

Everything downstream of the one `rustls/aws_lc_rs` feature — i.e. **echoes, not additional causes**:

- `rustls v0.23.36` (holds the feature)
- `hyper-rustls v0.27.7`, `tokio-rustls v0.26.4`, `tungstenite 0.23/0.28`, `tokio-tungstenite 0.23/0.28` (via `?/aws-lc-rs` forwards)
- `rustls-webpki v0.103.9` (via `rustls/aws_lc_rs → webpki/aws-lc-rs`)
- `rustls-platform-verifier v0.6.2` (only consumes `std`; its aws-lc appearance is purely transitive)
- reqwest **0.13.2** only — the 0.12.28 consumers (`serenity`, `teloxide-core`, `opentelemetry-otlp`, `hf-hub`/`fastembed`) use native-tls or ring and do **not** enable aws-lc.

## 3. Removal path taken (`rustls-no-provider` + app-installed ring provider)

reqwest 0.13 offers `rustls-no-provider`, which adds `__rustls` (→ `dep:rustls`) **without** forcing a
provider — `rustls-no-provider = ["dep:rustls-platform-verifier", "__rustls"]`. It does **not** enable
`__rustls-aws-lc-rs`. reqwest then resolves the provider at client-build time from
`rustls::crypto::CryptoProvider::get_default()` (src/async_impl/client.rs:714).

**spacebot already installs the ring provider at startup** — `src/main.rs:222-225`:
```rust
fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))?;
```
So `rustls-no-provider` is safe: the provider is guaranteed installed before any client is built.
(reqwest's `default_rustls_crypto_provider()` would `panic!("No provider set")` only if `get_default()` were None — impossible here.)

## 4. Anything else hard-requiring aws-lc-*? — No.

- `grep -rn 'aws_lc\|aws-lc' src/ tests/ examples/` → **no hits**.
- `rustls-platform-verifier` only enables `std`, not aws-lc.
- No crate in the graph pins `aws-lc-*` directly.

## 5. The edit (made, uncommitted)

```diff
-# LLM / Rig framework
-rig = { version = "0.33", package = "rig-core", features = ["derive"] }
-
-# HTTP clients for LLM providers
-reqwest = { version = "0.13", features = ["json", "stream", "form", "query", "gzip", "multipart"] }
+rig = { version = "0.33", package = "rig-core", default-features = false,
+        features = ["derive", "reqwest-native-tls"] }
+
+reqwest = { version = "0.13", default-features = false,
+            features = ["json", "stream", "form", "query", "gzip", "multipart",
+                        "http2", "charset", "system-proxy", "rustls-no-provider"] }
...
-rmcp = { version = "1.1", features = ["client", "reqwest", ...] }
+rmcp = { version = "1.1", features = ["client", "reqwest-tls-no-provider", ...] }
```

Rationale per line:
- **reqwest**: drop default features (`default-tls` is the aws-lc switch); re-add the previously-implicit
  defaults explicitly (`http2`, `charset`, `system-proxy` — these pull no aws-lc; `system-proxy`
  preserves prior env-proxy behavior) and add `rustls-no-provider` for TLS via the installed ring provider.
- **rig**: default features bring `reqwest-rustls`; use `reqwest-native-tls` instead. (native-tls/openssl
  was *already* in the tree via hf-hub/imap/lettre/reqwest-0.12, so no new C library.)
- **rmcp**: `reqwest` feature == `reqwest?/rustls`; `reqwest-tls-no-provider` == `reqwest?/rustls-no-provider`.

## 6. Verification

- `cargo tree -i aws-lc-sys` → **"package ID specification did not match any packages"** (absent).
- `cargo tree -i aws-lc-rs` → absent.
- `cargo tree | grep -i aws-lc` → **NONE**.
- `cargo tree -p spacebot | grep -i aws` → **zero**.
- `cargo tree -e features` → **every** rustls provider edge is now `ring`; zero `aws` tokens.
- **`cargo check` → `Finished dev profile (unoptimized + debuginfo) target(s) in 3m 17s`** (first run,
  cold-ish) and **1m 54s** (second run, after re-adding `system-proxy`) — no errors either time
  (only a pre-existing unrelated warning: `proc-macro-error2` future-incompat).
- `cargo metadata` OK.
- `Cargo.lock` diff removes exactly: **aws-lc-rs, aws-lc-sys, cmake, dunce, fs_extra**.
  `quinn` is *removed from reqwest's dep list* (it was only needed by reqwest's http3/aws-lc path) but
  remains in the lock via tokio-tungstenite.

## 7. Size / compile-time benefit (measured from pre-change artifacts)

aws-lc-sys is the **#1 compile-cost hog** in the prior audit. That build work no longer happens:

| Item | Frozen in pre-change `target/` (now unreachable) |
|------|------|
| aws-lc-sys build dir (+ second dir) | 27 MB (+ 6.4 MB) |
| aws-lc-rs build dirs | small |
| `libaws_lc_sys-*.rlib` + rmeta | 14.4 MB + 2.3 MB |
| `libaws_lc_rs-*.rlib` + rmeta | 5.6 MB + 1.9 MB |
| **Total stale aws-lc artifacts** | **~60 MB** |
| **C translation units no longer compiled** | **342 `.o`** (cc + cmake + bindgen) |
| aws-lc-sys crate source no longer needed | **69 MB** |
| Extra crates removed | cmake 0.1.57, dunce 1.0.5, fs_extra 1.3.0 |

Qualitatively: eliminates the single slowest native step in the cold build (342 `.o` through the C
compiler + cmake + bindgen) and drops the `cmake` build-script dependency entirely — a real win for CI
hermeticity too (cmake + a C toolchain were needed only for this).

## 8. Caveats / things chi should decide before committing

1. **`system-proxy` — RESOLVED (kept).** reqwest 0.13's default includes `system-proxy` (env-proxy
   support). It pulls no aws-lc, so it was re-added to the explicit feature list to preserve prior
   HTTP(S)_PROXY behavior. `grep` found no direct env-proxy use in `src/`, but keeping it is free.
2. **LLM HTTP path backend changes on the rig side**: rig now uses reqwest **native-tls (openssl)** rather
   than rustls/ring. This is a backend swap for LLM calls. If you'd rather keep that path on ring, the
   alternative is to leave rig on `reqwest-rustls` **and** add `rustls-no-provider`... but that does *not*
   work — rig's `reqwest-rustls` literally enables `reqwest/rustls` (aws-lc). So the options for rig are
   (a) native-tls (chosen), or (b) pin rig-core to a version whose `reqwest-rustls` uses a no-provider
   feature set (not available in 0.33). `reqwest-native-tls` is the only clean lever in 0.33.
   Note: native-tls/openssl was **already** compiled in this tree (hf-hub, imap, lettre, reqwest 0.12),
   so this swaps one already-present backend in for the LLM path; it adds no new C library.
3. **provider-install timing — RESOLVED.** spacebot's own reqwest uses `rustls-no-provider` → ring provider
   installed first thing in `main()`. The only other bin, `openapi-spec`
   (src/bin/openapi_spec.rs), just prints JSON and builds **no** reqwest client, so there is no
   construct-before-install panic risk.
4. Two reqwest versions (0.12 + 0.13) remain — this change does not de-dup them.
