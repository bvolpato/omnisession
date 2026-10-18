//! Process-wide Ctrl+C and Ctrl+Break policy for `omni` on Windows.
//!
//! Console control events reach every process attached to the console. With default handling,
//! `omni` exits at once while a provider child keeps running in the same console: the shell
//! prompt returns early, private handoff cleanup is skipped, and the child exit code is lost.
//!
//! `ctrlc` wraps `SetConsoleCtrlHandler` without unsafe code in this crate, but allows one
//! handler per process and never removes it. Every Windows caller therefore goes through
//! [`InterruptScope`]: the newest live scope decides what an interrupt means, and with no live
//! scope `omni` exits as default handling would.

use std::sync::{
    Mutex, MutexGuard, OnceLock, PoisonError,
    atomic::{AtomicU64, Ordering},
};

/// What `omni` does with one console interrupt.
pub(crate) enum InterruptAction {
    /// Keep running.
    Continue,
    /// Exit with `STATUS_CONTROL_C_EXIT`, as default console handling would.
    Terminate,
}

/// Routes Ctrl+C and Ctrl+Break to a handler until dropped.
#[must_use = "interrupt handling reverts when this scope drops"]
pub(crate) struct InterruptScope {
    id: u64,
}

impl InterruptScope {
    /// Keeps `omni` running while a child that shares the console handles the interrupt, so
    /// `omni` can still clean up and exit with the child's code.
    ///
    /// Returns `None` when the process handler cannot be installed; default handling remains.
    pub(crate) fn ignore() -> Option<Self> {
        Self::enter(|| InterruptAction::Continue)
    }

    /// Routes interrupts to `handler` until the scope drops. Handlers run on `ctrlc`'s thread.
    ///
    /// Returns `None` when the process handler cannot be installed; default handling remains.
    pub(crate) fn enter(
        handler: impl Fn() -> InterruptAction + Send + Sync + 'static,
    ) -> Option<Self> {
        if !INSTALLED.get_or_init(|| ctrlc::set_handler(on_interrupt).is_ok()) {
            return None;
        }
        let id = NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
        scopes().push((id, Box::new(handler)));
        Some(Self { id })
    }
}

impl Drop for InterruptScope {
    fn drop(&mut self) {
        scopes().retain(|(id, _)| *id != self.id);
    }
}

type Handler = Box<dyn Fn() -> InterruptAction + Send + Sync>;

/// `STATUS_CONTROL_C_EXIT` (`0xC000013A`), the exit code of a console process ended by Ctrl+C.
const STATUS_CONTROL_C_EXIT: i32 = -1_073_741_510;

static INSTALLED: OnceLock<bool> = OnceLock::new();
static NEXT_SCOPE_ID: AtomicU64 = AtomicU64::new(0);
static SCOPES: Mutex<Vec<(u64, Handler)>> = Mutex::new(Vec::new());

fn on_interrupt() {
    let action = scopes()
        .last()
        .map_or(InterruptAction::Terminate, |(_, handler)| handler());
    if matches!(action, InterruptAction::Terminate) {
        std::process::exit(STATUS_CONTROL_C_EXIT);
    }
}

fn scopes() -> MutexGuard<'static, Vec<(u64, Handler)>> {
    SCOPES.lock().unwrap_or_else(PoisonError::into_inner)
}
