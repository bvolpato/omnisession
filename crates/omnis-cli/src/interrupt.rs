//! Ctrl+C handling for commands that must stop only at safe points.

use std::{
    fmt, io,
    process::{Child, Command, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use wait_timeout::ChildExt;

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
/// One guard is active at a time. On Windows, Ctrl+Break counts as Ctrl+C, and the guard overrides
/// enclosing console interrupt scopes until it drops.
pub(crate) struct InterruptGuard {
    requested: Arc<AtomicBool>,
    _handler: Option<sigint::Handler>,
}

impl InterruptGuard {
    pub(crate) fn install() -> Self {
        let (requested, handler) = sigint::begin().map_or_else(
            || (Arc::default(), None),
            |(requested, handler)| (requested, Some(handler)),
        );
        Self {
            requested,
            _handler: handler,
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

/// Starts a non-interactive provider helper outside the terminal's foreground process group.
///
/// A terminal Ctrl+C then reaches only omni, whose guard lets the helper finish publishing or
/// verifying, so the import can roll back exactly. On Windows the helper leads a new console
/// process group, which ignores Ctrl+C and misses Ctrl+Break sent to omni's group; Ctrl+Break typed
/// at the console still reaches it. Helpers must not use the terminal, and omni bounds and reaps
/// them. Interactive providers keep the terminal's group, so they still receive Ctrl+C once
/// launched. If a second Ctrl+C kills omni, a helper finishes on its own, and app-servers exit when
/// their input closes.
pub(crate) trait HelperProcess {
    fn outside_terminal_group(&mut self) -> &mut Self;
}

impl HelperProcess for Command {
    fn outside_terminal_group(&mut self) -> &mut Self {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(self, 0);
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(self, CREATE_NEW_PROCESS_GROUP);
        self
    }
}

/// Process creation flag that makes the child lead a new console process group with Ctrl+C
/// disabled.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Waits up to `timeout` for a helper, killing and reaping it when it is still running or the wait
/// failed.
pub(crate) fn wait_or_kill(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let waited = child.wait_timeout(timeout);
    if !matches!(waited, Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
    waited
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

    /// Re-arms the default action when dropped.
    pub(super) struct Handler(&'static Flags);

    impl Drop for Handler {
        fn drop(&mut self) {
            self.0.default_action.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn begin() -> Option<(Arc<AtomicBool>, Handler)> {
        let flags = FLAGS.get_or_init(register).as_ref()?;
        flags.requested.store(false, Ordering::SeqCst);
        flags.default_action.store(false, Ordering::SeqCst);
        Some((Arc::clone(&flags.requested), Handler(flags)))
    }
}

// `ctrlc` allows one console control handler per process, so the guard routes through the shared
// Windows interrupt scopes instead of registering its own.
#[cfg(windows)]
mod sigint {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use crate::shim::windows_console::{InterruptAction, InterruptScope};

    /// Routes console interrupts to the guard until dropped.
    pub(super) type Handler = InterruptScope;

    pub(super) fn begin() -> Option<(Arc<AtomicBool>, Handler)> {
        let requested = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&requested);
        let scope = InterruptScope::enter(move || {
            // The first interrupt requests a stop. The next one exits, as default handling would.
            if seen.swap(true, Ordering::SeqCst) {
                InterruptAction::Terminate
            } else {
                InterruptAction::Continue
            }
        })?;
        Some((requested, scope))
    }
}

// Other platforms keep the default Ctrl+C action, so a guard there never reports a request.
#[cfg(not(any(unix, windows)))]
mod sigint {
    use std::sync::{Arc, atomic::AtomicBool};

    pub(super) enum Handler {}

    pub(super) const fn begin() -> Option<(Arc<AtomicBool>, Handler)> {
        None
    }
}
