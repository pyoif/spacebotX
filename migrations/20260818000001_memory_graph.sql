-- Graph-structured memory: a `memory_nodes` table (one row per memory entry) and
-- a `memory_edges` table (typed, directed relations between entries), both inside
-- the EXISTING SQLite database — no external graph store.
--
-- Why: the store already carries supersede relationships, but only as prose inside
-- the new entry's `content` ("... supersedes the earlier dispatch-only note").
-- Nothing links the two rows, so at recall time the superseded predecessor is
-- still a perfectly valid FTS/vector hit and can win a top-k slot, surfacing a
-- stale fact as current. Modelling the relation as an edge makes it *queryable*:
-- recall walks `supersedes` edges transitively (WITH RECURSIVE) and drops the
-- predecessors.
--
-- Scope: `kind` starts with just 'supersedes' (src supersedes dst). The tables are
-- generic — later relation kinds are added by writing more rows, not by migrating.
--
-- Conventions follow the existing memory migrations: `TEXT` entry ids (memories.id
-- is TEXT), an index on the direction recall walks (dst), and foreign keys are
-- declared DEFERRED-off/no-enforcement exactly as `20260211000001_memories.sql`
-- does for `associations` (sqlx runs SQLite with `foreign_keys` off by default;
-- the association table's FK is likewise declarative only). `entry_id` is kept
-- loose — NOT a hard FK to `memories(id)` — so a graph node/edge can be recorded
-- for an entry that is later forgotten or merged without cascading the graph away.

------------------------------------------------------------------------------
-- 1. Nodes: one row per memory entry that participates in the graph
------------------------------------------------------------------------------
-- `entry_id` is `memories.id`. `kind` mirrors the entry's own memory_type by
-- default ('fact') but is a free label — a node may be re-kinded to e.g.
-- 'checkpoint' without touching the memories table.
CREATE TABLE IF NOT EXISTS memory_nodes (
    id         INTEGER PRIMARY KEY,
    entry_id   TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL DEFAULT 'fact',
    label      TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

------------------------------------------------------------------------------
-- 2. Edges: typed, directed relations between entries
------------------------------------------------------------------------------
-- For kind = 'supersedes', src supersedes dst: src is the NEWER entry, dst the
-- predecessor. Recall starts from a surviving entry and follows edges in the
-- dst direction to collect transitive predecessors.
--
-- The UNIQUE(src_entry_id, dst_entry_id, kind) constraint matches the shape of
-- the existing `associations` table and makes edge capture idempotent — the same
-- heuristically-parsed supersede reference re-parsed on a re-write is a no-op.
CREATE TABLE IF NOT EXISTS memory_edges (
    id            INTEGER PRIMARY KEY,
    src_entry_id  TEXT NOT NULL,
    dst_entry_id  TEXT NOT NULL,
    kind          TEXT NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(src_entry_id, dst_entry_id, kind)
);

-- Recall walks *backwards* along supersedes edges — "who did I supersede?" — so
-- the lookup is by dst_entry_id, and that is the index it needs.
CREATE INDEX IF NOT EXISTS idx_memory_edges_dst ON memory_edges(dst_entry_id);
-- Secondary: a diagnostic/forward query ("what does this entry supersede?") and
-- the UNIQUE constraint's own lookup both benefit from src being indexed too.
CREATE INDEX IF NOT EXISTS idx_memory_edges_src ON memory_edges(src_entry_id);
