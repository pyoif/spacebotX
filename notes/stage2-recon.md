# Stage 2 recon — memory read-path switch (lancedb → SQLite)

Read-only map produced at `5c258564` (Stage 1B, CI green, 1454 tests). Every item is
`file:line` + one line. This is the map a coder implements from without re-reading
the codebase. Nothing here was edited; the only write is this file.

Repo root: `/data/agents/main/workspace/spacebotX` (branch `main`).

---

## 1. `MemorySearch` struct + lance-typed fields + constructors

- `src/memory/search.rs:45` — `pub struct MemorySearch` definition. Fields:
  - `store: Arc<MemoryStore>` (`:46`)
  - **`embedding_table: EmbeddingTable` (`:47`) — lance-typed field #1. Swap to the SQLite-backed equivalent.**
  - `embedding_model: Arc<EmbeddingModel>` (`:48`)
  - `consolidation: Arc<ConsolidationState>` (`:50`)
  - **`chronicle_table: Option<ChronicleEmbeddingTable>` (`:53`) — lance-typed field #2 (optional).**
- `src/memory/search.rs:56` — `impl Clone for MemorySearch` (clones both lance fields — must be updated when types change).
- `src/memory/search.rs:68` — `impl Debug` (only prints `store`, `finish_non_exhaustive`).
- `src/memory/search.rs:78` — `pub fn new(store, embedding_table: EmbeddingTable, embedding_model)` — **constructor #1 (production).** Takes the lance `EmbeddingTable`.
- `src/memory/search.rs:94` — `pub fn with_chronicle_table(mut self, table: ChronicleEmbeddingTable) -> Self` — attaches the optional chronicle table.
- `src/memory/search.rs:196` — `pub fn embedding_table(&self) -> &EmbeddingTable` — accessor; callers use it to write/ensure-FTS (see §4, §5). Must return the new type.
- `src/memory/search.rs:201` — `pub fn embedding_model(&self) -> &EmbeddingModel`.
- `src/memory/search.rs:206` — `pub fn embedding_model_arc(&self) -> &Arc<EmbeddingModel>`.
- `src/memory/search.rs:191` — `pub fn store(&self) -> &MemoryStore`.

## 2. The three search entry points (lance consumers)

All three live in `src/memory/search.rs`:

1. **`pub async fn hybrid_search(&self, query, config)` — `src/memory/search.rs:301`.**
   The RRF-producing path. Consumes lance in two places:
   - `:319` — `self.embedding_table.text_search(query, config.max_results_per_source)` (lance FTS) → `fts_results: Vec<ScoredMemory>`.
   - `:346` — `self.embedding_model.embed_one(query)` then `:351` `self.embedding_table.vector_search(&query_embedding, ...)` (lance vector) → `vector_results: Vec<ScoredMemory>`.
   - Graph step (`:370`–`:389`) is SQLite-only (`store.get_high_importance` + `traverse_graph`) — **no lance, leave alone.**
   - `:385` — calls `reciprocal_rank_fusion(...)` (see §3).
   - Both lance calls are wrapped in `match ... Err => tracing::debug!` fallbacks (`:333`, `:364`) — the new SQLite path should keep graceful degradation.

2. **`pub async fn search(&self, query, config)` — `src/memory/search.rs:218`.**
   Dispatcher: `SearchMode::Hybrid => hybrid_search` (`:223`), else `metadata_search` (SQLite-only). Also runs the chronicle range join (`:232`–`:243`, SQLite) and metrics. **The switch point: `SearchMode::Hybrid` must reach the new SQLite hybrid.**

3. **`pub async fn search_with_chronicle(&self, query, config, checkpoint_limit)` — `src/memory/search.rs:166`.**
   Calls `self.search(...)` (`:171`), then if `chronicle_table` is set embeds the query and calls `table.vector_search(&embedding, checkpoint_limit)` (`:180`) → `Vec<ChronicleHit>`, then retains `similarity >= CHRONICLE_MIN_SIMILARITY` (`:181`; const `:16` = `0.3`). **Chronicle vector search is lance consumer #3.**

- `metadata_search` — `src/memory/search.rs:266` — **SQLite-only** (`store.get_sorted`), no lance. Leave alone.

