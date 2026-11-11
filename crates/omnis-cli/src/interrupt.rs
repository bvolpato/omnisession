//! Ctrl+C handling for commands that must stop only at safe points.

use std::{
    fmt, io,
    process::{Child, Command, ExitStatus},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    handler: Option<sigint::Handler>,
    #[cfg(unix)]
    termination: Option<(Arc<AtomicBool>, sigint::Handler)>,
    _acknowledgement: Option<InterruptAcknowledgement>,
    _protected_command: Option<ProtectedCommand>,
}

impl InterruptGuard {
    pub(crate) fn install() -> Self {
        Self::arm(Some(ProtectedCommand::enter()))
    }

    fn arm(protected_command: Option<ProtectedCommand>) -> Self {
        let (requested, handler) = sigint::begin().map_or_else(
            || (Arc::default(), None),
            |(requested, handler)| (requested, Some(handler)),
        );
        let acknowledgement = if handler.is_some() {
            InterruptAcknowledgement::watch(&requested)
        } else {
            None
        };
        Self {
            requested,
            handler,
            #[cfg(unix)]
            termination: None,
            _acknowledgement: acknowledgement,
            _protected_command: protected_command,
        }
    }

    fn install_for_probe() -> Self {
        #[cfg(unix)]
        {
            let mut guard = Self::arm(None);
            guard.termination = sigint::begin_termination();
            guard
        }
        #[cfg(not(unix))]
        Self::arm(None)
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Restores the default action, then reports whether Ctrl+C arrived while the guard was armed.
    ///
    /// Restoring first means a later Ctrl+C runs the default action instead of going unseen.
    pub(crate) fn finish(mut self) -> bool {
        self.restore_default()
    }

    /// Restores signal handling while keeping provider probes out of pending rollback.
    pub(crate) fn restore_default(&mut self) -> bool {
        drop(self.handler.take());
        #[cfg(unix)]
        let terminated = self.termination.take().map(|(flag, _handler)| flag);
        #[cfg(unix)]
        if terminated.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            // The probe has stopped. Restore the normal exit status for SIGTERM.
            let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTERM);
        }
        self.requested()
    }
}

// Import and index guards handle interrupts at their own safe points. Their probes must
// leave those handlers armed and let the protected command finish or roll back.
static PROTECTED_COMMANDS: AtomicUsize = AtomicUsize::new(0);

/// Keeps diagnostic handlers out of a native provider transaction and its read-back.
pub(crate) struct ProtectedCommand;

