//! Ctrl+C handling for commands that must stop only at safe points.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Error for a command that stopped after Ctrl+C and already reported what it stopped or undid.
#[derive(Debug)]
pub(crate) struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Turns the first Ctrl+C into a stop request that the command checks at safe points.
///
/// A second Ctrl+C runs the default action, and dropping the guard restores the default action.
/// One guard is active at a time.
pub(crate) struct InterruptGuard {
    requested: Arc<AtomicBool>,
}

impl InterruptGuard {
    pub(crate) fn install() -> Self {
        Self {
            requested: sigint::begin().unwrap_or_default(),
        }
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Restores the default action, then reports whether Ctrl+C arrived while the guard was armed.
    ///
    /// Restoring first means a later Ctrl+C runs the default action instead of going unseen.
    pub(crate) fn finish(self) -> bool {
        let requested = Arc::clone(&self.requested);
        drop(self);
        requested.load(Ordering::SeqCst)
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        sigint::end();
    }
}

#[cfg(unix)]
mod sigint {
    use std::sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    };

    use signal_hook::{consts::SIGINT, flag};

    struct Flags {
        requested: Arc<AtomicBool>,
        default_action: Arc<AtomicBool>,
    }

    // Actions register once per process. Unregistering them would leave SIGINT ignored, so the
    // default action is re-armed instead.
    static FLAGS: OnceLock<Option<Flags>> = OnceLock::new();

    fn register() -> Option<Flags> {
        let flags = Flags {
            requested: Arc::new(AtomicBool::new(false)),
            default_action: Arc::new(AtomicBool::new(true)),
        };
        // Order matters: the default action checks its flag before the next action arms it.
        flag::register_conditional_default(SIGINT, Arc::clone(&flags.default_action)).ok()?;
        flag::register(SIGINT, Arc::clone(&flags.default_action)).ok()?;
        flag::register(SIGINT, Arc::clone(&flags.requested)).ok()?;
        Some(flags)
    }

    pub(super) fn begin() -> Option<Arc<AtomicBool>> {
        let flags = FLAGS.get_or_init(register).as_ref()?;
        flags.requested.store(false, Ordering::SeqCst);
        flags.default_action.store(false, Ordering::SeqCst);
        Some(Arc::clone(&flags.requested))
    }

    pub(super) fn end() {
        if let Some(Some(flags)) = FLAGS.get() {
            flags.default_action.store(true, Ordering::SeqCst);
        }
    }
}

// Windows keeps the default Ctrl+C action for now, so a guard there never reports a request. A
// console control handler needs unsafe FFI, which the workspace forbids, and signal-hook is a
// Unix-only dependency here.
#[cfg(not(unix))]
mod sigint {
    use std::sync::{Arc, atomic::AtomicBool};

    pub(super) const fn begin() -> Option<Arc<AtomicBool>> {
        None
    }

    pub(super) const fn end() {}
}