**External call sites of the search API (for compile-fixing after a signature change):**
- `src/tools/memory_recall.rs:253` — `.search_with_chronicle(query, &config, args.max_results.min(5))`.
- `src/api/memories.rs:216` — `.search_with_chronicle(&query.q, &config, query.limit.min(5))`.
- `src/tools/factory_search_context.rs:154` — `.search(query, &config)`.
- (`src/tools/wiki_search.rs:81` and `src/api/wiki.rs:199` call a *different* `search` — wiki store, not memory. Ignore.)

**Owners of the `Arc<MemorySearch>` (construct it / hold it):**
- `src/lib.rs:531` — `pub memory_search: Arc<memory::MemorySearch>` (the shared dep struct).
- `src/api/state.rs:14` — `use crate::memory::{EmbeddingModel, MemorySearch}`.
- `src/agent/maintenance.rs:137`–`:141` — passes `store()`, `embedding_table()`, `embedding_model_arc()` into maintenance ops.
- `src/agent/cortex.rs:1675`–`:1677` and `:2341`–`:2342` — same triple passed to consolidation/maintenance helpers.

## 3. RRF fusion — input contract

- `src/memory/search.rs:527` — `fn reciprocal_rank_fusion(vector_results: &[ScoredMemory], fts_results: &[ScoredMemory], graph_results: &[ScoredMemory], k: f64) -> Vec<ScoredMemory>`.
- `src/memory/search.rs:520` — `struct ScoredMemory { memory: Memory, score: f64 }` (private).
- **Contract: RRF consumes three ordered `&[ScoredMemory]` lists and uses *rank position only*** — the score field is ignored for fusion (`1.0/(k+rank+1)` at `:537`, `:548`, `:559`). It keys on `scored.memory.id` (`:538`). So the SQLite path only has to produce **the same candidate ids in the same rank order** as the lance path; absolute score scale is irrelevant. `ScoredMemory.memory` must be a fully loaded `Memory` (both current producers call `store.load(id)` and skip `forgotten`).
- `src/memory/search.rs:387`–`:407` — post-fusion: type filter, `enumerate()` for rank, `score >= config.min_score` filter, `take(max_results_per_source)`.

## 4. ALL lancedb write sites (tables `memory_embeddings` / `chronicle_embeddings`)

Lance table names: `TABLE_NAME = "memory_embeddings"` (`src/memory/lance.rs:11`), `CHRONICLE_TABLE_NAME = "chronicle_embeddings"` (`src/memory/lance.rs:501`).

**Production write paths (must be repointed to `memory_vectors` / `chronicle_vectors`):**
- `src/tools/memory_save.rs:456` — `self.memory_search.embedding_table().store(&memory.id, &memory.content, &embedding)` — the primary memory write.
- `src/tools/memory_save.rs:478` — `...embedding_table().ensure_fts_index()` after save.
- `src/memory/search.rs:114` — `table.store(&checkpoint.id, &checkpoint.title, &checkpoint.channel_id, checkpoint.seq, &text, &embedding)` inside `embed_chronicle_checkpoint` (`:101`) — the chronicle write.
- `src/memory/consolidation.rs:394` — `.store(survivor_id, content, &embedding)` (consolidation rewrite).
- `src/memory/consolidation.rs:414` — `embedding_table.delete(absorbed_id)`.
- `src/memory/maintenance.rs:367` — `embedding_table.delete(&updated_survivor.id)`.
- `src/memory/maintenance.rs:372` — `embedding_table.store(...)`.
- `src/memory/maintenance.rs:379` — `embedding_table.delete(&merged.id)`.
- `src/memory/maintenance.rs:511` — `.store(&memory.id, &memory.content, &embedding)`.

**Lance connection / table-open sites:**
- `src/db.rs:59` — `lancedb::connect(lance_path)`; path = `data_dir.join("lancedb")` (`:51`). This is the `db.lance` handle threaded everywhere.
- `src/api/agents.rs:852` — `EmbeddingTable::open_or_create(&db.lance)`; `:866` `ChronicleEmbeddingTable::open_or_create(&db.lance)`.
- `src/main.rs:2301` — `EmbeddingTable::open_or_create(&db.lance)`; `:2313` `ChronicleEmbeddingTable::open_or_create(&db.lance)`.
- Test-only lance connects (fixtures): `src/memory/maintenance.rs:523,663,815,854`; `src/memory/search.rs:719,747,772,797`; `src/memory/consolidation.rs:457`; `src/tools/memory_save.rs:691`.