impl ProtectedCommand {
    pub(crate) fn enter() -> Self {
        PROTECTED_COMMANDS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ProtectedCommand {
    fn drop(&mut self) {
        PROTECTED_COMMANDS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ActiveProbes {
    count: usize,
    guard: InterruptGuard,
}

// Concurrent lookups share the process-wide signal handler until the last group stops.
static PROBES: Mutex<Option<ActiveProbes>> = Mutex::new(None);

struct ProbeLease(Arc<AtomicBool>);

impl omnis_adapters::ProviderProbe for ProbeLease {
    fn cancellation(&self) -> &AtomicBool {
        &self.0
    }
}

impl Drop for ProbeLease {
    fn drop(&mut self) {
        let mut state = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
        let active = state.as_mut().expect("probe signal handler is active");
        active.count -= 1;
        if active.count == 0
            && state
                .take()
                .expect("last probe owns the handler")
                .guard
                .finish()
        {
            // The last group is gone. Stop the command before a caller can try a fallback.
            std::process::exit(130);
        }
    }
}

/// Arm cancellation only while a bounded provider process is active. Workspace lookup,
/// local file reads, and interactive launches retain their normal signal behavior.
pub(crate) fn with_provider_probes<T>(run: impl FnOnce() -> T) -> T {
    omnis_adapters::with_provider_probes(
        || {
            if PROTECTED_COMMANDS.load(Ordering::SeqCst) != 0 {
                return None;
            }
            let mut state = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
            let active = state.get_or_insert_with(|| ActiveProbes {
                count: 0,
                guard: InterruptGuard::install_for_probe(),
            });
            active.count += 1;
            Some(Box::new(ProbeLease(Arc::clone(&active.guard.requested))))
        },
        run,
    )
}

// Integration tests name a file here, and omni creates it once a guard recorded Ctrl+C, so a test
// lets a held import continue only after omni saw the interrupt.
const INTERRUPT_ACKNOWLEDGEMENT: &str = "OMNI_TEST_INTERRUPT_ACK";

/// Creates the `OMNI_TEST_INTERRUPT_ACK` file once its guard recorded Ctrl+C.
///
/// Signal handlers cannot write files, so a thread polls the guard's stop request until the guard
/// drops.
struct InterruptAcknowledgement {
    stopped: Arc<AtomicBool>,
}

impl InterruptAcknowledgement {
    fn watch(requested: &Arc<AtomicBool>) -> Option<Self> {
        let path = std::env::var_os(INTERRUPT_ACKNOWLEDGEMENT)?;
        let requested = Arc::clone(requested);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if requested.load(Ordering::SeqCst) {
                    let _ = std::fs::write(&path, "");
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Some(Self { stopped })
    }
}

impl Drop for InterruptAcknowledgement {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Starts a non-interactive provider helper outside the terminal's foreground process group.
///
/// A terminal Ctrl+C then reaches only omni, whose guard lets the helper finish publishing or
/// verifying, so the import can roll back exactly. On Windows the helper leads a new process group
/// on a hidden console of its own, so neither Ctrl+C nor Ctrl+Break typed at omni's console reaches
/// it. Helpers must not use the terminal, and omni bounds and reaps them. Interactive providers
/// keep the terminal's group, so they still receive Ctrl+C once launched. If a second Ctrl+C kills
/// omni, a helper finishes on its own, and app-servers exit when their input closes.
pub(crate) trait HelperProcess {
    fn outside_terminal_group(&mut self) -> &mut Self;
}

impl HelperProcess for Command {
    fn outside_terminal_group(&mut self) -> &mut Self {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(self, 0);
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(
            self,
            omnis_adapters::WINDOWS_HELPER_CREATION_FLAGS,
        );
        self
    }
}

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

    use signal_hook::{
        consts::{SIGINT, SIGTERM},
        flag,
    };

    struct Flags {
        requested: Arc<AtomicBool>,
        default_action: Arc<AtomicBool>,
    }

    // Actions register once per process. Unregistering them would leave SIGINT ignored, so the
    // default action is re-armed instead.
    static FLAGS: OnceLock<Option<Flags>> = OnceLock::new();

    fn register(signal: i32) -> Option<Flags> {
        let flags = Flags {
            requested: Arc::new(AtomicBool::new(false)),
            default_action: Arc::new(AtomicBool::new(true)),
        };
        // Order matters: the default action checks its flag before the next action arms it.
        flag::register_conditional_default(signal, Arc::clone(&flags.default_action)).ok()?;
        flag::register(signal, Arc::clone(&flags.default_action)).ok()?;
        flag::register(signal, Arc::clone(&flags.requested)).ok()?;
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
        let flags = FLAGS.get_or_init(|| register(SIGINT)).as_ref()?;
        Some(arm(flags))
    }

    pub(super) fn begin_termination() -> Option<(Arc<AtomicBool>, Handler)> {
        static TERMINATION: OnceLock<Option<Flags>> = OnceLock::new();
        let interrupt = FLAGS.get()?.as_ref()?;
        let flags = TERMINATION
            .get_or_init(|| {
                let flags = register(SIGTERM)?;
                flag::register(SIGTERM, Arc::clone(&interrupt.requested)).ok()?;
                // An interrupt of either kind arms the other signal's default action.
                flag::register(SIGTERM, Arc::clone(&interrupt.default_action)).ok()?;
                flag::register(SIGINT, Arc::clone(&flags.default_action)).ok()?;
                Some(flags)
            })
            .as_ref()?;
        Some(arm(flags))
    }

    fn arm(flags: &'static Flags) -> (Arc<AtomicBool>, Handler) {
        flags.requested.store(false, Ordering::SeqCst);
        flags.default_action.store(false, Ordering::SeqCst);
        (Arc::clone(&flags.requested), Handler(flags))
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

    use crate::windows_console::{InterruptAction, InterruptScope};

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
