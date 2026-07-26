//! Diagnostic-only capture of the sending process's PID for a received `SIGTERM` (#247).
//!
//! POSIX only exposes the sender's PID via `SA_SIGINFO` (`siginfo_t::si_pid`), which tokio's
//! async signal stream does not surface. This module registers an independent, observe-only
//! `SA_SIGINFO` handler alongside tokio's own `SIGTERM` handling (both coexist on
//! `signal-hook-registry`, which tokio's unix signal support is itself built on) purely to
//! record which process sent the signal, so the existing shutdown log lines can name it.

use std::sync::atomic::{AtomicI32, Ordering};

/// Sentinel for "no SIGTERM sender recorded yet". Valid PIDs start at 1, and the spec calls out
/// that 0/1 are themselves meaningful low PIDs that must not be aliased with an unset default.
const UNSET: i32 = -1;

static SENDER_PID: AtomicI32 = AtomicI32::new(UNSET);

/// Registers the `SA_SIGINFO` handler for `SIGTERM`. Must be called at most once, before the
/// tokio runtime is built, so it applies uniformly to both `run_socket_service` and
/// `run_mcp_standalone`.
pub fn register() -> Result<(), std::io::Error> {
    // SAFETY: the closure below is async-signal-safe — it performs a single atomic store and
    // nothing else (no heap allocation, no locking, no I/O). `register_sigaction` requires this
    // because it runs directly in the signal handler context, before returning control to
    // whatever the process was doing when the signal arrived (FR-002).
    unsafe {
        signal_hook_registry::register_sigaction(libc::SIGTERM, |info: &libc::siginfo_t| {
            SENDER_PID.store(info.si_pid(), Ordering::Release);
        })?;
    }
    Ok(())
}

/// Returns the most recently recorded `SIGTERM` sender PID as a display string, or `"unknown"`
/// if none has been recorded (FR-005).
pub fn sender_pid_display() -> String {
    match SENDER_PID.load(Ordering::Acquire) {
        UNSET => "unknown".to_string(),
        pid => pid.to_string(),
    }
}
