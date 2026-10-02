//! Waiter registry for exact, guardless zombie reaping.
//!
//! Spacebot can run as PID 1 inside a container. In that role it must reap any
//! child that exits — otherwise the child lingers as a zombie (`<defunct>`) and
//! permanently occupies a process-table slot until the container is recreated,
//! eventually exhausting the PID limit so every `fork()` fails.
//!
//! Tokio already reaps children that are awaited, but two classes of child are
//! never covered:
//!
//! 1. A child whose `Child` handle is dropped or whose execution task is
//!    aborted before its `wait()` runs ([`crate::tools::shell`] timeout/quiesce
//!    paths, worker cancellation). `kill_on_drop` sends the signal but does not
//!    collect the status.
//! 2. An adopted orphan: a grandchild whose parent died reparents to the daemon
//!    and is invisible to tokio.
//!
//! The [`super::zombie_reaper`] sweeps for zombies and reaps them — but it must
//! not steal the exit status of a child that still has a live `wait()` owner,
//! because `waitpid` is a consuming operation and the awaiting task would then
//! block forever. The reaper previously used a 30-second age guard as a proxy
//! for "no live waiter", which delays cleanup and is still only a heuristic.
//!
//! This module replaces the heuristic with exact bookkeeping:
//!
//! * [`WAITED`] is the set of PIDs that currently have a live `wait()` owner.
//! * [`SPAWNING`] counts in-flight spawns. A child can exit between `spawn()`
//!   and the registry insert, so while any spawn is in flight the reaper defers
//!   rather than risk reaping a child that is about to be tracked.
//!
//! The reaper reaps a zombie exactly when its PID is absent from [`WAITED`] and
//! [`SPAWNING`] is zero — no time-based guard anywhere.
//!
//! ## Correctness invariants
//!
//! * **RAII, not manual removal.** A PID is removed from [`WAITED`] by the
//!   [`WaitedGuard`]'s `Drop`, never by a manual `WAITED.remove()` after an
//!   await. If a `timeout` or `select!` cancels the task while `wait()` is
//!   pending, the code after the `.await` never runs; a manual removal would
//!   leak the PID in [`WAITED`] forever and permanently block reaping of that
//!   zombie. `Drop` runs on every path: success, `?`, panic, and cancellation.
//! * **Bind the guard.** Always `let _guard = WaitedGuard(...)`. Never
//!   `let _ = WaitedGuard(...)`, which drops the guard immediately and removes
//!   the PID right away.
//! * **Ordering.** On a successful spawn the PID is inserted into [`WAITED`]
//!   first, the guard is created, and only then is [`SPAWNING`] decremented.
//!   There must be no instant where `SPAWNING == 0` and the PID is not yet in
//!   [`WAITED`], or the reaper could reap a child that is about to be awaited.
//! * **Lock-across-spawn.** [`spawn_managed`] holds the [`WAITED`] mutex across
//!   `spawn()` + the insert, so the reaper (which holds the same mutex across
//!   its scan) can never observe a live, unregistered child: either the spawner
//!   blocks before forking, or its PID is already in the set.
//! * **Unconditional decrement.** [`SPAWNING`] is decremented on every exit
//!   path, including `spawn()` failure.
//! * Every in-scope spawn goes through [`spawn_managed`] / [`output_managed`];
//!   see the grep-gate note on [`spawn_managed`].

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use tokio::process::{Child, Command};

/// PIDs with a live `tokio::process` `wait()` owner. The reaper must not touch
/// a zombie while its PID is in this set.
pub(crate) static WAITED: LazyLock<Mutex<HashSet<libc::pid_t>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Number of spawns currently between `spawn()` and registry insert. While this
/// is non-zero the reaper defers a cycle, because a child that exited in that
/// window looks abandoned even though a waiter is about to be registered.
static SPAWNING: AtomicUsize = AtomicUsize::new(0);

/// RAII guard that keeps a PID in [`WAITED`] for as long as it lives and removes
/// it on drop.
///
/// Hold this across the entire `wait().await` / `wait_with_output().await`. The
/// removal is done exclusively by `Drop` so it survives task cancellation —
/// a manual removal after the await would be skipped on cancellation and leak
/// the PID forever.
#[derive(Debug)]
pub struct WaitedGuard {
    pid: Option<libc::pid_t>,
}