**lance.rs API surface being replaced** (`src/memory/lance.rs`):
- `EmbeddingTable`: `open_or_create` `:32`, `store` `:78`, `delete` `:126`, `vector_search` `:139`, `find_similar` `:195`, `text_search` `:259`, `create_indexes` `:301`, `ensure_fts_index` `:314`, `schema` `:339`, `validate_memory_id` `:359`, `ensure_cosine_vector_index` `:368`.
- `ChronicleEmbeddingTable`: `open_or_create` `:537`, `store` `:576`, `delete` `:638`, `has` `:648`, `vector_search` `:668`, `text_search` `:743`, `create_indexes` `:782`, `ensure_fts_index` `:795`, `schema` `:824`.
- `ChronicleHit` struct — `src/memory/lance.rs:505` (`checkpoint_id`, `title`, `channel_id`, `seq`, `similarity`). **This is the return type of `search_with_chronicle`'s second element — keep the struct (or re-home it), it is part of the public API.**
- Re-export: `src/memory.rs:16` — `pub use lance::{ChronicleEmbeddingTable, ChronicleHit, EmbeddingTable};`.
- `EMBEDDING_DIM: i32 = 384` — `src/memory/lance.rs:12` (all-MiniLM-L6-v2). Vector BLOB = `384 * 4 = 1536` bytes.

**Return-shape contracts the SQLite path must match (so RRF/callers are unchanged):**
- `EmbeddingTable::vector_search` → `Vec<(String, f32)>` = `(memory_id, cosine_distance)` (`src/memory/lance.rs:139`; distance from `_distance`).
- `EmbeddingTable::text_search` → `Vec<(String, f32)>` = `(memory_id, score)` (`src/memory/lance.rs:259`).
- `EmbeddingTable::find_similar` → `Vec<(String, f32)>` where `similarity = 1.0 - distance` (`src/memory/lance.rs:195`).
- `ChronicleEmbeddingTable::vector_search` → `Vec<ChronicleHit>` with `similarity = 1.0 - distance`, sorted desc (`src/memory/lance.rs:668`).
- `ChronicleEmbeddingTable::has` → `bool` (`:648`); `delete` → `()`.
- **Callers convert distance→similarity themselves**: `src/memory/search.rs:353` `let similarity = 1.0 - distance;`. Keep the same convention (return distance from the vector primitive, let callers invert) OR adjust the caller in the same commit.

## 5. `compensate_embedding_failure` — definition + call sites

- `src/tools/memory_save.rs:92` — `async fn compensate_embedding_failure(&self, memory: &Memory, replaced_anchor: Option<&Memory>)` — definition (deletes the just-created row + associations, or restores a merged human anchor, so a memory never "looks saved but is unrecallable").
- `src/tools/memory_save.rs:445` — call site (embedding *generation* failed).
- `src/tools/memory_save.rs:465` — call site (embedding *store* failed).
- **Delete this function + both call sites only after the SQLite write path lands** — with vectors in the same SQLite transaction as the row, the drift class it compensates for disappears. Per the Stage 2 brief this deletion is deferred until the read path passes tests.

## 6. `create_indexes` / `ensure_cosine_vector_index` — zero-caller confirmation

- `src/memory/lance.rs:301` — `EmbeddingTable::create_indexes` definition; `:302` calls `Self::ensure_cosine_vector_index`, `:304` `self.ensure_fts_index()`.
- `src/memory/lance.rs:368` — `async fn ensure_cosine_vector_index(table)` definition.
- `src/memory/lance.rs:782` — `ChronicleEmbeddingTable::create_indexes` definition; `:783` calls `EmbeddingTable::ensure_cosine_vector_index`, `:785` `self.ensure_fts_index()`.
- **Callers of `create_indexes`: NONE** (grep across `src/` finds only the two definitions above). → **Stage 3 deletion candidate; the ANN index never runs.**
- `ensure_fts_index` (the FTS index *is* used) is called at: `src/api/agents.rs:859`, `src/tools/memory_save.rs:478`, `src/main.rs:2306`, `src/main.rs:2315`. All four become no-ops / are removed in Stage 2/3 (FTS5 triggers maintain the index automatically).

## 7. `sqlite_index.rs` current state (what Stage 2 builds alongside)

