-- Stage 1 (step A) of moving memory + chronicle search off LanceDB onto SQLite.
--
-- This migration is ADDITIVE and INERT: it creates the SQLite-side storage that
-- will replace the LanceDB `memory_embeddings` and `chronicle_embeddings` tables,
-- but
-- nothing reads or writes any of it yet. The live search path (`src/memory/lance.rs`,
-- `src/memory/search.rs`) is deliberately untouched so this release is a runtime
-- no-op. The read path switches over in a later step, and only after that is
-- `lancedb` / `lance-index` removed from Cargo.toml.
--
-- Why: `lancedb` + `lance-index` pull in ~185 crates (DataFusion, arrow, the geo
-- stack, tantivy + 9 siblings). `lance-index` is a *direct* dependency used for a
-- single type (`lance_index::scalar::FullTextSearchQuery`), and `create_indexes()`
-- has zero callers — so the ANN index never runs and today's vector search is
-- already an exact brute-force scan. Storing the vectors in SQLite alongside their
-- own rows is therefore not a ranking change, and it removes the dual-write +
-- `compensate_embedding_failure` drift class entirely.
--
-- Naming: the SQLite tables are `memory_vectors` / `chronicle_vectors`, NOT the
-- LanceDB names `memory_embeddings` / `chronicle_embeddings` (see `TABLE_NAME`
-- and `CHRONICLE_TABLE_NAME` in src/memory/lance.rs). Both stores coexist for
-- the length of this migration, so sharing a name would make "which store does
-- this insert go to?" ambiguous at exactly the moment the migration is trying
-- to eliminate that class of bug.

------------------------------------------------------------------------------
-- 1. Vector storage (replaces the LanceDB `memory_embeddings` table)
------------------------------------------------------------------------------
-- `EmbeddingTable` in src/memory/lance.rs stores (id, content, embedding) where
-- `content` is duplicated into Lance *only* so the full-text index has something
-- to index. Here the text lives where it already lives (`memories.content`) and
-- only the vector is stored, keyed by the memory it describes.
--
-- `model` + `dim` are recorded so a future embedding-model change is detectable
-- rather than silently mixing incompatible vectors in one column. fastembed
-- L2-normalizes on both the write and query paths, so a vector read back as f32
-- and scored by dot product is numerically identical to today's cosine search.
CREATE TABLE IF NOT EXISTS memory_vectors (
    memory_id  TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
    -- Embedding model identifier, e.g. "AllMiniLML6V2".
    model      TEXT NOT NULL,
    -- Vector length, so a dimension change is detectable without decoding.
    dim        INTEGER NOT NULL,
    -- f32 little-endian, `dim` * 4 bytes, memcpy'd from the fastembed output.
    embedding  BLOB NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_memory_vectors_model ON memory_vectors(model);

------------------------------------------------------------------------------
-- 2. Vector storage (replaces the LanceDB `chronicle_embeddings` table)
------------------------------------------------------------------------------
-- `ChronicleEmbeddingTable` stores (id, title, channel_id, seq, text, embedding)
-- over `channel_chronicle_checkpoints`. As above, only the vector is new storage;
-- title/channel/seq already exist on the checkpoint row.
CREATE TABLE IF NOT EXISTS chronicle_vectors (
    checkpoint_id TEXT PRIMARY KEY REFERENCES channel_chronicle_checkpoints(id) ON DELETE CASCADE,
    model         TEXT NOT NULL,
    dim           INTEGER NOT NULL,
    embedding     BLOB NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_chronicle_vectors_model ON chronicle_vectors(model);

------------------------------------------------------------------------------
-- 3. Full-text search over memories (replaces the LanceDB FTS index)
------------------------------------------------------------------------------
-- The indexed text is a Rust-computed column, not `content` itself, because
-- FTS5 has no stemmer: `src/memory/analyzer.rs` writes the stemmed tokens plus
-- the raw text into it. The raw copy is what lets the `tokenchars` tokenizer
-- below treat `context_window` / `glm-5.3-flash` as single tokens, while the
-- stemmed copy preserves the split-and-stemmed matches the current tantivy
-- index produces. Both forms in one column means a query can hit either.
ALTER TABLE memories ADD COLUMN search_text TEXT NOT NULL DEFAULT '';

-- External content: the index stores only the inverted lists, and column values
-- are read back from `memories` on demand (needed for bm25/highlight). This is
-- the same pattern `migrations/global/20260407120000_wiki.sql` already uses for
-- `wiki_pages_fts`, and it is why the index can never disagree with the row: the
-- triggers below fire inside the caller's own transaction.
--
-- `tokenchars '_-.'` keeps identifier-shaped tokens whole. Note the asymmetry
-- with `_` and `.`: FTS5's *tokenizer* treats them as token characters here, but
-- its *query parser* still interprets `-` and `.` as operators, so all query
-- terms must be double-quoted (`analyzer::build_match_query` does this).
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    search_text,
    content='memories',
    content_rowid='rowid',
    tokenize = "unicode61 tokenchars '_-.'"
);

CREATE TRIGGER IF NOT EXISTS memories_fts_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, search_text) VALUES (new.rowid, new.search_text);
END;

