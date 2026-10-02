//! Daemon-level zombie reaper.
//!
//! Spacebot can run as PID 1 inside a container (e.g. `spacebot start
//! --foreground` in Docker). As PID 1 it is the reaper of last resort for the
//! whole container: every orphaned process reparents to it, and only it can
//! collect their exit status. When it does not, those processes linger as
//! zombies (`<defunct>`) and permanently occupy process-table slots until the
//! container is recreated — which eventually exhausts the PID limit and makes
//! every `fork()` fail with `EDQUOT`/`EAGAIN`.
//!
//! Individual spawn sites reap their own children, and the MCP SDK reaps its
//! own stdio child, but a daemon that is PID 1 should not rely on every current
//! and future spawn site being correct. This task is defense in depth: it
//! sweeps for zombie children of this process and reaps the ones that are truly
//! abandoned.
//!
//! ## Exact reaping (no age guard)
//!
//! A zombie must not be reaped while it still has a live `tokio::process`
//! `wait()` owner: `waitpid` consumes the exit status, so the awaiting task
//! would block forever. Instead of guessing with an age threshold, this reaper
//! consults [`crate::process_registry`], which tracks the exact set of PIDs with
//! a live waiter ([`crate::process_registry::is_waited`]) and whether any spawn
//! is mid-flight ([`crate::process_registry::spawning_in_progress`]).
//!
//! A zombie is reaped precisely when:
//!
//! * it is a child of this process and in the `Z` state, **and**
//! * its PID is not in the waited set, **and**
//! * no spawn is in progress.
//!
//! The third condition closes the spawn-to-register race: a child can exit
//! between `spawn()` and the registry insert, and would look abandoned during
//! that instant. While a spawn is in flight the reaper defers (re-checking
//! shortly), so it never reaps a child that is about to be tracked.
//!
//! Every in-scope spawn site registers its child through
//! [`crate::process_registry::spawn_managed`] / `output_managed`, and the
//! registration guard guarantees the PID leaves the set on any exit path, so
//! abandonment is proven by bookkeeping rather than inferred from age.

use std::time::Duration;

use crate::process_registry::{spawning_in_progress, WAITED};

/// How often the periodic sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait before retrying a sweep that was deferred because a spawn
/// was in flight (the spawn-to-register window is microseconds; this is a
/// generous safety margin).
const DEFER_RETRY: Duration = Duration::from_millis(100);

/// Spawn the background reaper task. Safe to call once at daemon startup; it
/// runs for the lifetime of the process and never returns.
///
/// The sweep runs on two triggers, both feeding the *same* exact logic:
///
/// 1. **SIGCHLD wake** — when any child exits, the kernel delivers SIGCHLD and
///    we sweep almost immediately (sub-second), so an abandoned zombie is
///    collected without waiting for the periodic tick. We register via
///    [`tokio::signal::unix::signal`], which uses the signal-hook-registry
///    demultiplexer: tokio's own process driver also listens for SIGCHLD, and
///    the registry lets both callbacks coexist instead of one stealing the
///    signal from the other.
/// 2. **Periodic tick** — the correctness floor. SIGCHLD is standard (non-RT)
///    and may be coalesced or, in principle, missed around registration; the
///    30s tick guarantees a sweep regardless.
pub fn spawn() {
    tokio::spawn(async {
        // Signal wake. If registration fails (e.g. unusual platform), fall back
        // to the periodic tick alone rather than losing the reaper entirely.
        let mut sigchld = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::child(),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "zombie reaper: could not register SIGCHLD wake; \
                     falling back to periodic sweep only"
                );
                None
            }
        };

        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        // The first tick fires immediately; skip it so we do not race startup.
        ticker.tick().await;

        loop {
            match sigchld.as_mut() {
                Some(sig) => {
                    tokio::select! {
                        _ = sig.recv() => {}
                        _ = ticker.tick() => {}
                    }
                }
                None => {
                    ticker.tick().await;
                }
            }

            // Defer while a spawn is in flight: a child may have exited between
            // spawn() and registry insert, so it would look abandoned for an
            // instant. Re-check shortly until the window closes, then sweep
            // once, exactly.
            while spawning_in_progress() {
                tokio::time::sleep(DEFER_RETRY).await;
            }
            run_sweep("triggered");

            // After a signal wake, a child that exited *during* the scan may not
            // yet be tracked by the time we looked. One short follow-up sweep
            // catches it.
            if sigchld.is_some() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                while spawning_in_progress() {
                    tokio::time::sleep(DEFER_RETRY).await;
                }
                run_sweep("follow-up");
            }
        }
    });
}

/// Run one sweep and log if anything was collected. `trigger` labels the log
/// line so signal-driven and tick-driven sweeps can be told apart in logs.
fn run_sweep(trigger: &'static str) {
    let reaped = sweep_zombies();
    if reaped > 0 {
        tracing::info!(
            reaped,
            trigger,
            "zombie reaper collected abandoned child processes (PID-1 duty)"
        );
    }
}

