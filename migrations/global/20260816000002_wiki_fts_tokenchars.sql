-- Wiki FTS: switch `wiki_pages_fts` to the identifier-preserving tokenizer.
--
-- `20260407120000_wiki.sql` created the table with FTS5's default tokenizer
-- (`unicode61`), which treats `_`, `-` and `.` as separators. A page containing
-- `context_window` or `glm-5.3-flash` is therefore indexed as fragments
-- (`context`, `window`, `glm`, `5`, `3`, `flash`) and cannot be found by its own
-- identifier — searching for the exact term the page is *about* returns
-- everything that merely mentions a fragment, or nothing at all.
--
-- This mirrors `20260816000001_memory_search_sqlite.sql`, so the wiki and the
-- memory index tokenize identically. Two indexes in one product disagreeing
-- about what a token is would be a silent, permanent behaviour split.
--
-- FTS5 has no `ALTER ... tokenize`, so the index is rebuilt: create the
-- replacement, repopulate from the content table, swap, recreate the triggers.
--
-- `tokenchars '_-.'` — `.` as well as `_` and `-`, because version-shaped
-- identifiers (`qwen3.8-flash`, `deepseek-v4.1-flash`) are exactly the kind of
-- term a wiki page is titled after, and the query side
-- (`src/wiki/store.rs::sanitize_fts_query`) preserves all three characters. The
-- tokenizer and the query builder must agree; they do now.
--
-- FTS5's *query parser* still treats `-`, `.` and `:` as operators even when
-- they are token characters, which is why `sanitize_fts_query` double-quotes
-- every term. See that function for the escaping rules.

-- Triggers go first: they reference the table being replaced, and a rebuild
-- through them would re-enter the index while it is being swapped.
DROP TRIGGER IF EXISTS wiki_pages_fts_insert;
DROP TRIGGER IF EXISTS wiki_pages_fts_update;
DROP TRIGGER IF EXISTS wiki_pages_fts_delete;

-- External content (`content='wiki_pages'`): the index stores only inverted
-- lists and reads column values back from `wiki_pages`. Dropping the table
-- therefore destroys no content — only a derived index that 'rebuild' recreates
-- in the next statement. This is why the swap is safe on a live database.
DROP TABLE IF EXISTS wiki_pages_fts;

CREATE VIRTUAL TABLE IF NOT EXISTS wiki_pages_fts USING fts5(
    slug UNINDEXED,
    title,
    content,
    content='wiki_pages',
    content_rowid='rowid',
    tokenize = "unicode61 tokenchars '_-.'"
);

-- Repopulate from `wiki_pages`. Runs inside the migration transaction, so the
-- index is never visible in a partially-built state.
INSERT INTO wiki_pages_fts(wiki_pages_fts) VALUES('rebuild');

-- Triggers restored byte-identically to the versions in `20260407120000_wiki.sql`
-- apart from the table they target, which is the same table name as before — the
-- swap is invisible to every existing callsite.
CREATE TRIGGER IF NOT EXISTS wiki_pages_fts_insert AFTER INSERT ON wiki_pages BEGIN
    INSERT INTO wiki_pages_fts(rowid, slug, title, content) VALUES (new.rowid, new.slug, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_fts_update AFTER UPDATE ON wiki_pages BEGIN
    INSERT INTO wiki_pages_fts(wiki_pages_fts, rowid, slug, title, content) VALUES ('delete', old.rowid, old.slug, old.title, old.content);
    INSERT INTO wiki_pages_fts(rowid, slug, title, content) VALUES (new.rowid, new.slug, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_fts_delete AFTER DELETE ON wiki_pages BEGIN
    INSERT INTO wiki_pages_fts(wiki_pages_fts, rowid, slug, title, content) VALUES ('delete', old.rowid, old.slug, old.title, old.content);
END;