CREATE TRIGGER IF NOT EXISTS memories_fts_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, search_text)
    VALUES ('delete', old.rowid, old.search_text);
END;

-- Fires for every UPDATE on `memories`, not only ones touching `search_text`.
-- That is intentional and matches the wiki pattern: it keeps the index correct
-- if `content` changes without `search_text` being rewritten in the same statement.
CREATE TRIGGER IF NOT EXISTS memories_fts_update AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, search_text)
    VALUES ('delete', old.rowid, old.search_text);
    INSERT INTO memories_fts(rowid, search_text) VALUES (new.rowid, new.search_text);
END;

------------------------------------------------------------------------------
-- 4. Full-text search over chronicle checkpoints
------------------------------------------------------------------------------
-- Mirrors the above for `ChronicleEmbeddingTable`, whose Lance FTS index is on
-- its `text` column. `search_text` is analysed from title + summary.
--
-- Assumption to confirm at step B: the Lance `text` field is derived from the
-- checkpoint's title/summary. If it carries anything else, this FTS column and
-- the write path must be widened together.
ALTER TABLE channel_chronicle_checkpoints ADD COLUMN search_text TEXT NOT NULL DEFAULT '';

CREATE VIRTUAL TABLE IF NOT EXISTS chronicle_fts USING fts5(
    search_text,
    content='channel_chronicle_checkpoints',
    content_rowid='rowid',
    tokenize = "unicode61 tokenchars '_-.'"
);

CREATE TRIGGER IF NOT EXISTS chronicle_fts_insert AFTER INSERT ON channel_chronicle_checkpoints BEGIN
    INSERT INTO chronicle_fts(rowid, search_text) VALUES (new.rowid, new.search_text);
END;

CREATE TRIGGER IF NOT EXISTS chronicle_fts_delete AFTER DELETE ON channel_chronicle_checkpoints BEGIN
    INSERT INTO chronicle_fts(chronicle_fts, rowid, search_text)
    VALUES ('delete', old.rowid, old.search_text);
END;

CREATE TRIGGER IF NOT EXISTS chronicle_fts_update AFTER UPDATE ON channel_chronicle_checkpoints BEGIN
    INSERT INTO chronicle_fts(chronicle_fts, rowid, search_text)
    VALUES ('delete', old.rowid, old.search_text);
    INSERT INTO chronicle_fts(rowid, search_text) VALUES (new.rowid, new.search_text);
END;

------------------------------------------------------------------------------
-- 5. Transitional backfill (RAW COPY ONLY — REQUIRED RUST BACKFILL BELOW)
------------------------------------------------------------------------------
-- RUST BACKFILL REQUIRED: stemming cannot be done in SQL, so this backfill only
-- copies the raw text. Until the Rust backfill runs, an existing row's
-- `search_text` holds the unstemmed text, which means:
--
--   * whole-word and identifier queries work (`memory`, `context_window`);
--   * stem-sharing queries do NOT yet match (a query for `memories` will not
--     find a row containing `memory`, because the stemmed half is missing).
--
-- The Rust backfill MUST rewrite `search_text` for every row with
-- `analyzer::analyzed_text(content)`; because the UPDATE trigger above fires on
-- any UPDATE, that rewrite reindexes the row in the same transaction. It must
-- run to completion BEFORE the read path switches to these tables, and it is
-- idempotent: recomputing `analyzed_text` over an already-analysed value is
-- harmless because the raw copy is emitted verbatim alongside the stems.
--
-- Vectors have the same requirement and are NOT backfilled here: they need the
-- embedding model, so they are filled by the existing embedding backfill path
-- once it is pointed at `memory_vectors`. Text search therefore works
-- immediately while vector search is still warming up — degrade, don't fail.
UPDATE memories SET search_text = content WHERE search_text = '';

UPDATE channel_chronicle_checkpoints
   SET search_text = title || ' ' || summary
 WHERE search_text = '';