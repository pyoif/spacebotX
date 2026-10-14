# bollard removal — state notes (2026-10-02)

Repo: /data/agents/main/workspace/spacebotX (HEAD 9a45624b, working tree DIRTY with bollard-removal edits)

## Blocked by
Process table exhausted (`Failed to spawn command: Disk quota exceeded (os error 122)`).
Same zombie-bwrap PID-pool issue as before. Cannot run cargo check or git push from the
sandbox until the daemon is restarted (which clears zombies) or the PID pool is freed.

## Edits made (working tree, UNCOMMITTED, UNVERIFIED by compiler)
- src/update.rs:
  - module doc rewritten (no Docker self-update)
  - new_shared_status(): Docker branch now can_apply=false + SELF_UPDATE_DISABLED_REASON
  - added SELF_UPDATE_DISABLED_REASON const + pub fn self_update_unavailable_reason()
  - removed docker_socket_available, detect_current_docker_image, apply_docker_update,
    get_own_container_id, resolve_target_image, the old async detect_apply_capability
  - added bollard-free sync detect_apply_capability(deployment) (always can_apply=false)
  - check_for_update: call site updated `.await` -> sync
  - tests: removed test_resolve_target_image; added apply_capability_is_disabled_everywhere
- src/api/settings.rs: update_apply() no longer calls apply_docker_update; returns an
  honest error JSON with self_update_unavailable_reason()
- src/cli/update.rs: Apply subcommand help text updated (was "Pull the new image...")
- Cargo.toml: bollard line already absent (removed earlier)

## Caller audit (all hand-verified clean, no removed symbols referenced)
- src/api/settings.rs -> updated (uses self_update_unavailable_reason)
- src/cli/update.rs -> reads raw JSON only (status/error), compiles unchanged
- src/agent/status.rs -> uses Deployment only (kept)
- src/self_awareness.rs -> uses Deployment only (kept)
- Cargo.toml -> no bollard; semver kept

## Remaining to do once shell works
1. cargo check --all-targets (via: bash -c 'source ./build-env.sh && hash -r && cargo check --all-targets')
2. grep -rn bollard src/ Cargo.toml  -> expect zero code refs
3. git add -A (exclude build-env.sh) && commit "refactor: remove bollard / Docker self-update" && push origin main
4. ls-remote verify local == remote