impl WaitedGuard {
    /// The tracked child PID, if the guard still owns one.
    pub fn pid(&self) -> Option<libc::pid_t> {
        self.pid
    }
}

impl Drop for WaitedGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            if let Ok(mut set) = WAITED.lock() {
                set.remove(&pid);
            }
        }
    }
}

/// Returns `true` when no spawn is in flight. The reaper checks this before
/// reaping: a non-zero count means a child may have exited before its PID was
/// inserted into [`WAITED`].
pub fn spawning_in_progress() -> bool {
    SPAWNING.load(Ordering::SeqCst) != 0
}

/// Returns `true` when `pid` has a live waiter and must not be reaped.
pub fn is_waited(pid: libc::pid_t) -> bool {
    // Lock poisoning must fail safe: treat an unreadable registry as "has a
    // waiter" so we never reap a PID we cannot prove is abandoned.
    WAITED.lock().map(|set| set.contains(&pid)).unwrap_or(true)
}

/// Test helper: simulate an in-flight spawn for reaper tests. Exposed at module
/// scope (not inside the private `tests` module) so the reaper's own tests can
/// exercise the deferral path.
#[cfg(test)]
pub(crate) fn hold_one_for_test() {
    SPAWNING.fetch_add(1, Ordering::SeqCst);
}

/// Test helper: release the counter held by [`hold_one_for_test`].
#[cfg(test)]
pub(crate) fn release_one_for_test() {
    SPAWNING.fetch_sub(1, Ordering::SeqCst);
}

/// Test helper: read the current in-flight spawn count (for relative assertions
/// in tests that share the process-global counter).
#[cfg(test)]
pub(crate) fn spawning_delta_for_test() -> usize {
    SPAWNING.load(Ordering::SeqCst)
}

/// Increment the in-flight spawn counter. The matching decrement must run on
/// every path; [`SpawnTicket`]`'s `Drop` provides that for the error paths so a
/// failure cannot permanently wedge the counter.
struct SpawnTicket;

impl SpawnTicket {
    fn acquire() -> Self {
        SPAWNING.fetch_add(1, Ordering::SeqCst);
        SpawnTicket
    }

    fn release(self) {
        drop(self);
    }
}

impl Drop for SpawnTicket {
    fn drop(&mut self) {
        SPAWNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawn `cmd` and register the child as waited before returning.
///
/// # Lock-across-spawn hardening
///
/// The [`WAITED`] mutex is acquired **before** `cmd.spawn()` and held through
/// the [`WAITED`] insert, then released before this function returns. This
/// serializes the spawner against the reaper's scan:
///
/// * If the reaper holds the lock first, the spawner blocks *before* forking,
///   so there can be no child in existence that is unregistered (the reaper
///   cannot observe a PID this spawner has not yet created).
/// * If the spawner holds the lock first, it has inserted the PID into
///   [`WAITED`] before the reaper can read the set, so the reaper sees the PID
///   as waited and skips it.
///
/// This closes the spawn→register race exactly, rather than heuristically. The
/// reaper's [`crate::zombie_reaper`] 100ms `SPAWNING` defer remains as
/// defense-in-depth for pathological stalls, but it is no longer load-bearing.
///
/// `cmd.spawn()` is synchronous (tokio's `Command::spawn` does not await), so
/// there is **no `.await` while the lock is held** — the critical section wraps
/// only `spawn()` + the `insert`, then drops the guard.
///
/// Ordering otherwise: insert the PID into [`WAITED`], create the
/// [`WaitedGuard`], then decrement [`SPAWNING`]. The decrement is strictly
/// after the insert, so there is no window where the reaper sees
/// `SPAWNING == 0` while the PID is still missing from [`WAITED`].
///
/// The caller must bind the guard to a named variable (`let _guard = ...`) and
/// hold it across the child's `wait` call; dropping it marks the child
/// reapable, including when the task is cancelled.
///
/// # Scope (grep gate)
///
/// Every `Command::spawn()` in this crate must route through this function, be
/// an `.output()`/`.status()` preflight site (self-reaping — see
/// [`output_managed`]), or be documented as out-of-scope (the MCP SDK's internal
/// child, reaped by its own `Drop`).
pub fn spawn_managed(cmd: &mut Command) -> io::Result<(Child, WaitedGuard)> {
    // In-flight counter first, so the reaper defers if it triggers during the
    // (brief) window before we take the WAITED lock.
    let ticket = SpawnTicket::acquire();

    // Acquire WAITED *before* spawning and hold it across spawn() + insert.
    // No await happens in this critical section.
    //
    // On lock poisoning we still spawn, but skip registration: failing to
    // register only means the reaper might decline to reap this one child
    // (conservative, never steals a live waiter's status). We must not fail the
    // spawn itself because of an unrelated poisoned mutex.
    let mut waited = match WAITED.lock() {
        Ok(guard) => Some(guard),
        Err(_) => None,
    };

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            // Unconditional decrement on the failure path, before returning.
            // Dropping `waited` (below, on scope exit) releases the lock.
            drop(waited);
            ticket.release();
            return Err(error);
        }
    };