/// One sweep. Returns the number of zombies reaped. Never panics: any error
/// reading `/proc` or the registry simply ends this pass, and the next trigger
/// retries.
///
/// Thin wrapper that also enforces the spawn-window guard used by every caller;
/// the per-wake body itself is [`reap_orphans_once`].
fn sweep_zombies() -> usize {
    // A child exiting inside an in-flight spawn window would look abandoned; do
    // not reap anything until the registry is consistent again.
    if spawning_in_progress() {
        return 0;
    }
    reap_orphans_once()
}

/// The per-wake reaper body.
///
/// If a spawn is currently in flight, a process might have exited in the
/// sub-millisecond window before its PID reached [`WAITED`]. Defer reaping by a
/// short sleep; the loop above re-checks [`spawning_in_progress`] and only calls
/// this once the window is closed. (If a spawn is still in flight after the
/// deferral we proceed anyway — the residual race is bounded and documented
/// here rather than papered over with a different mechanism.)
///
/// Reaps exactly the zombie children of this process whose PID is *not* in
/// [`WAITED`]. Adopted orphans (reparented to this PID-1 process, with no tokio
/// waiter and no SIGCHLD delivered for the reparent) are the intended target of
/// the periodic tick, since reparenting does not fire SIGCHLD.
///
/// `waitpid` is called per-PID with `WNOHANG`; `waitpid(-1, ...)` is never used,
/// because it could consume the exit status of a child that a tokio task is
/// about to observe.
fn reap_orphans_once() -> usize {
    // Hold the WAITED lock across the /proc scan and the per-PID waitpid (chi's
    // shape). This is what makes the lock-across-spawn hardening in
    // `spawn_managed` effective: a spawner that takes the lock first has its
    // PID inserted before we can read the set; a spawner that arrives while we
    // hold it blocks *before forking*, so no unregistered child can exist while
    // we scan. There is no `.await` in this critical section.
    let waited = match WAITED.lock() {
        Ok(set) => set,
        // Fail safe: if the registry is unreadable, reap nothing rather than
        // risk stealing a live waiter's exit status.
        Err(_) => return 0,
    };

    let mut reaped = 0usize;
    for pid in get_zombies_ppid_self() {
        if waited.contains(&pid) {
            // Still owned by a live `wait()`; do not touch it.
            continue;
        }

        // SAFETY: `waitpid` on one of our own children with `WNOHANG` never
        // blocks and only touches kernel state for `pid`. A non-positive return
        // means the child was already collected (or is not ours); both are fine.
        let rc = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if rc == pid {
            reaped += 1;
            tracing::debug!(pid, "zombie reaper reaped abandoned child");
        }
    }
    reaped
}

/// Scan `/proc` for zombie (`state == Z`) children of this process, returning
/// their PIDs.
///
/// On non-Linux targets (or if `/proc` is unavailable) this returns an empty
/// vec; the reaper then has nothing to do, which is correct — the daemon is not
/// a PID-1 reaper on those platforms.
fn get_zombies_ppid_self() -> Vec<libc::pid_t> {
    let my_pid = std::process::id() as libc::pid_t;
    let mut zombies = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return zombies;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<libc::pid_t>().ok()) else {
            continue;
        };

        // Parse /proc/<pid>/stat. Field layout (1-indexed, space separated):
        //   (1) pid (2) comm (3) state ... (4) ppid ...
        // `comm` may contain spaces and parentheses, so split after the final
        // ')' to get stable fields.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        // After ')', fields restart at state (index 0), then ppid (index 1).
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        if fields[0] != "Z" {
            continue;
        }
        let Ok(ppid) = fields[1].parse::<libc::pid_t>() else {
            continue;
        };
        if ppid != my_pid {
            continue;
        }
        zombies.push(pid);
    }
    zombies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_does_not_panic() {
        // A sweep must never panic even if there are no reapable children.
        let _ = sweep_zombies();
    }

    #[test]
    fn zombie_scan_does_not_panic() {
        // The /proc scanner must tolerate whatever the environment looks like.
        let _ = get_zombies_ppid_self();
    }

    #[test]
    fn sweep_skips_while_spawn_in_flight() {
        // While a spawn is mid-flight the sweep must reap nothing. SPAWNING is
        // global and parallel tests may also nudge it, so assert the delta.
        let before = crate::process_registry::spawning_delta_for_test();
        crate::process_registry::hold_one_for_test();
        assert!(spawning_in_progress());
        assert_eq!(sweep_zombies(), 0);
        crate::process_registry::release_one_for_test();
        assert_eq!(crate::process_registry::spawning_delta_for_test(), before);
    }
}
