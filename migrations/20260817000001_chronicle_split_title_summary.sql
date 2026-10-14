-- Split the chronicle FTS index's single combined `search_text` column into two
-- per-column FTS columns (`title`, `summary`).
--
-- Why: `migrations/20260816000001_memory_search_sqlite.sql` indexed a checkpoint
-- as ONE document — `search_text` = analysed(`title || ' ' || summary`) — and
-- scored it with an un-weighted `bm25(chronicle_fts)`. Folding two fields into
-- one document has three costs the read path now needs to avoid:
--
--   * no weighting — a hit in the title cannot be scored differently from a hit
--     in the (much longer) summary, so title terms are diluted by summary length;
--   * proximity leakage — a phrase spanning the title/summary boundary matches as
--     if the two fields were adjacent prose (they are simply concatenated);
--   * no per-column filtering — a query cannot be restricted to the title.
--
-- The source-of-truth table `channel_chronicle_checkpoints` already stores
-- `title` and `summary` in SEPARATE columns, so no data-table rebuild is needed:
-- the fix is entirely in the (derived, rebuildable) FTS index. The two analysed
-- values are computable from the row's own `title`/`summary` via the Rust
-- analyzer, and `backfill_chronicle_search_text` rewrites them on boot exactly
-- as it does today.

------------------------------------------------------------------------------
-- 1. Retire the single-column index
------------------------------------------------------------------------------
-- FTS5 virtual tables cannot ALTER their column set, so the split index is a new
-- virtual table. Dropping the old one is safe: it is a derived index rebuilt from
-- `channel_chronicle_checkpoints` (external content), never a source of truth.
-- The old triggers must go first so they cannot fire against the dropped table.
DROP TRIGGER IF EXISTS chronicle_fts_insert;
DROP TRIGGER IF EXISTS chronicle_fts_delete;
DROP TRIGGER IF EXISTS chronicle_fts_update;
DROP TABLE IF EXISTS chronicle_fts;

-- The now-unused combined column, and its per-row storage. `search_text` is
-- dropped here because every reader/writer moved to `search_title`/
-- `search_summary`; leaving it would keep a second, conflicting index source
-- alive. SQLite's `DROP COLUMN` requires the column to participate in no index,
-- trigger, or virtual table — satisfied by the drops above, and `search_text`
-- was the last column in the table (the ADD COLUMN migration appended it), which
-- `DROP COLUMN` also requires. Verified on SQLite 3.53.1, the version this repo's
-- tests run against.
ALTER TABLE channel_chronicle_checkpoints DROP COLUMN search_text;

------------------------------------------------------------------------------
-- 2. Per-column index
------------------------------------------------------------------------------
-- Two indexed columns, each holding the analysed form of its own field:
-- `<stemmed tokens> <raw text>` (see src/memory/analyzer.rs). Keeping the two
-- fields in separate FTS columns is what lets bm25 weight them independently
-- (title terms weigh more) and what removes the cross-field proximity leak.
--
-- External content over `channel_chronicle_checkpoints`, exactly as before, so
-- the index can never disagree with its row.
CREATE VIRTUAL TABLE IF NOT EXISTS chronicle_fts USING fts5(
    title,
    summary,
    content='channel_chronicle_checkpoints',
    content_rowid='rowid',
    tokenize = "unicode61 tokenchars '_-.'"
);

-- One analysed column per field. Named `search_*` (not `title`/`summary`) so the
-- analysed value never collides with the raw `title`/`summary` columns the FTS
-- table shadows by name.
ALTER TABLE channel_chronicle_checkpoints ADD COLUMN search_title TEXT NOT NULL DEFAULT '';
ALTER TABLE channel_chronicle_checkpoints ADD COLUMN search_summary TEXT NOT NULL DEFAULT '';

-- The `<> ''` guards are load-bearing for the same reason documented on the
-- `memories_fts` triggers: FTS5 never stores a document with no tokens, so
-- issuing `'delete'` for a row the index never held corrupts it.
--
-- A row is indexed if EITHER field carries tokens; an empty field is written as
-- '' and contributes nothing. The insert/delete use both columns verbatim so the
-- 'delete' command can find the document the insert created.
CREATE TRIGGER IF NOT EXISTS chronicle_fts_insert AFTER INSERT ON channel_chronicle_checkpoints
WHEN new.search_title <> '' OR new.search_summary <> '' BEGIN
    INSERT INTO chronicle_fts(rowid, title, summary)
    VALUES (new.rowid, new.search_title, new.search_summary);
END;

CREATE TRIGGER IF NOT EXISTS chronicle_fts_delete AFTER DELETE ON channel_chronicle_checkpoints
WHEN old.search_title <> '' OR old.search_summary <> '' BEGIN
    INSERT INTO chronicle_fts(chronicle_fts, rowid, title, summary)
    VALUES ('delete', old.rowid, old.search_title, old.search_summary);
END;

-- Unindex-then-reindex in ONE trigger, scoped to the two analysed columns, for
-- the ordering and staleness reasons documented on `memories_fts_update`.
CREATE TRIGGER IF NOT EXISTS chronicle_fts_update AFTER UPDATE OF search_title, search_summary ON channel_chronicle_checkpoints BEGIN
    INSERT INTO chronicle_fts(chronicle_fts, rowid, title, summary)
        SELECT 'delete', old.rowid, old.search_title, old.search_summary
         WHERE old.search_title <> '' OR old.search_summary <> '';
    INSERT INTO chronicle_fts(rowid, title, summary)
        SELECT new.rowid, new.search_title, new.search_summary
         WHERE new.search_title <> '' OR new.search_summary <> '';
END;

------------------------------------------------------------------------------
-- 3. Transitional backfill (RAW COPY ONLY — REQUIRED RUST BACKFILL BELOW)
------------------------------------------------------------------------------
-- Two analysed columns, one per field. SQL cannot stem, so this only copies the
-- raw values; the Rust backfill (`backfill_chronicle_search_text`) rewrites each
-- column with `analyzer::search_text(field)` and is idempotent.
--
-- The "already analysed" test cannot be a plain equality against the raw source
-- (an analysed value CONTAINS the raw text but is prefixed by its stems), so the
-- backfill detects un-analysed rows by `search_title = title` instead.
UPDATE channel_chronicle_checkpoints
   SET search_title = title, search_summary = summary
 WHERE search_title = '';
