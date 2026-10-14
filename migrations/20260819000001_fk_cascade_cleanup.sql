-- Enforce the memory satellite FKs for real: rebuild `memory_nodes` and
-- `memory_edges` with hard `REFERENCES memories(id) ON DELETE CASCADE`
-- constraints, so a memory hard-delete reclaims every row that points at it.
--
-- Why this migration exists
-- -------------------------
-- Migrations 20260816000001 (`memory_vectors`) and 20260812000002
-- (`human_identities`) already DECLARE `REFERENCES memories(id) ON DELETE
-- CASCADE`, and `associations` (20260211000001) declares the same on
-- source_id/target_id. None of those cascades ever fired: SQLite enforces
-- foreign keys only when `PRAGMA foreign_keys = ON`, which is a PER-CONNECTION
-- setting, and the daemon connected with a plain `SqlitePool::connect` that
-- never issued it (see `src/db.rs`). So the declared cascades were inert and
-- hard deletes left rows behind in every one of those tables.
--
-- The graph tables (`memory_nodes` / `memory_edges`, 20260818000001) were the
-- worst case: they declared NO foreign key at all, deliberately (see that
-- migration's note). The stated reason was that a graph node/edge must be able
-- to outlive an entry that is "forgotten or merged". That rationale only
-- half-holds: `forgotten = 1` is a SOFT delete — it is an UPDATE, not a
-- DELETE — so `ON DELETE CASCADE` never fires for it and forgotten entries
-- keep their graph rows either way. The concern that a cascade would erase
-- provenance on a forget/merge therefore does not apply; cascades fire only on
-- the real hard delete (prune), which is exactly when the rows ARE garbage.
-- The two endpoints are real references to `memories.id`, so they get real FKs.
--
-- With this migration plus `PRAGMA foreign_keys = ON` on every connection, a
-- plain `DELETE FROM memories WHERE id = ?` reclaims: associations,
-- memory_vectors, human_identities, memory_nodes, and every memory_edges row
-- touching the entry in either direction. The hand-written
-- `delete_with_graph_cleanup` is deleted in the same change.
--
-- What this migration does
-- ------------------------
-- 1. Pre-cleans any legacy orphan rows (rows whose referenced memory/node is
--    already gone). These are unreachable garbage produced by the inert-cascade
--    era; they would otherwise abort the constraint-checked rebuild below.
--    Expected scale: a handful of rows from historical hard deletes.
-- 2. Rebuilds `memory_nodes` and `memory_edges` (SQLite cannot ADD a constraint
--    to an existing table — create/copy/drop/rename is the only path) carrying
--    real FKs, preserving ids, data and indexes.
--
-- Note on `channel_chronicle_checkpoints.rolled_up_into`
-- ------------------------------------------------------
-- That column is a plain (nullable) `REFERENCES channel_chronicle_checkpoints(id)`
-- with no ON DELETE clause. Turning enforcement on makes it a NO ACTION
-- constraint, so deleting a checkpoint that a later rollup still points at
-- would fail. That is safe here: nothing in the codebase ever DELETEs a
-- checkpoint (`grep -rn "DELETE FROM channel_chronicle_checkpoints" src/` is
-- empty) — checkpoints are append-only and `rolled_up_into` is only ever SET.
-- Left as-is deliberately, so the FK still guards against an accidental
-- checkpoint delete rather than silently nulling provenance.

------------------------------------------------------------------------------
-- 1. Pre-clean legacy orphans (unreachable rows from the inert-cascade era)
------------------------------------------------------------------------------
-- An edge is reachable only if BOTH of its endpoints still exist; a node only
-- if its entry still exists. Anything else is garbage by definition.
DELETE FROM memory_edges
WHERE src_entry_id NOT IN (SELECT id FROM memories)
   OR dst_entry_id NOT IN (SELECT id FROM memories);

DELETE FROM memory_nodes
WHERE entry_id NOT IN (SELECT id FROM memories);

-- Same class for the tables that DID declare a cascade but never enforced it.
DELETE FROM memory_vectors
WHERE memory_id NOT IN (SELECT id FROM memories);

DELETE FROM human_identities
WHERE memory_id NOT IN (SELECT id FROM memories);

DELETE FROM associations
WHERE source_id NOT IN (SELECT id FROM memories)
   OR target_id NOT IN (SELECT id FROM memories);

------------------------------------------------------------------------------
-- 2a. Rebuild `memory_nodes` with a real FK on entry_id -> memories(id)
------------------------------------------------------------------------------
CREATE TABLE memory_nodes_new (
    id         INTEGER PRIMARY KEY,
    entry_id   TEXT NOT NULL UNIQUE REFERENCES memories(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL DEFAULT 'fact',
    label      TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO memory_nodes_new (id, entry_id, kind, label, created_at)
SELECT id, entry_id, kind, label, created_at FROM memory_nodes;

DROP TABLE memory_nodes;
ALTER TABLE memory_nodes_new RENAME TO memory_nodes;

------------------------------------------------------------------------------
-- 2b. Rebuild `memory_edges` with real FKs on both endpoints
------------------------------------------------------------------------------
CREATE TABLE memory_edges_new (
    id            INTEGER PRIMARY KEY,
    src_entry_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst_entry_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(src_entry_id, dst_entry_id, kind)
);

INSERT INTO memory_edges_new (id, src_entry_id, dst_entry_id, kind, created_at)
SELECT id, src_entry_id, dst_entry_id, kind, created_at FROM memory_edges;

DROP TABLE memory_edges;
ALTER TABLE memory_edges_new RENAME TO memory_edges;

-- Recreate the indexes the rebuild dropped (dst is the direction recall walks;
-- src backs the forward query and the UNIQUE lookup).
CREATE INDEX IF NOT EXISTS idx_memory_edges_dst ON memory_edges(dst_entry_id);
CREATE INDEX IF NOT EXISTS idx_memory_edges_src ON memory_edges(src_entry_id);