`src/memory/sqlite_index.rs` (Stage 1B) — **backfill only, no query path yet.**
- `:33` — `fn chronicle_source_text(title, summary) -> String` (private) — `"{title} {summary}"`, the exact chronicle indexed-text split.
- `:50` — `pub async fn backfill_memory_search_text(pool: &SqlitePool) -> Result<usize>` — rewrites `memories.search_text` to `analyzer::search_text(content)`; keyset-paginated, `BACKFILL_BATCH = 500` (`:30`), idempotent.
- `:155` — `pub async fn backfill_chronicle_search_text(pool: &SqlitePool) -> Result<usize>` — mirrors the above for `channel_chronicle_checkpoints.search_text`.
- `:253` — `pub async fn backfill_search_text(pool) -> Result<usize>` — entry point calling both (`:254`), returns total.
- Test helpers: `pending_memories` `:273`, `search_text_of` `:282`, `save_legacy_memory` `:292`; tests `:304`–`:526`.
- **Module doc `:1`–`:16` explicitly says the `EmbeddingTable`/`ChronicleEmbeddingTable` are replaced by SQLite-backed equivalents "from stage 1 step B onward"** — this is the intended home for the new query path.
- Backfill is wired at boot: `backfill_search_text` is called from the boot path (see `src/memory/sqlite_index.rs:254`; the call site is in `main.rs`/`api/agents.rs` alongside `backfill_chronicle_embeddings` — confirm exact line when editing).

## 8. The four test constructors building `MemorySearch`

All in `src/memory/search.rs`, each builds a temp lance dir + `EmbeddingTable::open_or_create` + `MemorySearch::new`:
- `:714` — `test_metadata_search_recent` (ctor at `:725`).
- `:743` — `test_metadata_search_important` (ctor at `:753`).
- `:768` — `test_metadata_search_typed` (ctor at `:778`).
- `:793` — `test_metadata_search_typed_empty` (ctor at `:803`).
Shared helper: `:663` — `async fn setup_search_with_memories() -> (Arc<MemoryStore>, Vec<Memory>)`.
Each ctor block is the same 6 lines: `tempfile::tempdir()` → `lancedb::connect` → `EmbeddingTable::open_or_create` → `embedding::shared_test_model()` → `MemorySearch::new(...)`. When `MemorySearch::new`'s signature changes, these four are the compile breakages to fix (plus any in `consolidation.rs:455`/`maintenance.rs` fixtures, which build the table directly, not `MemorySearch`).

## 9. FTS5 / vector schema — the storage contract

Migration: `migrations/20260816000001_memory_search_sqlite.sql` (agent DB; **already applied, additive + inert**).

- `:38` — `CREATE TABLE memory_vectors (memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE, model TEXT NOT NULL, dim INTEGER NOT NULL, embedding BLOB NOT NULL, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`. Index `idx_memory_vectors_model` `:49`.
- `:57` — `CREATE TABLE chronicle_vectors (checkpoint_id TEXT PRIMARY KEY REFERENCES channel_chronicle_checkpoints(id) ON DELETE CASCADE, model, dim, embedding BLOB, created_at)`. Index `:65`.
- `:88` — `CREATE VIRTUAL TABLE memories_fts USING fts5(search_text, content='memories', content_rowid='rowid', tokenize = "unicode61 tokenchars '_-.'")` — external-content FTS5.
- `:143` — `CREATE VIRTUAL TABLE chronicle_fts USING fts5(search_text, content='channel_chronicle_checkpoints', content_rowid='rowid', tokenize = "unicode61 tokenchars '_-.'")`.
- Columns added: `ALTER TABLE memories ADD COLUMN search_text TEXT NOT NULL DEFAULT ''` (`:70`); `ALTER TABLE channel_chronicle_checkpoints ADD COLUMN search_text TEXT NOT NULL DEFAULT ''` (`:145`).
- Triggers (insert/delete/update) — `memories_fts_*` at `:100`, `:105`, `:125`; `chronicle_fts_*` at `:150`, `:155`, `:163`. **All guarded on `search_text <> ''` and the update triggers use the single-statement `INSERT ... SELECT 'delete' ... WHERE ...; INSERT ... SELECT ... WHERE ...` form** (both load-bearing — see migration comments). The index is maintained automatically: **the write path only needs to set `search_text` (via the `memories` row), never touch the FTS table.**
- **Model label for `memory_vectors.model`:** the migration comment (`:41`) names `"AllMiniLML6V2"` as the example; confirm against `src/memory/embedding.rs` before hardcoding.
- `migrations/global/20260816000002_wiki_fts_tokenchars.sql:45` — the wiki FTS rebuild uses the same `tokenize = "unicode61 tokenchars '_-.'"`.

## 10. Analyzer module (Stage 1B) — public functions + signatures