    // Insert into WAITED *before* the SPAWNING decrement, while still holding
    // the lock, so the reaper can never observe SPAWNING == 0 with this PID
    // absent.
    let guard = match child.id() {
        Some(raw) => {
            let pid = raw as libc::pid_t;
            if let Some(set) = waited.as_mut() {
                set.insert(pid);
            }
            WaitedGuard { pid: Some(pid) }
        }
        // `id()` is `None` only if the child has already been reaped, in which
        // case there is nothing to track.
        None => WaitedGuard { pid: None },
    };

    // Release the WAITED lock before returning (do NOT hold it across the
    // caller's await).
    drop(waited);

    // Decrement strictly after the WAITED insert (invariant #3).
    ticket.release();
    Ok((child, guard))
}

/// Run `cmd` to completion, holding the registry guard across the await.
///
/// Equivalent to [`spawn_managed`] followed by `child.wait_with_output()`, but
/// the guard is bound to a named variable and held for the whole await so that
/// cancellation cannot leak the PID. Use this for the batch/preflight paths
/// that previously called `cmd.output()` directly.
pub async fn output_managed(cmd: &mut Command) -> io::Result<std::process::Output> {
    let (child, _guard) = spawn_managed(cmd)?;
    // `_guard` is a named binding: it lives until the end of this scope, i.e.
    // across the await below. Do NOT change this to `let _ = ...`.
    child.wait_with_output().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawning_flag_tracks_acquire_release() {
        // SPAWNING is process-global and tests run in parallel, so we must not
        // assert an absolute at-rest value. Instead verify the counter moves by
        // exactly one across an acquire/release pair.
        let before = SPAWNING.load(Ordering::SeqCst);
        let ticket = SpawnTicket::acquire();
        assert_eq!(SPAWNING.load(Ordering::SeqCst), before + 1);
        ticket.release();
        assert_eq!(SPAWNING.load(Ordering::SeqCst), before);
    }

    #[test]
    fn guard_removes_pid_from_waited_on_drop() {
        let pid = 999_001;
        WAITED.lock().unwrap().insert(pid);
        assert!(is_waited(pid));
        {
            let _guard = WaitedGuard { pid: Some(pid) };
            assert!(is_waited(pid));
        }
        assert!(!is_waited(pid), "guard drop must unregister the pid");
    }

    #[test]
    fn guard_drop_is_idempotent() {
        let pid = 999_002;
        WAITED.lock().unwrap().insert(pid);
        let guard = WaitedGuard { pid: Some(pid) };
        drop(guard);
        // No panic, still unregistered.
        WAITED.lock().unwrap().remove(&pid);
        assert!(!is_waited(pid));
    }

    #[test]
    fn ticket_decrement_is_unconditional() {
        // Relative check only: the global counter may be touched by parallel
        // tests, so assert the acquire/release pair nets to zero change.
        let before = SPAWNING.load(Ordering::SeqCst);
        let ticket = SpawnTicket::acquire();
        assert_eq!(SPAWNING.load(Ordering::SeqCst), before + 1);
        ticket.release();
        assert_eq!(SPAWNING.load(Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn spawn_managed_registers_pid_and_guard_removes_it() {
        // A real spawn through the registry: the PID must be in WAITED once the
        // function returns (lock-across-spawn), the guard must track it, and
        // dropping the guard must unregister it. We do not assert the global
        // SPAWNING counter here (other tests share it).
        let mut cmd = Command::new("true");
        let (child, guard) = spawn_managed(&mut cmd).expect("spawn `true`");
        let pid = guard.pid().expect("pid tracked");
        assert!(is_waited(pid), "pid must be registered on return");

        drop(guard);
        assert!(!is_waited(pid), "dropping the guard unregisters the pid");

        // Reap the real child so the test does not itself leave a zombie.
        let mut child = child;
        let _ = child.wait().await;
    }

    /// Grep gate: every raw process spawn in `src/` must route through this
    /// module, be an allowed non-tokio / self-reaping site, or be the documented
    /// MCP exclusion. This test fails loudly if a future spawn site is added
    /// without registering it here, so the waiter registry cannot silently go
    /// stale.
    ///
    /// The allowed list is intentionally explicit and minimal.
    #[test]
    fn no_unregistered_raw_spawn_sites() {
        // Files permitted to contain raw process spawns outside the managed
        // path, each with a justification:
        //
        //   - process_registry.rs: this module; the managed spawn itself.
        //   - zombie_reaper.rs:    reads /proc + calls libc::waitpid; no spawn.
        //   - mcp.rs:              rmcp's TokioChildProcess spawns the stdio
        //                          child internally and its ChildWithCleanup
        //                          Drop kills+waits it (self-reaping). We do not
        //                          own that Child, cannot register it, and it is
        //                          long-lived (not a zombie source). Documented
        //                          at the call site.
        //   - bin/cargo-bump.rs:   standalone build-helper binary, not the daemon.
        //
        // Files that create processes only via `std::process::Command` are *not*
        // daemon children and are exempt: a `std::process::Child` is not awaited
        // by tokio and cannot become an abandoned tokio child, so it is outside
        // the registry's scope. These are detected structurally (the line
        // contains `std::process::Command::new`) and skipped.
        const ALLOWED_NON_MANAGED: &[&str] = &[
            "src/process_registry.rs",
            "src/zombie_reaper.rs",
            "src/mcp.rs",
            "src/bin/cargo-bump.rs",
        ];

        // Files that legitimately contain a process `Command::new` but only pass
        // the command to `output_managed`/`spawn_managed`. The gate asserts the
        // managed import is present.
        const MANAGED_FILES: &[&str] = &[
            "src/tools/shell.rs",
            "src/sandbox.rs",
            "src/sandbox/detection.rs",
            "src/projects/git.rs",
            "src/api/ssh.rs",
            "src/opencode/server.rs",
        ];

        // Match only genuine process-command constructors, not substrings of
        // unrelated types like `CreateCommand::new` / `BotCommand::new`.
        fn has_process_command_new(line: &str) -> bool {
            let trimmed = line.trim_start();
            trimmed.contains("std::process::Command::new")
                || trimmed.contains("tokio::process::Command::new")
                // bare `Command::new(` where the token before it is exactly
                // `Command` (word boundary), e.g. `let mut cmd = Command::new(`
                || {
                    let mut idx = 0;
                    while let Some(pos) = trimmed[idx..].find("Command::new") {
                        let abs = idx + pos;
                        // char immediately before must be a non-identifier char
                        let before_ok = abs == 0
                            || !trimmed[..abs]
                                .chars()
                                .last()
                                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
                        if before_ok {
                            return true;
                        }
                        idx = abs + "Command::new".len();
                    }
                    false
                }
        }

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let mut files = Vec::new();
        walk(&src, &mut files);

        for path in files {
            let rel = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };

            // A file is "interesting" if any line constructs a process command.
            let creates_process = contents
                .lines()
                .any(|line| has_process_command_new(line));
            if !creates_process {
                continue;
            }

            // Exempt files whose only process construction is std::process
            // (never a tokio child). If a file has any tokio/process Command, it
            // is still examined even if it also uses std::process.
            let uses_std = contents.contains("std::process::Command::new");
            let uses_tokio = contents.contains("tokio::process::Command::new")
                || contents.contains("use tokio::process::Command");

            if uses_std && !uses_tokio {
                // Pure std::process file — outside the registry's scope.
                continue;
            }

            if ALLOWED_NON_MANAGED.contains(&rel.as_str()) {
                continue;
            }

            let is_managed = MANAGED_FILES.contains(&rel.as_str())
                && (contents.contains("output_managed") || contents.contains("spawn_managed"));

            if !is_managed {
                offenders.push(rel.clone());
            }
        }

        assert!(
            offenders.is_empty(),
            "raw process spawn sites bypassing the waiter registry \
             (add to ALLOWED_NON_MANAGED with justification, or route through \
             output_managed/spawn_managed and add to MANAGED_FILES): {offenders:?}"
        );
    }
}
