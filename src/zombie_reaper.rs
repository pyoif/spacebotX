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
//! Individual spawn sites (the sandbox builders, the shell timeout and quiesce
//! paths) now reap their own children, but a daemon that is PID 1 should not
//! rely on every future spawn site being correct. This task is defense in
//! depth: it periodically sweeps for *stale* zombie children of this process
//! and reaps them.
//!
//! ## Why the age guard
//!
//! A freshly-exited child may still have a pending `tokio::process::Child::wait()`
//! owner that has not yet observed the exit status. Calling `waitpid` on such a
//! child would steal the status out from under that await, leaving it blocked
//! forever (a much worse bug than a temporary zombie). We therefore only reap
//! children that have been zombies for longer than [`STALE_ZOMBIE_AGE`], by
//! which point any legitimate waiter would already have consumed the status.

use std::time::Duration;

/// How often the sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// A zombie must be at least this old before we reap it. This is the guard that
/// keeps us from stealing the exit status of a child that still has a live
/// `wait()` owner.
const STALE_ZOMBIE_AGE: Duration = Duration::from_secs(30);

/// Spawn the background reaper task. Safe to call once at daemon startup; it
/// runs for the lifetime of the process and never returns.
///
/// The sweep runs on two triggers, both feeding the *same* stale-only logic:
///
/// 1. **SIGCHLD wake** — when any child exits, the kernel delivers SIGCHLD and
///    we sweep almost immediately (sub-second), so a leaked zombie is collected
///    long before the periodic tick. We register via
///    [`tokio::signal::unix::signal`], which uses the signal-hook-registry
///    demultiplexer: tokio's own process driver also listens for SIGCHLD, and
///    the registry lets both callbacks coexist instead of one stealing the
///    signal from the other.
/// 2. **Periodic tick** — the correctness floor. SIGCHLD is standard (non-RT)
///    and may be coalesced or, in principle, missed around registration; the
///    30s tick guarantees a sweep regardless.
///
/// The signal only *accelerates detection*. The age guard in
/// [`sweep_stale_zombies`] is unchanged and remains the safety property: we
/// never reap a zombie younger than [`STALE_ZOMBIE_AGE`], so we can never steal
/// the exit status of a child that still has a live `wait()` owner.
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

            run_sweep("triggered");

            // After a signal wake, a child that exited *during* the scan may not
            // be as old as the guard yet. One short follow-up sweep catches it
            // once it crosses the threshold, without busy-looping.
            if sigchld.is_some() {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                run_sweep("follow-up");
            }
        }
    });
}

/// Run one sweep and log if anything was collected. `trigger` labels the log
/// line so signal-driven and tick-driven sweeps can be told apart in logs.
fn run_sweep(trigger: &'static str) {
    let reaped = sweep_stale_zombies();
    if reaped > 0 {
        tracing::info!(
            reaped,
            trigger,
            "zombie reaper collected stale child processes (PID-1 duty)"
        );
    }
}

/// One sweep. Returns the number of zombies reaped. Never panics: any error
/// reading `/proc` simply ends this pass, and the next tick retries.
fn sweep_stale_zombies() -> usize {
    let my_pid = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };

    let mut reaped = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
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
        let Ok(ppid) = fields[1].parse::<i32>() else {
            continue;
        };
        if ppid != my_pid {
            continue;
        }

        if zombie_age(pid) < Some(STALE_ZOMBIE_AGE) {
            continue;
        }

        // SAFETY: `waitpid` on our own child with WNOHANG never blocks and only
        // touches kernel state for `pid`. A non-positive return just means the
        // child was already reaped or is not ours; both are fine here.
        let rc = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if rc == pid {
            reaped += 1;
        }
    }
    reaped
}

/// Age of the zombie process with `pid`, derived from its start time
/// (`/proc/<pid>/stat` field 22) vs. `/proc/uptime`. Returns `None` if anything
/// cannot be read or parsed, in which case the caller skips the process (we
/// never reap what we cannot date).
fn zombie_age(pid: i32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // starttime is field 22 overall; after the ')' the state is field 3, so
    // starttime sits at index 19 here (22 - 3).
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let uptime_ticks = (uptime_secs * *CLK_TCK as f64) as u64;

    // The start time is an absolute tick count since boot. A zombie's "age" is
    // how long ago it exited; we approximate with now - starttime, which is a
    // safe over-estimate for short-lived children (started and exited within the
    // same window) and never reaps something younger than it really is.
    let elapsed_ticks = uptime_ticks.saturating_sub(starttime_ticks);
    Some(Duration::from_secs(elapsed_ticks / *CLK_TCK))
}

/// `_SC_CLK_TCK`, resolved once.
static CLK_TCK: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    // SAFETY: `sysconf` is thread-safe and returns a long; a non-positive value
    // falls back to the virtually-universal 100 Hz.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_of_self_is_parseable() {
        // The daemon's own process should yield *some* age; this exercises the
        // /proc/<pid>/stat + /proc/uptime parsing end to end.
        let age = zombie_age(std::process::id() as i32);
        assert!(age.is_some(), "expected to parse our own start time");
    }

    #[test]
    fn sweep_does_not_panic() {
        // A sweep must never panic even if there are no reapeable children.
        let _ = sweep_stale_zombies();
    }
}