`src/memory/analyzer.rs`:
- `:60` — `pub fn simple_tokens(text: &str) -> Vec<String>` — split on non-alphanumeric (note: **does split identifiers**, `context_window` → 2 tokens; the whole-identifier form comes from the raw copy).
- `:85` — `pub fn analyze_tokens(text: &str) -> Vec<String>` — full tantivy-equivalent pipeline (length filter → lowercase → stem → stop-word → ascii-fold), `Stemmer::create(Algorithm::English)`.
- `:95` — `pub fn analyzed_text(content: &str) -> String` — `<stemmed tokens> <raw text>`; returns raw unchanged if no tokens.
- `:117` — `pub fn search_text(content: &str) -> String` — the value for the `search_text` column; returns `""` for tokenless content (so the FTS guards agree). **This is what the write path stores.**
- `:141` — `pub fn build_match_query(query: &str) -> Option<String>` — builds the FTS5 `MATCH` expression: every term double-quoted, joined with ` OR `, each token contributing raw+stemmed forms; `None` when no usable terms (**callers must treat `None` as "no results", never as an empty `MATCH`**).
- `:177` — `pub fn escape_term(term: &str) -> String` — quotes one term (doubles embedded `"`); `*` prefix goes *outside* the quotes.
- Private helpers: `analyzed_tokens_with` `:192`, `push_unique` `:216`, `ascii_fold` `:230`, `fold_char` `:243`. Consts `MAX_TOKEN_BYTES`, `ENGLISH_STOP_WORDS` referenced in `:141`–`:171`.

---

## Resolved during recon (was listed as unresolved, now pinned)

1. **Boot call site of `backfill_search_text`** — RESOLVED. Wrapper `MemorySearch::backfill_search_text` at `src/memory/search.rs:157` (delegates to `sqlite_index::backfill_search_text` at `:158`). Called at `src/api/agents.rs:884` and `src/main.rs:2350`. **The new query path's init/backfill hook joins these two lines.**
2. **`find_similar` is NOT dead code** — RESOLVED (corrects an earlier note). Five production callers:
   - `src/memory/maintenance.rs:234` — `embedding_table.find_similar(...)` (dedup scan).
   - `src/memory/maintenance.rs:613` — `.find_similar(&duplicate.id, 0.0, 10)`.
   - `src/memory/maintenance.rs:786` — `.find_similar(duplicate_id, 0.0, 10)`.
   - `src/memory/consolidation.rs:216` — `.find_similar(memory_id, threshold, 5)`.
   - `src/agent/cortex.rs:2367` — `.find_similar(memory_id, similarity_threshold, 10)`.
   → **The SQLite replacement must implement `find_similar` too** (returns `(id, similarity)` where `similarity = 1.0 - distance`, excludes the source id, `threshold` filter). Do NOT drop it in Stage 3.
3. **`memory_vectors.model` label** — RESOLVED as "default model, not a literal string." `EmbeddingModel::new` (`src/memory/embedding.rs:19`) builds `fastembed::TextEmbedding::try_new(InitOptions::default())` — no explicit model id, so fastembed's default (`AllMiniLML6V2`, 384-dim) is in force. The migration comment's `"AllMiniLML6V2"` is consistent. Store a stable label (e.g. `"AllMiniLML6V2"`) rather than reading it off fastembed.
4. **`MemoryStore::pool()`** — `src/memory/store.rs:45` is `pub(crate) fn pool(&self) -> &SqlitePool`. Sufficient for the in-crate new query path (call with `self.store.pool()`); no external-crate caller found. If the query path lives in `sqlite_index.rs` it receives `&SqlitePool` directly, exactly like the backfills.
5. **`ChronicleEmbeddingTable::has` (`src/memory/lance.rs:648`)** — no caller found in `src/agent/chronicle.rs` or `src/conversation/`. Grep the whole tree (`grep -rn '\.has(' src/`) before deleting in Stage 3; likely a Stage 3 drop.
6. **`store.get_high_importance` / `traverse_graph`** — SQLite-only today (graph step of hybrid), so they stay; no lance dependency to migrate.

## Residual unknowns (genuinely not resolved by recon)

- **`ChronicleEmbeddingTable::has` exact caller** — not found; treat as Stage 3 deletion candidate pending a full-tree grep.
- **`memory_vectors.model` vs. actual fastembed default** — the default is `AllMiniLML6V2` by fastembed convention, not asserted from source (the crate's default is not in this repo). A stable hardcoded label is the safe choice.
