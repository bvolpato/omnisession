use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use directories::BaseDirs;
use omnis_adapters::{
    AdapterRegistry, CodexAdapter, LaunchPlan, LaunchTarget, NativeSession, ProviderInstallation,
    installed_opencode_model_with_binary, read_opencode_session_with_binary_at,
};
use omnis_core::{
    build_fidelity_report, build_native_fork_report, build_native_materialization_report,
    build_official_import_report, build_semantic_handoff_report_for_snapshot, capture_workspace,
    fidelity_report_for_snapshot, redact_json_secrets, redact_secrets, render_markdown_export,
    render_semantic_handoff, safe_terminal_line, trajectory_search_document, workspace_paths_match,
    workspace_root,
};
use omnis_ir::{
    BundleManifest, CanonicalSnapshot, FidelityEntry, FidelityReport, FidelityStatus,
    PortableBundle, Provider, ReplayPolicy, SCHEMA_VERSION, Sensitivity, SessionRef, TransferMode,
};
use omnis_store::{
    BindingRecord, IndexedSession, SessionTrajectoryOrigin, Store, StoreError, TaskRecord,
    state_root,
};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use uuid::Uuid;
use wait_timeout::ChildExt;

mod antigravity_import;
mod claude_import;
mod codex_import;
#[cfg(test)]
mod conversion_matrix_tests;
mod cursor_ide_import;
mod cursor_import;
mod grok_import;
mod hermes_import;
mod native_path;
mod opencode_import;
mod pi_import;
mod private_store_lock;
mod provider_compatibility;
mod self_update;
mod session_picker;
mod shim;
mod transfer;
mod version_gate;

#[cfg(test)]
use shim::recognized_resume_prefix;
#[cfg(all(test, unix))]
use shim::shell_quote;
#[cfg(all(test, any(unix, windows)))]
use shim::{create_shim_link, validate_owned_shim};
use shim::{
    cursor_ide_binary, invoked_shim_provider, resolved_provider_binary, runnable_target_providers,
    shim_exec,
};
#[cfg(test)]
use transfer::{
    ResolvedResumeRequest, can_resume_without_snapshot, requires_materialized_fork, resume_project,
    selected_native_workspace,
};
use transfer::{
    error_after_rollback, fork, materialize_antigravity_import, materialize_claude_import,
    materialize_codex_import, materialize_cursor_import, materialize_grok_import,
    materialize_hermes_import, materialize_opencode_import, materialize_pi_import, provider_name,
    resume, rollback_opencode_import,
};

const PROVIDERS: [Provider; 9] = provider_compatibility::PROVIDER_PRIORITY;
const MAX_BUNDLE_SIZE: u64 = 64 * 1024 * 1024;
const MAX_MARKDOWN_SIZE: u64 = 64 * 1024 * 1024;
const SHIM_BRANCH: &str = "main";
const SHIM_PROVIDERS: [Provider; 8] = [
    Provider::Codex,
    Provider::Claude,
    Provider::OpenCode,
    Provider::Pi,
    Provider::Grok,
    Provider::CursorCli,
    Provider::Antigravity,
    Provider::Hermes,
];
#[cfg(target_os = "linux")]
const DELETE_PROVIDERS: [Provider; 8] = [
    Provider::Codex,
    Provider::OpenCode,
    Provider::Grok,
    Provider::Hermes,
    Provider::Antigravity,
    Provider::Pi,
    Provider::CursorCli,
    Provider::CursorIde,
];
#[cfg(not(target_os = "linux"))]
const DELETE_PROVIDERS: [Provider; 4] = [
    Provider::Codex,
    Provider::OpenCode,
    Provider::Grok,
    Provider::Hermes,
];

trait IndexedSessionReader {
    fn read_session_indexed(&self, session: &SessionRef) -> Result<CanonicalSnapshot>;
}

impl IndexedSessionReader for AdapterRegistry {
    fn read_session_indexed(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        let snapshot = read_session(self, session)?;
        match Store::open_default() {
            Ok(store) => {
                let current = match store
                    .session_trajectory_source_is_current(session, snapshot.captured_at)
                {
                    Ok(current) => current,
                    Err(error) => {
                        eprintln!("warning: local trajectory index status: {error}");
                        false
                    }
                };
                if !current {
                    let document = trajectory_search_document(&snapshot);
                    if let Err(error) = store_search_document(
                        &store,
                        session,
                        &snapshot,
                        &document,
                        document.source_complete,
                        if session.provider == Provider::Imported {
                            SessionTrajectoryOrigin::ImportedBundle
                        } else {
                            SessionTrajectoryOrigin::Native
                        },
                    ) {
                        eprintln!("warning: local trajectory index write: {error}");
                    }
                }
            }
            Err(error) => eprintln!("warning: local trajectory index unavailable: {error}"),
        }
        Ok(snapshot)
    }
}

fn store_search_document(
    store: &Store,
    session: &SessionRef,
    snapshot: &CanonicalSnapshot,
    document: &omnis_core::SearchDocument,
    source_complete: bool,
    origin: SessionTrajectoryOrigin,
) -> omnis_store::Result<()> {
    store.upsert_session_trajectory_document(
        session,
        &document.text,
        snapshot.captured_at,
        document.source_byte_count,
        document.indexed_byte_count,
        document.truncation_strategy.as_str(),
        source_complete && document.source_complete,
        origin,
    )
}

fn imported_bundle_id(session: &SessionRef) -> Result<Uuid> {
    let bundle_id = Uuid::parse_str(&session.id)
        .with_context(|| format!("imported source `{session}` has an invalid bundle UUID"))?;
    if session.id != bundle_id.to_string() {
        bail!("imported source must use canonical `imported:{bundle_id}` syntax");
    }
    Ok(bundle_id)
}

fn read_session(registry: &AdapterRegistry, session: &SessionRef) -> Result<CanonicalSnapshot> {
    if session.provider != Provider::Imported {
        return registry.read_session(session);
    }
    let bundle_id = imported_bundle_id(session)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    load_imported_bundle(&store, bundle_id).map(|bundle| bundle.snapshot)
}

fn continuation_target_provider(session: &SessionRef) -> Result<Provider> {
    if session.provider != Provider::Imported {
        return Ok(session.provider);
    }
    let bundle_id = imported_bundle_id(session)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    load_imported_bundle(&store, bundle_id).map(|bundle| bundle.snapshot.session.provider)
}

fn load_imported_bundle(store: &Store, bundle_id: Uuid) -> Result<PortableBundle> {
    let bundle = store
        .load_bundle(bundle_id)
        .context("loading imported bundle")?
        .with_context(|| format!("imported bundle `{bundle_id}` was not found"))?;
    validate_bundle(&bundle).context("validating stored imported bundle")?;
    if bundle.manifest.bundle_id != bundle_id {
        bail!("stored imported bundle `{bundle_id}` has mismatched identity");
    }
    Ok(bundle)
}

#[derive(Debug, Parser)]
#[command(
    name = "omni",
    version,
    about = "Continue coding sessions across agents",
    after_help = "Run `omni` to choose a session and target. Use `omni resume ...` to continue or `omni fork ...` to branch."
)]
struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Choose a session and continue it in any available agent.
    Resume(ResumeArgs),
    /// Fork a session into any available agent.
    Fork(ForkArgs),
    /// Export visible conversation history as Markdown.
    Markdown(MarkdownArgs),
    /// Check provider installations, stores, and `OmniSession` state.
    Doctor,
    /// Diagnostic: list native sessions.
    List(ListArgs),
    /// Diagnostic: render safe context from one native session.
    Show(SessionArgs),
    /// Diagnostic: report transfer fidelity for one source and target.
    Inspect(InspectArgs),
    /// Advanced: move selected routing binding to another provider.
    Switch(SwitchArgs),
    /// Advanced: manage persistent routing bindings.
    Task(TaskArgs),
    /// Advanced: select routing binding for current workspace.
    Checkout(CheckoutArgs),
    /// Advanced: write a redacted portable bundle.
    Export(ExportArgs),
    /// Advanced: validate and store a portable bundle locally.
    Import(ImportArgs),
    /// Diagnostic: verify one session can be read.
    Verify(SessionArgs),
    /// List built-in adapter capabilities.
    Adapters,
    /// Install, remove, or execute opt-in provider shims.
    Shim(ShimArgs),
}

#[derive(Debug, Args)]
struct ShimArgs {
    #[command(subcommand)]
    command: ShimCommand,
}

#[derive(Debug, Subcommand)]
enum ShimCommand {
    /// Install provider shims into `OmniSession` state directory.
    Install(ShimInstallArgs),
    /// Remove provider shims owned by this `OmniSession` installation.
    Uninstall(ShimInstallArgs),
    /// Execute one provider through routing guard.
    Exec(ShimExecArgs),
}

#[derive(Debug, Args)]
struct ShimInstallArgs {
    #[arg(
        long,
        value_name = "DIR",
        help = "Directory containing installed `omni`"
    )]
    bin_dir: PathBuf,
}

#[derive(Debug, Args)]
struct ShimExecArgs {
    provider: Provider,
    #[arg(last = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    provider: Option<Provider>,
    #[arg(long, value_name = "PATH", default_value = ".")]
    project: PathBuf,
    #[arg(long, help = "List sessions across all known workspaces")]
    all_projects: bool,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Debug, Args)]
struct SessionArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
}

#[derive(Debug, Args)]
struct MarkdownArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: String,
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
    #[arg(long, value_name = "PROVIDER")]
    target: Option<Provider>,
}

#[derive(Debug, Args, Default)]
#[allow(clippy::struct_excessive_bools)]
struct ResumeArgs {
    #[arg(
        value_name = "SOURCE",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: Option<String>,
    #[arg(
        long = "in",
        value_name = "PROVIDER",
        help = "Target agent; omit to choose interactively"
    )]
    target: Option<Provider>,
    #[arg(
        long = "from",
        value_name = "PROVIDER",
        conflicts_with = "source",
        help = "Start interactive selection filtered to one source provider"
    )]
    source_provider: Option<Provider>,
    #[arg(
        long = "all",
        visible_alias = "all-projects",
        conflicts_with = "source",
        help = "Start interactive selection across all workspaces"
    )]
    all_projects: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(
        long,
        help = "Create and verify supported native target session without launching it"
    )]
    materialize_only: bool,
    #[arg(
        long,
        conflicts_with = "no_fork",
        help = "Fork same-provider session into a new session before continuing"
    )]
    fork: bool,
    #[arg(
        long,
        conflicts_with = "fork",
        help = "Resume same-provider session in place instead of forking"
    )]
    no_fork: bool,
    #[arg(
        long,
        help = "Allow explicit transfer across different workspace roots"
    )]
    allow_workspace_mismatch: bool,
}

#[derive(Debug, Args)]
struct ForkArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: String,
    #[arg(
        long = "in",
        value_name = "PROVIDER",
        help = "Target agent; omit to choose interactively"
    )]
    target: Option<Provider>,
    #[arg(long)]
    dry_run: bool,
    #[arg(
        long,
        help = "Create and verify supported native target session without launching it"
    )]
    materialize_only: bool,
    #[arg(
        long,
        help = "Allow explicit transfer across different workspace roots"
    )]
    allow_workspace_mismatch: bool,
}

#[derive(Debug, Args)]
struct SwitchArgs {
    target: Provider,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "main")]
    branch: String,
}

#[derive(Debug, Args)]
struct TaskArgs {
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Create and select a task for current workspace.
    Start {
        name: String,
        #[arg(long)]
        from: Option<SessionRef>,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Show selected task and branch head.
    Status {
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Bind selected task branch to a verified native session.
    Bind {
        session: SessionRef,
        #[arg(long, default_value = "main")]
        branch: String,
    },
}

#[derive(Debug, Args)]
struct CheckoutArgs {
    name: String,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct ImportArgs {
    input: PathBuf,
}

fn main() -> ExitCode {
    let result = invoked_shim_provider().map_or_else(
        || run(Cli::parse()),
        |provider| {
            let args = env::args_os().skip(1).collect::<Vec<_>>();
            shim_exec(provider, &args)
        },
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Err(error) = migrate_imported_bundle_sources() {
        eprintln!("warning: local imported source migration: {error}");
    }
    let registry = AdapterRegistry::with_local_adapters();
    match command_or_resume(cli.command) {
        Commands::Doctor => doctor(&registry, cli.json),
        Commands::List(args) => list(&registry, &args, cli.json),
        Commands::Show(args) => {
            let session = resolve_session_ref(&registry, &args.session)?;
            show(&registry, &session, cli.json)
        }
        Commands::Markdown(args) => markdown(&registry, &args, cli.json),
        Commands::Inspect(args) => inspect(&registry, &args, cli.json),
        Commands::Resume(args) => resume(&registry, &args, cli.json, None),
        Commands::Fork(args) => fork(&registry, &args, cli.json),
        Commands::Switch(args) => switch(&registry, &args, cli.json),
        Commands::Task(args) => task(&registry, args, cli.json),
        Commands::Checkout(args) => checkout(&args, cli.json),
        Commands::Export(args) => export(&registry, &args, cli.json),
        Commands::Import(args) => import(&args, cli.json),
        Commands::Verify(args) => {
            let session = resolve_session_ref(&registry, &args.session)?;
            verify(&registry, &session, cli.json)
        }
        Commands::Adapters => adapters(&registry, cli.json),
        Commands::Shim(args) => shim::run(args),
    }
}

fn command_or_resume(command: Option<Commands>) -> Commands {
    command.unwrap_or_else(|| Commands::Resume(ResumeArgs::default()))
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ProviderStatus {
    installation: ProviderInstallation,
    launcher: Option<PathBuf>,
    source_detected: bool,
    read_index: bool,
    clean_start: bool,
    same_provider_resume: bool,
    cross_provider_import_supported: bool,
    cross_provider_import: bool,
    native_writer_ready: bool,
}

impl ProviderStatus {
    fn installed(&self) -> bool {
        self.source_detected || self.launcher.is_some()
    }

    fn cross_provider_route(&self, provider: Provider) -> &'static str {
        if self.cross_provider_import {
            if matches!(provider, Provider::OpenCode | Provider::Hermes) {
                "official_import"
            } else {
                "native_materialization"
            }
        } else if self.clean_start {
            "semantic_handoff"
        } else {
            "unavailable"
        }
    }

    fn native_writer_readiness(&self) -> &'static str {
        if self.native_writer_ready {
            "ready"
        } else if !self.cross_provider_import_supported {
            "unsupported_platform"
        } else if self.launcher.is_none() {
            "launcher_not_detected"
        } else if self.cross_provider_import {
            "runtime_validation_required"
        } else {
            "unavailable"
        }
    }
}

fn session_discovery_status(installed: bool, read_index_supported: bool) -> &'static str {
    if !installed {
        "not_installed"
    } else if !read_index_supported {
        "unsupported_platform"
    } else {
        "no_source"
    }
}

fn provider_status(registry: &AdapterRegistry, provider: Provider) -> Result<ProviderStatus> {
    use provider_compatibility::{Capability, supports_capability};

    let installation = registry.adapter(provider)?.probe();
    let launcher = if provider == Provider::CursorIde {
        cursor_ide_binary().ok()
    } else {
        resolved_provider_binary(provider).ok()
    };
    let source_detected = match provider {
        Provider::OpenCode => false,
        Provider::CursorIde => installation.installed,
        Provider::Hermes => installation
            .data_root
            .as_ref()
            .is_some_and(|root| root.join("state.db").is_file()),
        _ => installation
            .data_root
            .as_ref()
            .is_some_and(|root| root.is_dir()),
    };
    let read_index = supports_capability(provider, Capability::ReadIndex)
        && (source_detected || (provider == Provider::OpenCode && launcher.is_some()));
    let clean_start = supports_capability(provider, Capability::CleanStart) && launcher.is_some();
    let same_provider_resume =
        supports_capability(provider, Capability::SameProviderResume) && launcher.is_some();
    let cross_provider_import_supported =
        supports_capability(provider, Capability::CrossProviderImport);
    let cross_provider_import = cross_provider_import_supported && launcher.is_some();
    Ok(ProviderStatus {
        installation,
        launcher,
        source_detected,
        read_index,
        clean_start,
        same_provider_resume,
        cross_provider_import_supported,
        cross_provider_import,
        native_writer_ready: false,
    })
}

fn doctor(registry: &AdapterRegistry, json_output: bool) -> Result<()> {
    let mut results = Vec::new();
    for provider in PROVIDERS {
        let status = provider_status(registry, provider)?;
        let read_index_supported = provider_compatibility::supports_capability(
            provider,
            provider_compatibility::Capability::ReadIndex,
        );
        let sessions = if status.read_index {
            match registry.list_sessions(provider, Some(&current_project()?)) {
                Ok(sessions) => json!({"status": "ok", "count": sessions.len()}),
                Err(error) => json!({"status": "degraded", "error": error.to_string()}),
            }
        } else {
            json!({
                "status": session_discovery_status(status.installed(), read_index_supported),
                "count": 0,
            })
        };
        results.push(json!({
            "provider": provider,
            "installed": status.installed(),
            "source_detected": status.source_detected,
            "launcher_detected": status.launcher.is_some(),
            "executable": status.launcher,
            "data_root": status.installation.data_root,
            "sessions": sessions,
        }));
    }

    let store = Store::open_default().context("opening OmniSession state")?;
    let selected = store
        .selected_task(current_project()?)
        .context("reading selected task")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "providers": results,
                "selected_task": selected.as_ref().map(|task| &task.name),
            }))?
        );
        return Ok(());
    }

    println!("OmniSession {SCHEMA_VERSION}");
    for result in results {
        let provider = result["provider"].as_str().unwrap_or("unknown");
        let status = result["sessions"]["status"].as_str().unwrap_or("unknown");
        let count = result["sessions"]["count"].as_u64().unwrap_or(0);
        println!("{provider:<12} {status:<14} {count:>5} project sessions");
    }
    match selected {
        Some(task) => println!("selected task: {}", task.name),
        None => println!("selected task: none"),
    }
    Ok(())
}

fn list(registry: &AdapterRegistry, args: &ListArgs, json_output: bool) -> Result<()> {
    let project = if args.all_projects {
        None
    } else {
        Some(
            fs::canonicalize(&args.project)
                .with_context(|| format!("resolving project `{}`", args.project.display()))?,
        )
    };
    let include_imported = args
        .provider
        .is_none_or(|provider| provider == Provider::Imported);
    let providers = args.provider.map_or_else(
        || PROVIDERS.to_vec(),
        |provider| {
            (provider != Provider::Imported)
                .then_some(provider)
                .into_iter()
                .collect()
        },
    );
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let discovered = thread::scope(|scope| {
        let handles = providers
            .into_iter()
            .map(|provider| {
                let project = project.as_deref();
                (
                    provider,
                    scope.spawn(move || registry.list_sessions(provider, project)),
                )
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(provider, handle)| {
                (
                    provider,
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("provider discovery panicked"))),
                )
            })
            .collect::<Vec<_>>()
    });
    for (provider, result) in discovered {
        match result {
            Ok(found) => sessions.extend(found),
            Err(error) => warnings.push(format!("{provider}: {error}")),
        }
    }
    if include_imported {
        match indexed_imported_sessions(project.as_deref()) {
            Ok(imported) => sessions.extend(imported),
            Err(error) => warnings.push(format!("imported: {error}")),
        }
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at));
    sessions.truncate(args.limit);

    if json_output {
        let values = sessions.iter().map(session_json).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"sessions": values, "warnings": warnings}))?
        );
        return Ok(());
    }
    for session in sessions {
        let updated = session
            .updated_at
            .map_or_else(|| "unknown".to_owned(), |time| time.to_rfc3339());
        println!(
            "{:<48}  {}",
            safe_terminal_line(&session.session.to_string()),
            updated
        );
    }
    for warning in warnings {
        eprintln!("warning: {}", safe_terminal_line(&warning));
    }
    Ok(())
}

fn indexed_imported_sessions(project: Option<&Path>) -> omnis_store::Result<Vec<NativeSession>> {
    Store::open_default()?
        .indexed_sessions_for_provider(Provider::Imported)
        .map(|sessions| {
            sessions
                .into_iter()
                .filter(|session| {
                    project.is_none_or(|project| {
                        session
                            .project_path
                            .as_deref()
                            .is_some_and(|path| workspace_paths_match(path, project))
                    })
                })
                .map(|session| NativeSession {
                    session: session.session,
                    title: session.title,
                    project_path: session.project_path,
                    git_branch: session.git_branch,
                    created_at: session.created_at,
                    updated_at: session.updated_at,
                    updated_at_approximate: session.updated_at_approximate,
                    event_count: session.event_count,
                    source_path: None,
                })
                .collect()
        })
}

fn session_json(session: &NativeSession) -> Value {
    json!({
        "session": session.session,
        "project_path": session.project_path,
        "git_branch": session.git_branch,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "event_count": session.event_count,
        "source_path": session.source_path,
    })
}

fn resolve_session_ref(registry: &AdapterRegistry, selector: &str) -> Result<SessionRef> {
    if selector.contains(':') {
        let mut session = selector.parse::<SessionRef>()?;
        if session.provider == Provider::Imported {
            session.id = Uuid::parse_str(&session.id)
                .with_context(|| {
                    format!(
                        "imported source `{}` has an invalid bundle UUID",
                        safe_terminal_line(selector)
                    )
                })?
                .to_string();
        }
        return Ok(session);
    }
    if selector.trim().is_empty() {
        bail!("session ID cannot be empty");
    }

    let discovered = thread::scope(|scope| {
        let handles = PROVIDERS.map(|provider| {
            (
                provider,
                scope.spawn(move || registry.list_sessions(provider, None)),
            )
        });
        handles.map(|(provider, handle)| {
            (
                provider,
                handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("provider discovery panicked"))),
            )
        })
    });
    let mut matches = Vec::new();
    let mut failures = Vec::new();
    for (provider, result) in discovered {
        match result {
            Ok(sessions) => {
                for session in sessions {
                    if session.session.id == selector && !matches.contains(&session.session) {
                        matches.push(session.session);
                    }
                }
            }
            Err(error) => failures.push(format!("{provider}: {error}")),
        }
    }
    let selected = select_discovered_session(selector, matches, &failures)?;
    if !failures.is_empty() {
        let providers = failures
            .iter()
            .filter_map(|failure| failure.split_once(':').map(|(provider, _)| provider))
            .collect::<Vec<_>>()
            .join(", ");
        progress_line(&format!(
            "warning: resolved `{}` as `{}`, but could not check duplicate IDs in {providers}. Use `provider:id` when ambiguity matters.",
            safe_terminal_line(selector),
            safe_terminal_line(&selected.to_string())
        ))?;
    }
    Ok(selected)
}

fn select_discovered_session(
    selector: &str,
    matches: Vec<SessionRef>,
    failures: &[String],
) -> Result<SessionRef> {
    if matches.is_empty() && !failures.is_empty() {
        bail!(
            "cannot resolve bare session ID while provider discovery failed ({}); use provider:id",
            failures
                .iter()
                .map(|failure| safe_terminal_line(failure))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    select_exact_session(selector, matches)
}

fn select_exact_session(selector: &str, mut matches: Vec<SessionRef>) -> Result<SessionRef> {
    matches.sort_by_key(ToString::to_string);
    match matches.as_slice() {
        [session] => Ok(session.clone()),
        [] => bail!(
            "no provider session found with exact ID `{}`",
            safe_terminal_line(selector)
        ),
        _ => bail!(
            "session ID `{}` is ambiguous ({}); use provider:id",
            safe_terminal_line(selector),
            matches
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn show(registry: &AdapterRegistry, session: &SessionRef, json_output: bool) -> Result<()> {
    let snapshot = registry
        .read_session_indexed(session)
        .with_context(|| format!("reading `{session}`"))?;
    if json_output {
        let safe = sanitize_snapshot(snapshot);
        println!("{}", serde_json::to_string_pretty(&safe)?);
    } else {
        println!("Source: {session}");
        if let Some(title) = &snapshot.title {
            println!("Title: {}", safe_terminal_line(&redact_secrets(title)));
        }
        println!("Events: {}", snapshot.events.len());
        println!();
        println!("{}", render_semantic_handoff(&snapshot));
    }
    Ok(())
}

fn markdown(registry: &AdapterRegistry, args: &MarkdownArgs, json_output: bool) -> Result<()> {
    if json_output && args.output.is_none() {
        bail!("`--json` requires `--output` for Markdown export");
    }
    let source = resolve_session_ref(registry, &args.source)?;
    let snapshot = registry
        .read_session_indexed(&source)
        .with_context(|| format!("reading `{source}`"))?;
    let snapshot = sanitize_snapshot(snapshot);
    let document = render_markdown_export(&snapshot);

    let Some(output) = &args.output else {
        io::stdout()
            .lock()
            .write_all(document.as_bytes())
            .context("writing Markdown to stdout")?;
        return Ok(());
    };

    write_markdown(output, &document)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "source": source,
                "output": output,
                "events": snapshot.events.len(),
            }))?
        );
    } else {
        println!("Exported `{source}` to `{}`.", output.display());
    }
    Ok(())
}

fn inspect(registry: &AdapterRegistry, args: &InspectArgs, json_output: bool) -> Result<()> {
    let source = resolve_session_ref(registry, &args.session)?;
    let snapshot = registry
        .read_session_indexed(&source)
        .with_context(|| format!("reading `{source}`"))?;
    let target = args.target.unwrap_or(snapshot.session.provider);
    let matches = repository_matches(&snapshot, &capture_workspace(current_project()?)?);
    let report = inspect_report(&snapshot, target, matches)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_fidelity(&report)?;
    }
    Ok(())
}

fn inspect_report(
    snapshot: &CanonicalSnapshot,
    target: Provider,
    repository_matches: bool,
) -> Result<FidelityReport> {
    if snapshot.session.provider == target {
        return Ok(fidelity_report_for_snapshot(
            snapshot,
            target,
            repository_matches,
        ));
    }
    if !provider_compatibility::supports_capability(
        target,
        provider_compatibility::Capability::CrossProviderImport,
    ) {
        return Ok(build_semantic_handoff_report_for_snapshot(
            snapshot,
            target,
            repository_matches,
        ));
    }
    let project = current_project()?;
    let stats = match target {
        Provider::Claude => resolved_provider_binary(target)
            .and_then(|binary| claude_import::ensure_supported(&binary))
            .and_then(|_| claude_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::Codex => resolved_provider_binary(target)
            .and_then(|binary| codex_import::ensure_supported(&binary))
            .and_then(|_| codex_import::build(snapshot))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::OpenCode => resolved_provider_binary(target)
            .and_then(|binary| installed_opencode_model_with_binary(&binary, &project))
            .and_then(|model| opencode_import::build(snapshot, &project, &model))
            .map(|import| (import.truncated, import.tool_events, true)),
        Provider::Grok => resolved_provider_binary(target)
            .and_then(|binary| grok_import::ensure_supported(&binary))
            .and_then(|_| grok_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::Hermes => resolved_provider_binary(target)
            .and_then(|binary| hermes_import::ensure_supported(&binary))
            .and_then(|_| hermes_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::Antigravity => resolved_provider_binary(target)
            .and_then(|binary| antigravity_import::ensure_supported(&binary))
            .and_then(|_| antigravity_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::Pi => resolved_provider_binary(target)
            .and_then(|binary| pi_import::ensure_supported(&binary))
            .and_then(|_| pi_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::CursorCli => resolved_provider_binary(target)
            .and_then(|binary| cursor_import::ensure_supported(&binary))
            .and_then(|_| cursor_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::CursorIde => cursor_ide_binary()
            .and_then(|binary| cursor_ide_import::ensure_supported(&binary))
            .and_then(|_| cursor_ide_import::build(snapshot, &project))
            .map(|import| (import.truncated, import.tool_events, false)),
        Provider::GenericAcp | Provider::Imported => {
            return Ok(build_semantic_handoff_report_for_snapshot(
                snapshot,
                target,
                repository_matches,
            ));
        }
    };
    Ok(stats.map_or_else(
        |_| build_semantic_handoff_report_for_snapshot(snapshot, target, repository_matches),
        |(truncated, tool_events, official)| {
            if official {
                build_official_import_report(
                    snapshot.session.provider,
                    repository_matches,
                    truncated,
                    tool_events,
                )
            } else {
                build_native_materialization_report(
                    snapshot.session.provider,
                    target,
                    repository_matches,
                    truncated,
                    tool_events,
                )
            }
        },
    ))
}

fn switch(registry: &AdapterRegistry, args: &SwitchArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    let task = store
        .selected_task(&project)
        .context("reading selected task")?
        .ok_or_else(|| {
            anyhow!("no selected task; run `omni task start NAME --from PROVIDER:ID`")
        })?;
    let binding = store
        .current_binding(task.id, &args.branch)
        .context("reading branch head")?
        .ok_or_else(|| {
            anyhow!(
                "task `{}` branch `{}` has no session binding",
                task.name,
                args.branch
            )
        })?;
    let resume_args = ResumeArgs {
        source: Some(binding.session.to_string()),
        target: Some(args.target),
        source_provider: None,
        all_projects: false,
        dry_run: args.dry_run,
        materialize_only: false,
        fork: false,
        no_fork: false,
        allow_workspace_mismatch: false,
    };
    let task_binding = (task.id, args.branch.clone());
    resume(registry, &resume_args, json_output, Some(&task_binding))
}

fn task(registry: &AdapterRegistry, args: TaskArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    match args.command {
        TaskCommand::Start { name, from, branch } => {
            validate_branch(&branch)?;
            if let Some(session) = &from {
                validate_session_workspace(registry, session, &project)?;
            }
            let task = store
                .start_task(&name, &project, &branch, from.as_ref())
                .context("starting task")?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "task": task.name,
                        "workspace": task.workspace_root,
                        "branch": branch,
                        "session": from,
                    }))?
                );
            } else {
                println!("Selected task `{}` for `{}`.", task.name, project.display());
                if let Some(session) = from {
                    println!("Bound `{branch}` to `{session}`.");
                }
            }
        }
        TaskCommand::Status { branch } => {
            let selected = store
                .selected_task(&project)
                .context("reading selected task")?;
            let binding = selected
                .as_ref()
                .map(|task| store.current_binding(task.id, &branch))
                .transpose()
                .context("reading branch head")?
                .flatten();
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "task": selected.as_ref().map(|task| &task.name),
                        "workspace": project,
                        "branch": branch,
                        "session": binding.as_ref().map(|binding| &binding.session),
                    }))?
                );
            } else if let Some(task) = selected {
                println!("Task: {}", task.name);
                println!("Branch: {branch}");
                match binding {
                    Some(binding) => println!("Head: {}", binding.session),
                    None => println!("Head: none"),
                }
            } else {
                println!("No task selected.");
            }
        }
        TaskCommand::Bind { session, branch } => {
            bind_task_session(registry, &store, &project, &session, &branch, json_output)?;
        }
    }
    Ok(())
}

fn bind_task_session(
    registry: &AdapterRegistry,
    store: &Store,
    project: &Path,
    session: &SessionRef,
    branch: &str,
    json_output: bool,
) -> Result<()> {
    validate_branch(branch)?;
    validate_session_workspace(registry, session, project)?;
    let selected = store
        .selected_task(project)
        .context("reading selected task")?
        .ok_or_else(|| anyhow!("no selected task; run `omni task start NAME`"))?;
    let prior = store
        .current_binding(selected.id, branch)
        .context("reading prior branch head")?;
    if let Some(prior) = prior.as_ref().filter(|prior| prior.session != *session) {
        let source = registry
            .read_session_indexed(&prior.session)
            .with_context(|| format!("reading prior `{}`", prior.session))?;
        let current = capture_workspace(project)?;
        let report = fidelity_report_for_snapshot(
            &source,
            session.provider,
            repository_matches(&source, &current),
        );
        store
            .record_handoff_and_bind(
                selected.id,
                branch,
                &prior.session,
                session,
                report.mode,
                &serde_json::to_value(&report)?,
            )
            .context("recording handoff and binding session")?;
    } else {
        store
            .bind_session(selected.id, branch, session)
            .context("binding session")?;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "task": selected.name,
                "workspace": project,
                "branch": branch,
                "session": session,
            }))?
        );
    } else {
        println!(
            "Bound task `{}` branch `{branch}` to `{session}`.",
            selected.name
        );
    }
    Ok(())
}

fn validate_branch(branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("branch name cannot be empty");
    }
    Ok(())
}

fn validate_session_workspace(
    registry: &AdapterRegistry,
    session: &SessionRef,
    project: &Path,
) -> Result<()> {
    let snapshot = registry
        .read_session_indexed(session)
        .with_context(|| format!("validating `{session}`"))?;
    if workspace_paths_match(&snapshot.workspace.root, project) {
        return Ok(());
    }
    if session.provider == Provider::Imported
        && !snapshot.workspace.root.exists()
        && imported_repository_matches(&snapshot, &capture_workspace(project)?)
    {
        return Ok(());
    }
    bail!(
        "session `{session}` belongs to `{}`, not `{}`",
        snapshot.workspace.root.display(),
        project.display()
    )
}

fn imported_repository_matches(
    source: &CanonicalSnapshot,
    current: &omnis_ir::WorkspaceSnapshot,
) -> bool {
    source
        .workspace
        .git
        .remote_fingerprint
        .as_ref()
        .zip(current.git.remote_fingerprint.as_ref())
        .is_some_and(|(source, current)| source == current)
}

fn checkout(args: &CheckoutArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    let task = store
        .select_task(&project, &args.name)
        .with_context(|| format!("selecting task `{}`", args.name))?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"task": task.name, "workspace": project}))?
        );
    } else {
        println!("Selected task `{}`.", task.name);
    }
    Ok(())
}

fn export(registry: &AdapterRegistry, args: &ExportArgs, json_output: bool) -> Result<()> {
    let source = resolve_session_ref(registry, &args.session)?;
    let snapshot = registry
        .read_session_indexed(&source)
        .with_context(|| format!("reading `{source}`"))?;
    let safe_snapshot = sanitize_snapshot(snapshot);
    let bundle_source = safe_snapshot.session.clone();
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::new_v4(),
            created_at: Utc::now(),
            source: bundle_source.clone(),
            event_count: safe_snapshot.events.len(),
            redactions: vec![
                "secret events omitted".to_owned(),
                "credential patterns redacted".to_owned(),
            ],
        },
        snapshot: safe_snapshot,
        fidelity: Some(FidelityReport {
            source: bundle_source.provider,
            target: bundle_source.provider,
            mode: TransferMode::PortableExport,
            repository_matches: false,
            entries: vec![
                FidelityEntry {
                    feature: "Visible canonical events".to_owned(),
                    status: FidelityStatus::Summarized,
                    detail: Some(
                        "Provider-specific and unrecognized records are not exported".to_owned(),
                    ),
                },
                FidelityEntry {
                    feature: "Explicitly secret events".to_owned(),
                    status: FidelityStatus::Omitted,
                    detail: Some("Events classified secret are excluded".to_owned()),
                },
                FidelityEntry {
                    feature: "Credential-like values".to_owned(),
                    status: FidelityStatus::Redacted,
                    detail: Some(
                        "Common credential patterns and sensitive object keys are redacted"
                            .to_owned(),
                    ),
                },
                FidelityEntry {
                    feature: "Private provider state".to_owned(),
                    status: FidelityStatus::Unsupported,
                    detail: None,
                },
            ],
            warnings: vec![
                "Export is a redacted canonical snapshot, not a native-session backup.".to_owned(),
            ],
        }),
    };
    write_bundle(&args.output, &bundle)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    store.save_bundle(&bundle).context("storing bundle")?;
    index_bundle_source(&store, &bundle).context("indexing bundle source")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bundle_id": bundle.manifest.bundle_id,
                "output": args.output,
                "events": bundle.manifest.event_count,
            }))?
        );
    } else {
        println!(
            "Exported {} redacted events to `{}`.",
            bundle.manifest.event_count,
            args.output.display()
        );
    }
    Ok(())
}

fn import(args: &ImportArgs, json_output: bool) -> Result<()> {
    let link_metadata = fs::symlink_metadata(&args.input)
        .with_context(|| format!("reading `{}`", args.input.display()))?;
    if link_metadata.file_type().is_symlink() {
        bail!("refusing symlink bundle `{}`", args.input.display());
    }
    let file =
        File::open(&args.input).with_context(|| format!("opening `{}`", args.input.display()))?;
    let metadata = file.metadata().context("reading opened bundle metadata")?;
    if !metadata.is_file() {
        bail!("bundle input must be a regular file");
    }
    if metadata.len() > MAX_BUNDLE_SIZE {
        bail!("bundle exceeds {MAX_BUNDLE_SIZE} byte limit");
    }
    let reader = BufReader::new(file.take(MAX_BUNDLE_SIZE + 1));
    let bundle: PortableBundle = serde_json::from_reader(reader).context("parsing bundle")?;
    validate_bundle(&bundle)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    match store.save_new_bundle(&bundle) {
        Ok(()) => {}
        Err(StoreError::BundleAlreadyExists) => {
            let existing = store
                .load_bundle(bundle.manifest.bundle_id)
                .context("loading existing bundle")?;
            if existing.as_ref() != Some(&bundle) {
                bail!(
                    "bundle UUID {} already exists with different content",
                    bundle.manifest.bundle_id
                );
            }
        }
        Err(error) => return Err(error).context("saving bundle"),
    }
    index_bundle_source(&store, &bundle).context("indexing imported bundle source")?;
    let imported = imported_session_ref(bundle.manifest.bundle_id);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bundle_id": bundle.manifest.bundle_id,
                "session": imported,
                "source": bundle.manifest.source,
                "events": bundle.manifest.event_count,
            }))?
        );
    } else {
        println!(
            "Imported `{imported}` from `{}`.",
            safe_terminal_line(&bundle.manifest.source.to_string())
        );
    }
    Ok(())
}

fn imported_session_ref(bundle_id: Uuid) -> SessionRef {
    SessionRef::new(Provider::Imported, bundle_id.to_string())
}

fn index_bundle_source(store: &Store, bundle: &PortableBundle) -> Result<()> {
    let imported = imported_session_ref(bundle.manifest.bundle_id);
    store
        .upsert_indexed_session(&IndexedSession {
            session: imported.clone(),
            title: bundle.snapshot.title.as_deref().map(redact_secrets),
            project_path: Some(redact_path(&bundle.snapshot.workspace.root)),
            git_branch: bundle
                .snapshot
                .workspace
                .git
                .branch
                .as_deref()
                .map(redact_secrets),
            created_at: Some(bundle.manifest.created_at),
            updated_at: Some(bundle.snapshot.captured_at),
            updated_at_approximate: false,
            event_count: bundle.manifest.event_count,
        })
        .context("indexing imported bundle metadata")?;
    let document = trajectory_search_document(&bundle.snapshot);
    store_search_document(
        store,
        &imported,
        &bundle.snapshot,
        &document,
        document.source_complete,
        SessionTrajectoryOrigin::ImportedBundle,
    )
    .context("indexing imported bundle trajectory")
}

fn migrate_imported_bundle_sources() -> Result<()> {
    let store = Store::open_default().context("opening OmniSession state")?;
    for bundle_id in store
        .bundle_ids_missing_imported_source()
        .context("finding imported sources needing migration")?
    {
        let Ok(bundle) = load_imported_bundle(&store, bundle_id) else {
            eprintln!("warning: ignored malformed stored bundle `{bundle_id}`");
            continue;
        };
        index_bundle_source(&store, &bundle)
            .with_context(|| format!("migrating imported bundle `{bundle_id}`"))?;
    }
    Ok(())
}

fn validate_bundle(bundle: &PortableBundle) -> Result<()> {
    if bundle.manifest.schema_version != SCHEMA_VERSION
        || bundle.snapshot.schema_version != SCHEMA_VERSION
        || bundle.snapshot.workspace.schema_version != SCHEMA_VERSION
        || bundle
            .snapshot
            .events
            .iter()
            .any(|event| event.schema_version != SCHEMA_VERSION)
    {
        bail!("bundle contains unsupported schema version; expected `{SCHEMA_VERSION}`");
    }
    if bundle.manifest.source != bundle.snapshot.session {
        bail!("bundle source does not match snapshot session");
    }
    if bundle.manifest.source.id.trim().is_empty() {
        bail!("bundle source session ID is empty");
    }
    if bundle.manifest.event_count != bundle.snapshot.events.len() {
        bail!("bundle event count does not match snapshot");
    }
    if bundle
        .fidelity
        .as_ref()
        .is_some_and(|report| report.source != bundle.manifest.source.provider)
    {
        bail!("bundle fidelity source does not match manifest source");
    }
    let mut ids = HashSet::new();
    let mut previous_sequence = None;
    for (index, event) in bundle.snapshot.events.iter().enumerate() {
        if previous_sequence.is_some_and(|previous| event.sequence <= previous) {
            bail!("bundle event sequence is not strictly increasing at index {index}");
        }
        previous_sequence = Some(event.sequence);
        if !ids.insert(event.event_id) {
            bail!("bundle contains duplicate event ID {}", event.event_id);
        }
        if event.thread_id != bundle.snapshot.thread_id
            || event.branch_id != bundle.snapshot.branch_id
            || event.source.native_session_id.trim().is_empty()
        {
            bail!("bundle event {index} does not match snapshot identity");
        }
    }
    Ok(())
}

fn verify(registry: &AdapterRegistry, session: &SessionRef, json_output: bool) -> Result<()> {
    let snapshot = registry
        .read_session_indexed(session)
        .with_context(|| format!("reading `{session}`"))?;
    let mut kinds = BTreeMap::new();
    for event in &snapshot.events {
        *kinds.entry(format!("{:?}", event.kind)).or_insert(0usize) += 1;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": session,
                "readable": true,
                "events": snapshot.events.len(),
                "kinds": kinds,
            }))?
        );
    } else {
        println!("{session}: readable, {} events", snapshot.events.len());
        for (kind, count) in kinds {
            println!("  {kind:<24} {count}");
        }
    }
    Ok(())
}

fn adapters(registry: &AdapterRegistry, json_output: bool) -> Result<()> {
    let values = PROVIDERS
        .iter()
        .map(|provider| {
            let status = provider_status(registry, *provider)?;
            let route = status.cross_provider_route(*provider);
            let writer_readiness = status.native_writer_readiness();
            Ok(json!({
                "provider": provider,
                "installed": status.installed(),
                "source_detected": status.source_detected,
                "launcher_detected": status.launcher.is_some(),
                "executable": status.launcher,
                "data_root": status.installation.data_root,
                "read_index": status.read_index,
                "clean_start": status.clean_start,
                "same_provider_resume": status.same_provider_resume,
                "cross_provider_import_supported": status.cross_provider_import_supported,
                "cross_provider_import": status.cross_provider_import,
                "native_writer_ready": status.native_writer_ready,
                "native_writer_readiness": writer_readiness,
                // Backward-compatible aliases retained for existing consumers.
                "native_resume": status.same_provider_resume,
                "native_write": status.native_writer_ready,
                "official_import": status.cross_provider_import
                    && matches!(*provider, Provider::OpenCode | Provider::Hermes),
                "cross_provider": route,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        for value in values {
            println!(
                "{:<16} installed={:<5} source={:<5} launcher={:<5} read_index={:<5} clean_start={:<5} same_resume={:<5} cross_supported={:<5} cross_import={:<5} route={:<22} writer={}",
                value["provider"].as_str().unwrap_or("unknown"),
                value["installed"].as_bool().unwrap_or(false),
                value["source_detected"].as_bool().unwrap_or(false),
                value["launcher_detected"].as_bool().unwrap_or(false),
                value["read_index"].as_bool().unwrap_or(false),
                value["clean_start"].as_bool().unwrap_or(false),
                value["same_provider_resume"].as_bool().unwrap_or(false),
                value["cross_provider_import_supported"]
                    .as_bool()
                    .unwrap_or(false),
                value["cross_provider_import"].as_bool().unwrap_or(false),
                value["cross_provider"].as_str().unwrap_or("unavailable"),
                value["native_writer_readiness"]
                    .as_str()
                    .unwrap_or("unavailable"),
            );
        }
    }
    Ok(())
}

fn current_project() -> Result<PathBuf> {
    workspace_root(std::env::current_dir()?).context("resolving current workspace")
}

fn repository_matches(source: &CanonicalSnapshot, current: &omnis_ir::WorkspaceSnapshot) -> bool {
    workspace_paths_match(&source.workspace.root, &current.root)
        && source.workspace.git.head == current.git.head
        && source.workspace.git.dirty_tree_digest == current.git.dirty_tree_digest
}

fn source_workspace_matches(source: &CanonicalSnapshot, project: &Path) -> bool {
    workspace_paths_match(&source.workspace.root, project)
}

fn print_fidelity(report: &omnis_ir::FidelityReport) -> Result<()> {
    println!("Transfer: {} -> {}", report.source, report.target);
    println!("Mode: {}", report.mode);
    println!("Repository match: {}", report.repository_matches);
    for entry in &report.entries {
        if let Some(detail) = &entry.detail {
            println!(
                "  {:<24} {:?} ({})",
                entry.feature,
                entry.status,
                safe_terminal_line(detail)
            );
        } else {
            println!("  {:<24} {:?}", entry.feature, entry.status);
        }
    }
    flush_stdout()?;
    for warning in &report.warnings {
        progress_line(&format!("warning: {warning}"))?;
    }
    Ok(())
}

fn progress_line(message: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "{message}").context("writing import progress")?;
    stderr.flush().context("flushing import progress")
}

fn flush_stdout() -> Result<()> {
    io::stdout().flush().context("flushing command output")
}

fn launch_json(plan: &LaunchPlan) -> Value {
    json!({"program": plan.program, "args": plan.args, "cwd": plan.cwd})
}

fn display_command(plan: &LaunchPlan) -> String {
    std::iter::once(plan.program.as_str())
        .chain(plan.args.iter().map(String::as_str))
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_launch(plan: &LaunchPlan) -> Result<()> {
    let child = spawn_launch(plan)?;
    wait_for_launch(child, plan)
}

fn spawn_launch(plan: &LaunchPlan) -> Result<Child> {
    let provider = match plan.program.as_str() {
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        "opencode" => Some(Provider::OpenCode),
        "grok" => Some(Provider::Grok),
        "hermes" => Some(Provider::Hermes),
        "agy" => Some(Provider::Antigravity),
        "pi" => Some(Provider::Pi),
        "cursor-agent" => Some(Provider::CursorCli),
        _ => None,
    };
    let program = provider.map_or_else(
        || Ok(PathBuf::from(&plan.program)),
        resolved_provider_binary,
    )?;
    let mut command = Command::new(&program);
    command.args(&plan.args);
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    command
        .spawn()
        .with_context(|| format!("launching `{}`", plan.program))
}

fn wait_for_launch(mut child: Child, plan: &LaunchPlan) -> Result<()> {
    let status = child
        .wait()
        .with_context(|| format!("waiting for `{}`", plan.program))?;
    if !status.success() {
        bail!("target exited with status {status}");
    }
    Ok(())
}

fn native_delete_plan(session: &SessionRef, workspace: Option<&Path>) -> Result<LaunchPlan> {
    let args = match session.provider {
        Provider::Codex => {
            Uuid::parse_str(&session.id).context("Codex session ID must be a UUID")?;
            vec![
                "delete".to_owned(),
                session.id.clone(),
                "--force".to_owned(),
            ]
        }
        Provider::OpenCode => vec![
            "--pure".to_owned(),
            "session".to_owned(),
            "delete".to_owned(),
            session.id.clone(),
        ],
        Provider::Grok => vec![
            "sessions".to_owned(),
            "delete".to_owned(),
            session.id.clone(),
        ],
        Provider::Hermes => vec![
            "sessions".to_owned(),
            "delete".to_owned(),
            session.id.clone(),
            "--yes".to_owned(),
        ],
        provider => bail!(
            "{} does not expose documented session deletion; session was not changed",
            provider_name(provider)
        ),
    };
    Ok(LaunchPlan {
        program: session
            .provider
            .command()
            .context("session provider has no command")?
            .to_owned(),
        args,
        cwd: workspace
            .filter(|path| path.is_dir())
            .map(Path::to_path_buf),
    })
}

fn delete_native_session(
    registry: &AdapterRegistry,
    session: &SessionRef,
    workspace: Option<&Path>,
) -> Result<()> {
    let native = unique_native_session(
        registry
            .list_sessions(session.provider, None)
            .with_context(|| format!("locating selected {} session", session.provider))?,
        session,
    )?;
    let _private_write_guard = match session.provider {
        Provider::Antigravity => {
            let binary = resolved_provider_binary(Provider::Antigravity)?;
            Some(antigravity_import::delete_session(session, &binary)?)
        }
        Provider::Pi => {
            pi_import::delete_session(
                session,
                native
                    .source_path
                    .as_deref()
                    .context("Pi session discovery omitted source path")?,
            )?;
            None
        }
        Provider::CursorCli => {
            cursor_import::delete_session(
                session,
                native
                    .source_path
                    .as_deref()
                    .context("Cursor Agent discovery omitted metadata path")?,
            )?;
            None
        }
        Provider::CursorIde => {
            let binary = cursor_ide_binary()?;
            Some(cursor_ide_import::delete_session(session, &binary)?)
        }
        Provider::Codex | Provider::OpenCode | Provider::Grok | Provider::Hermes => {
            delete_with_provider_command(session, workspace)?;
            None
        }
        provider => bail!(
            "{} does not support guarded native deletion; session was not changed",
            provider_name(provider)
        ),
    };
    let sessions = registry
        .list_sessions(session.provider, None)
        .with_context(|| format!("verifying {} session deletion", session.provider))?;
    if sessions
        .iter()
        .any(|candidate| candidate.session == *session)
        || registry.read_session(session).is_ok()
        || (session.provider == Provider::Grok
            && grok_session_directory_exists(
                registry
                    .adapter(Provider::Grok)?
                    .probe()
                    .data_root
                    .as_deref(),
                &session.id,
            )?)
    {
        bail!(
            "{} deletion did not remove exact source session",
            provider_name(session.provider)
        );
    }
    let _ = Store::open_default().and_then(|store| store.forget_session(session));
    Ok(())
}

fn unique_native_session(
    sessions: Vec<NativeSession>,
    selected: &SessionRef,
) -> Result<NativeSession> {
    let mut matches = sessions
        .into_iter()
        .filter(|candidate| candidate.session == *selected);
    let session = matches
        .next()
        .context("selected source session is no longer discoverable")?;
    if matches.next().is_some() {
        bail!(
            "selected source session ID `{}` is ambiguous; no session was deleted",
            selected.id
        );
    }
    Ok(session)
}

fn grok_session_directory_exists(root: Option<&Path>, id: &str) -> Result<bool> {
    Uuid::parse_str(id).context("Grok session ID must be a UUID")?;
    let Some(root) = root else {
        return Ok(false);
    };
    let root_metadata = fs::symlink_metadata(root).context("reading Grok session root")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("Grok session root is not a safe directory");
    }
    for entry in fs::read_dir(root).context("reading Grok session workspaces")? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let candidate = entry.path().join(id);
        let Ok(metadata) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!("Grok session path is an unsafe symlink");
        }
        if metadata.is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn delete_with_provider_command(session: &SessionRef, workspace: Option<&Path>) -> Result<()> {
    let plan = native_delete_plan(session, workspace)?;
    let binary = resolved_provider_binary(session.provider)?;
    let mut stderr = tempfile::tempfile().context("creating session deletion error buffer")?;
    let mut command = Command::new(&binary);
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr.try_clone()?));
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {} session deletion", session.provider))?;
    let Some(status) = child
        .wait_timeout(Duration::from_secs(30))
        .context("waiting for provider session deletion")?
    else {
        child
            .kill()
            .context("stopping timed-out session deletion")?;
        child.wait().context("reaping timed-out session deletion")?;
        bail!(
            "{} session deletion timed out",
            provider_name(session.provider)
        );
    };
    if session.provider == Provider::Grok {
        reconcile_grok_catalog(&binary, session, plan.cwd.as_deref());
        if !status.success() {
            return Ok(());
        }
    }
    if !status.success() {
        stderr.rewind().context("reading session deletion error")?;
        let mut output = Vec::new();
        stderr
            .take(16 * 1024)
            .read_to_end(&mut output)
            .context("reading session deletion error")?;
        let detail = safe_terminal_line(&redact_secrets(&String::from_utf8_lossy(&output)));
        let detail = (!detail.trim().is_empty()).then(|| format!(": {detail}"));
        bail!(
            "{} session deletion exited with {status}{}; source session was not deleted",
            provider_name(session.provider),
            detail.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

fn reconcile_grok_catalog(binary: &Path, session: &SessionRef, workspace: Option<&Path>) {
    let mut command = Command::new(binary);
    command
        .args(["sessions", "search", &session.id, "--limit", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(workspace) = workspace {
        command.current_dir(workspace);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let Ok(None) = child.wait_timeout(Duration::from_secs(10)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn write_private_handoff(document: &str) -> Result<NamedTempFile> {
    let _store = Store::open_default().context("validating OmniSession state root")?;
    let directory = state_root()
        .context("resolving OmniSession state")?
        .join("handoffs");
    if directory
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "refusing symlink handoff directory `{}`",
            directory.display()
        );
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating `{}`", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .context("securing handoff directory")?;
    }
    let mut file = tempfile::Builder::new()
        .prefix("handoff-")
        .suffix(".md")
        .tempfile_in(&directory)
        .context("creating private handoff")?;
    file.as_file_mut()
        .write_all(document.as_bytes())
        .context("writing private handoff")?;
    file.as_file()
        .sync_all()
        .context("syncing private handoff")?;
    Ok(file)
}

fn write_private_json(document: &Value) -> Result<NamedTempFile> {
    let _store = Store::open_default().context("validating OmniSession state root")?;
    let directory = state_root()
        .context("resolving OmniSession state")?
        .join("imports");
    if directory
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "refusing symlink import directory `{}`",
            directory.display()
        );
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating `{}`", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .context("securing import directory")?;
    }
    let mut file = tempfile::Builder::new()
        .prefix("opencode-")
        .suffix(".json")
        .tempfile_in(&directory)
        .context("creating private import")?;
    serde_json::to_writer(file.as_file_mut(), document).context("writing private import")?;
    file.as_file()
        .sync_all()
        .context("syncing private import")?;
    if file.as_file().metadata()?.len() > MAX_BUNDLE_SIZE {
        bail!("OpenCode import exceeds {MAX_BUNDLE_SIZE} byte safety limit");
    }
    Ok(file)
}

fn sanitize_snapshot(mut snapshot: CanonicalSnapshot) -> CanonicalSnapshot {
    snapshot.events.retain(|event| {
        event.sensitivity != Sensitivity::Secret && event.replay_policy != ReplayPolicy::Secret
    });
    if let Some(title) = &mut snapshot.title {
        *title = redact_secrets(title);
    }
    for event in &mut snapshot.events {
        if redact_json_secrets(&mut event.payload) && event.sensitivity == Sensitivity::Normal {
            event.sensitivity = Sensitivity::PotentialSecret;
        }
        event.raw_blob_hash = None;
    }
    snapshot.workspace.root = redact_path(&snapshot.workspace.root);
    snapshot.workspace.current_dir = redact_path(&snapshot.workspace.current_dir);
    snapshot.workspace.git.worktree = snapshot.workspace.git.worktree.as_deref().map(redact_path);
    if let Some(branch) = &mut snapshot.workspace.git.branch {
        *branch = redact_secrets(branch);
    }
    snapshot.workspace.instruction_files = snapshot
        .workspace
        .instruction_files
        .iter()
        .map(|path| redact_path(path))
        .collect();
    snapshot.workspace.git.untracked_files = snapshot
        .workspace
        .git
        .untracked_files
        .iter()
        .map(|path| redact_path(path))
        .collect();
    snapshot.workspace.environment_names.clear();
    snapshot
}

fn redact_path(path: &Path) -> PathBuf {
    path.to_str().map_or_else(
        || PathBuf::from("[REDACTED: NON_UTF8_PATH]"),
        |path| PathBuf::from(redact_secrets(path)),
    )
}

fn write_bundle(path: &Path, bundle: &PortableBundle) -> Result<()> {
    let mut encoded = serde_json::to_vec_pretty(bundle).context("encoding bundle")?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_BUNDLE_SIZE {
        bail!("bundle exceeds {MAX_BUNDLE_SIZE} byte limit after redaction");
    }
    write_new_file(path, &encoded, "bundle")
}

fn write_markdown(path: &Path, document: &str) -> Result<()> {
    if document.len() as u64 > MAX_MARKDOWN_SIZE {
        bail!("Markdown export exceeds {MAX_MARKDOWN_SIZE} byte limit after redaction");
    }
    write_new_file(path, document.as_bytes(), "Markdown export")
}

fn write_new_file(path: &Path, contents: &[u8], description: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating `{}`", parent.display()))?;
    if path.exists() {
        bail!("refusing to overwrite existing `{}`", path.display());
    }
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary {description} in `{}`", parent.display()))?;
    temporary
        .as_file_mut()
        .write_all(contents)
        .with_context(|| format!("writing {description}"))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing {description}"))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persisting `{}`", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
        process::Command,
    };

    use chrono::Utc;
    use clap::Parser;
    use omnis_ir::{CanonicalSnapshot, GitState, SCHEMA_VERSION, WorkspaceSnapshot};
    use serde_json::json;
    use uuid::Uuid;

    #[cfg(unix)]
    use super::shell_quote;
    use super::{
        Cli, Commands, DELETE_PROVIDERS, NativeSession, Provider, ProviderInstallation,
        ProviderStatus, ResolvedResumeRequest, SessionRef, ShimCommand,
        can_resume_without_snapshot, command_or_resume, grok_session_directory_exists,
        native_delete_plan, recognized_resume_prefix, redact_json_secrets,
        requires_materialized_fork, resume_project, select_discovered_session,
        select_exact_session, selected_native_workspace, session_discovery_status,
        unique_native_session,
    };
    #[cfg(any(unix, windows))]
    use super::{create_shim_link, validate_owned_shim};
    use crate::provider_compatibility::{
        Capability, PROVIDER_PRIORITY, Platform, supports_capability_on,
    };
    use crate::session_picker::PickerSelection;

    #[test]
    fn provider_priority_matches_target_picker_contract() {
        assert_eq!(
            PROVIDER_PRIORITY,
            [
                Provider::Codex,
                Provider::Claude,
                Provider::OpenCode,
                Provider::Pi,
                Provider::Grok,
                Provider::CursorIde,
                Provider::CursorCli,
                Provider::Antigravity,
                Provider::Hermes,
            ]
        );
    }

    #[test]
    fn antigravity_resume_and_import_have_distinct_platform_scope() {
        assert!(supports_capability_on(
            Provider::Antigravity,
            Capability::SameProviderResume,
            Platform::Macos,
        ));
        assert!(!supports_capability_on(
            Provider::Antigravity,
            Capability::CrossProviderImport,
            Platform::Macos,
        ));
        assert!(!supports_capability_on(
            Provider::Codex,
            Capability::CrossProviderImport,
            Platform::Windows,
        ));
        assert!(!supports_capability_on(
            Provider::Codex,
            Capability::CleanStart,
            Platform::Windows,
        ));
    }

    #[test]
    fn provider_status_merges_evidence_and_separates_support_from_readiness() {
        let status = |source_detected, launcher| ProviderStatus {
            installation: ProviderInstallation {
                provider: Provider::Codex,
                installed: source_detected,
                executable: None,
                data_root: None,
            },
            launcher,
            source_detected,
            read_index: false,
            clean_start: false,
            same_provider_resume: false,
            cross_provider_import_supported: false,
            cross_provider_import: false,
            native_writer_ready: false,
        };
        assert!(status(true, None).installed());
        assert!(status(false, Some(PathBuf::from("/synthetic/codex"))).installed());
        assert!(!status(false, None).installed());

        let mut supported_without_launcher = status(false, None);
        supported_without_launcher.cross_provider_import_supported = true;
        assert!(!supported_without_launcher.cross_provider_import);
        assert_eq!(
            supported_without_launcher.native_writer_readiness(),
            "launcher_not_detected"
        );
        assert_eq!(
            supported_without_launcher.cross_provider_route(Provider::Codex),
            "unavailable"
        );

        let mut semantic = status(false, Some(PathBuf::from("/synthetic/agy")));
        semantic.clean_start = true;
        assert_eq!(
            semantic.cross_provider_route(Provider::Antigravity),
            "semantic_handoff"
        );

        let mut cursor = status(false, Some(PathBuf::from("/synthetic/cursor")));
        cursor.cross_provider_import_supported = true;
        cursor.cross_provider_import = true;
        assert_eq!(
            cursor.cross_provider_route(Provider::CursorIde),
            "native_materialization"
        );
        assert_eq!(
            cursor.native_writer_readiness(),
            "runtime_validation_required"
        );
    }

    #[test]
    fn doctor_status_distinguishes_platform_support_and_installation_evidence() {
        assert_eq!(
            session_discovery_status(true, false),
            "unsupported_platform"
        );
        assert_eq!(session_discovery_status(false, false), "not_installed");
        assert_eq!(session_discovery_status(true, true), "no_source");
    }

    #[test]
    fn bare_command_opens_resume_picker() {
        let cli = Cli::try_parse_from(["omni"]).expect("bare command");
        let Commands::Resume(args) = command_or_resume(cli.command) else {
            panic!("default resume command");
        };
        assert!(args.source.is_none());
        assert!(args.target.is_none());
        assert!(!args.all_projects);
    }

    #[test]
    fn native_delete_plans_use_documented_provider_commands() {
        let workspace_root = tempfile::tempdir().expect("workspace");
        let workspace = workspace_root.path();
        let codex = native_delete_plan(
            &SessionRef::new(Provider::Codex, "019fa3c6-0000-7000-8000-000000000000"),
            Some(workspace),
        )
        .expect("Codex delete plan");
        assert_eq!(
            codex.args,
            ["delete", "019fa3c6-0000-7000-8000-000000000000", "--force"]
        );
        assert_eq!(codex.cwd.as_deref(), Some(workspace));

        let opencode = native_delete_plan(
            &SessionRef::new(Provider::OpenCode, "ses_synthetic"),
            Some(workspace),
        )
        .expect("OpenCode delete plan");
        assert_eq!(
            opencode.args,
            ["--pure", "session", "delete", "ses_synthetic"]
        );

        let grok = native_delete_plan(
            &SessionRef::new(Provider::Grok, "synthetic"),
            Some(workspace),
        )
        .expect("Grok delete plan");
        assert_eq!(grok.args, ["sessions", "delete", "synthetic"]);

        let hermes = native_delete_plan(
            &SessionRef::new(Provider::Hermes, "synthetic"),
            Some(workspace),
        )
        .expect("Hermes delete plan");
        assert_eq!(hermes.args, ["sessions", "delete", "synthetic", "--yes"]);

        let missing = native_delete_plan(
            &SessionRef::new(Provider::OpenCode, "ses_missing_workspace"),
            Some(Path::new("/workspace/that/no/longer/exists")),
        )
        .expect("missing-workspace delete plan");
        assert!(missing.cwd.is_none());

        assert!(
            native_delete_plan(
                &SessionRef::new(Provider::Claude, "synthetic"),
                Some(workspace)
            )
            .is_err()
        );
    }

    #[test]
    fn private_store_deletion_requires_linux_writer_detection() {
        assert_eq!(
            DELETE_PROVIDERS.contains(&Provider::Pi),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            DELETE_PROVIDERS.contains(&Provider::CursorCli),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn deletion_refuses_duplicate_native_session_ids() {
        let selected = SessionRef::new(Provider::Pi, "duplicate");
        let native = |source_path: &str| NativeSession {
            session: selected.clone(),
            title: None,
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(PathBuf::from(source_path)),
        };
        let error = unique_native_session(
            vec![native("/sessions/one.jsonl"), native("/sessions/two.jsonl")],
            &selected,
        )
        .expect_err("duplicate ID must be ambiguous");

        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn grok_delete_verification_checks_exact_native_directory() {
        let root = tempfile::tempdir().expect("Grok sessions root");
        let workspace = root.path().join("workspace-key");
        let id = "019fa3c6-0000-7000-8000-000000000000";
        std::fs::create_dir_all(workspace.join(id)).expect("Grok session directory");

        assert!(
            grok_session_directory_exists(Some(root.path()), id)
                .expect("verify existing Grok session")
        );
        assert!(
            !grok_session_directory_exists(
                Some(root.path()),
                "019fa3c6-0000-7000-8000-000000000001"
            )
            .expect("verify missing Grok session")
        );
    }

    #[test]
    fn resume_contract_parses_target_provider() {
        let cli =
            Cli::try_parse_from(["omni", "resume", "claude:abc", "--in", "codex", "--dry-run"])
                .expect("valid command");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source.as_deref(), Some("claude:abc"));
        assert_eq!(args.target, Some(Provider::Codex));
        assert!(args.dry_run);
        assert!(!args.allow_workspace_mismatch);
    }

    #[test]
    fn resume_accepts_bare_id_and_optional_target() {
        let cli = Cli::try_parse_from(["omni", "resume", "abc", "--dry-run"])
            .expect("valid bare session ID");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source.as_deref(), Some("abc"));
        assert_eq!(args.target, None);
        assert!(!args.materialize_only);
    }

    #[test]
    fn resume_accepts_explicit_fork() {
        let cli =
            Cli::try_parse_from(["omni", "resume", "abc", "--fork"]).expect("valid fork request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert!(args.fork);
        assert!(!args.no_fork);

        assert!(Cli::try_parse_from(["omni", "resume", "abc", "--fork", "--no-fork"]).is_err());
    }

    #[test]
    fn fork_accepts_bare_id_and_optional_target() {
        let cli = Cli::try_parse_from(["omni", "fork", "abc", "--in", "claude-code", "--dry-run"])
            .expect("valid fork request");
        let Commands::Fork(args) = cli.command.expect("subcommand") else {
            panic!("fork command");
        };
        assert_eq!(args.source, "abc");
        assert_eq!(args.target, Some(Provider::Claude));
        assert!(args.dry_run);
        assert!(!args.allow_workspace_mismatch);

        let cli = Cli::try_parse_from(["omni", "fork", "claude-code:abc"])
            .expect("interactive fork request");
        let Commands::Fork(args) = cli.command.expect("subcommand") else {
            panic!("fork command");
        };
        assert_eq!(args.target, None);
    }

    #[test]
    fn resume_accepts_interactive_source_filters() {
        let cli = Cli::try_parse_from([
            "omni", "resume", "--in", "codex", "--from", "claude", "--all",
        ])
        .expect("valid interactive resume request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source, None);
        assert_eq!(args.target, Some(Provider::Codex));
        assert_eq!(args.source_provider, Some(Provider::Claude));
        assert!(args.all_projects);
    }

    #[test]
    fn native_resume_never_requires_full_transcript() {
        let codex = SessionRef::new(Provider::Codex, "019fa3c6-0000-7000-8000-000000000000");
        let request = ResolvedResumeRequest {
            source: codex.clone(),
            target: Provider::Codex,
            resume_in_place: true,
            picker_selection: Some(PickerSelection {
                session: codex,
                project_path: Some(PathBuf::from("/workspace/project")),
                across_projects: false,
                target: Provider::Codex,
                fork: false,
                workspace_override: None,
            }),
        };

        assert!(can_resume_without_snapshot(&request));

        let cross_provider = ResolvedResumeRequest {
            target: Provider::Claude,
            ..request
        };
        assert!(!can_resume_without_snapshot(&cross_provider));

        let cursor = ResolvedResumeRequest {
            source: SessionRef::new(Provider::CursorCli, "cursor-session"),
            target: Provider::CursorCli,
            resume_in_place: false,
            picker_selection: None,
        };
        assert!(requires_materialized_fork(&cursor));
        assert!(!can_resume_without_snapshot(&cursor));
    }

    #[test]
    fn picker_workspace_override_replaces_missing_recorded_path() {
        let chosen = tempfile::tempdir().expect("chosen workspace");
        let current = tempfile::tempdir().expect("current workspace");
        let selection = PickerSelection {
            session: SessionRef::new(Provider::Codex, "session"),
            project_path: Some(chosen.path().join("missing")),
            across_projects: false,
            target: Provider::Codex,
            fork: false,
            workspace_override: Some(chosen.path().to_path_buf()),
        };

        assert_eq!(
            selected_native_workspace(&selection, current.path()).expect("selected workspace"),
            chosen.path().canonicalize().expect("canonical workspace")
        );
    }

    #[test]
    fn picker_workspace_override_wins_over_current_workspace() {
        let chosen = tempfile::tempdir().expect("chosen workspace");
        let current = tempfile::tempdir().expect("current workspace");
        let current_path = current.path().canonicalize().expect("current path");
        let chosen_path = chosen.path().canonicalize().expect("chosen path");
        let snapshot = CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "session"),
            thread_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: current_path.clone(),
                current_dir: current_path.clone(),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: Vec::new(),
        };
        let selection = PickerSelection {
            session: snapshot.session.clone(),
            project_path: None,
            across_projects: true,
            target: Provider::Codex,
            fork: false,
            workspace_override: Some(chosen_path.clone()),
        };

        assert_eq!(
            resume_project(&snapshot, &current_path, false, Some(&selection))
                .expect("selected workspace"),
            chosen_path
        );
        assert_eq!(
            resume_project(&snapshot, &current_path, true, Some(&selection))
                .expect("selected workspace with mismatch allowed"),
            chosen.path().canonicalize().expect("chosen path")
        );
    }

    #[test]
    fn resume_project_accepts_nested_path_only_in_same_repository() {
        let temp = tempfile::tempdir().expect("temporary workspaces");
        let repo = temp.path().join("repo");
        let sibling_repo = temp.path().join("sibling-repo");
        std::fs::create_dir(&repo).expect("repository directory");
        std::fs::create_dir(&sibling_repo).expect("sibling repository directory");
        for path in [&repo, &sibling_repo] {
            let status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["init", "--quiet"])
                .status()
                .expect("run git init");
            assert!(status.success());
        }
        let nested = repo.join("crates/component");
        std::fs::create_dir_all(&nested).expect("nested repository directory");
        let repo = repo.canonicalize().expect("repository root");
        let nested = nested.canonicalize().expect("nested directory");
        let sibling_repo = sibling_repo
            .canonicalize()
            .expect("sibling repository root");
        let snapshot = CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "session"),
            thread_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: nested.clone(),
                current_dir: nested,
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: Vec::new(),
        };

        assert_eq!(
            resume_project(&snapshot, &repo, false, None).expect("same repository"),
            repo
        );
        assert!(resume_project(&snapshot, &sibling_repo, false, None).is_err());
    }

    #[test]
    fn resume_accepts_materialize_only() {
        let cli = Cli::try_parse_from([
            "omni",
            "resume",
            "claude:abc",
            "--in",
            "opencode",
            "--materialize-only",
        ])
        .expect("valid materialize-only request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert!(args.materialize_only);
    }

    #[test]
    fn markdown_accepts_bare_id_and_optional_output() {
        let cli = Cli::try_parse_from(["omni", "markdown", "abc", "-o", "session.md"])
            .expect("valid Markdown export");
        let Commands::Markdown(args) = cli.command.expect("subcommand") else {
            panic!("markdown command");
        };
        assert_eq!(args.source, "abc");
        assert_eq!(args.output.as_deref(), Some(Path::new("session.md")));
    }

    #[test]
    fn session_commands_accept_bare_ids() {
        for arguments in [
            vec!["omni", "show", "abc"],
            vec!["omni", "verify", "abc"],
            vec!["omni", "inspect", "abc", "--target", "codex"],
            vec!["omni", "export", "abc", "-o", "session.omnisession"],
        ] {
            Cli::try_parse_from(arguments).expect("bare session ID");
        }
    }

    #[test]
    fn exact_session_selection_requires_one_match() {
        let codex = SessionRef::new(Provider::Codex, "abc");
        assert_eq!(
            select_exact_session("abc", vec![codex.clone()]).expect("unique match"),
            codex
        );
        assert!(select_exact_session("abc", Vec::new()).is_err());
        assert!(
            select_exact_session(
                "abc",
                vec![
                    SessionRef::new(Provider::Claude, "abc"),
                    SessionRef::new(Provider::Codex, "abc")
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn bare_session_selection_uses_known_match_when_another_provider_fails() {
        let claude = SessionRef::new(Provider::Claude, "abc");
        let failures =
            vec!["cursor-ide: provider SQLite file exceeds safe snapshot limit".to_owned()];

        assert_eq!(
            select_discovered_session("abc", vec![claude.clone()], &failures)
                .expect("known provider match"),
            claude
        );
        assert!(select_discovered_session("missing", Vec::new(), &failures).is_err());
    }

    #[test]
    fn provider_alias_is_normalized_by_cli() {
        let cli =
            Cli::try_parse_from(["omni", "list", "--provider", "cursor"]).expect("valid alias");
        let Commands::List(args) = cli.command.expect("subcommand") else {
            panic!("list command");
        };
        assert_eq!(args.provider, Some(Provider::CursorCli));
    }

    #[test]
    fn shim_install_contract_requires_binary_directory() {
        let cli = Cli::try_parse_from(["omni", "shim", "install", "--bin-dir", "/opt/omni/bin"])
            .expect("valid shim install");
        let Commands::Shim(args) = cli.command.expect("subcommand") else {
            panic!("shim command");
        };
        let ShimCommand::Install(args) = args.command else {
            panic!("shim install command");
        };
        assert_eq!(args.bin_dir, Path::new("/opt/omni/bin"));
    }

    #[test]
    fn shim_exec_keeps_provider_arguments_opaque() {
        let cli = Cli::try_parse_from([
            "omni",
            "shim",
            "exec",
            "cursor-agent",
            "--",
            "--resume",
            "chat-id",
        ])
        .expect("valid shim exec");
        let Commands::Shim(args) = cli.command.expect("subcommand") else {
            panic!("shim command");
        };
        let ShimCommand::Exec(args) = args.command else {
            panic!("shim exec command");
        };
        assert_eq!(args.provider, Provider::CursorCli);
        assert_eq!(
            args.args,
            [OsString::from("--resume"), OsString::from("chat-id")]
        );
    }

    #[test]
    fn shim_routes_only_documented_implicit_resume_forms() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();

        assert!(recognized_resume_prefix(Provider::Claude, &args(&["--continue"])).is_some());
        assert!(recognized_resume_prefix(Provider::Codex, &args(&["resume", "--last"])).is_some());
        assert!(recognized_resume_prefix(Provider::OpenCode, &args(&["-c"])).is_some());
        assert!(recognized_resume_prefix(Provider::Grok, &args(&["--resume"])).is_some());
        assert!(recognized_resume_prefix(Provider::Hermes, &args(&["--resume"])).is_some());
        assert!(recognized_resume_prefix(Provider::Antigravity, &args(&["--continue"])).is_some());
        assert!(recognized_resume_prefix(Provider::Pi, &args(&["--resume"])).is_some());
        assert!(recognized_resume_prefix(Provider::CursorCli, &args(&["resume"])).is_some());

        assert_eq!(
            recognized_resume_prefix(
                Provider::Claude,
                &args(&["--dangerously-skip-permissions", "--continue"])
            ),
            Some(args(&["--dangerously-skip-permissions"]))
        );
        assert_eq!(
            recognized_resume_prefix(Provider::Codex, &args(&["--yolo", "resume"])),
            Some(args(&["--yolo"]))
        );

        assert!(
            recognized_resume_prefix(Provider::Claude, &args(&["--continue", "fix this"]))
                .is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::Codex, &args(&["resume", "explicit-id"])).is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::OpenCode, &args(&["--session", "explicit-id"]))
                .is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::Grok, &args(&["--resume", "explicit-id"])).is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::Hermes, &args(&["--resume", "explicit-id"]))
                .is_none()
        );
        assert!(
            recognized_resume_prefix(
                Provider::Antigravity,
                &args(&["--conversation", "explicit-id"])
            )
            .is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::Pi, &args(&["--session", "explicit-id"])).is_none()
        );
        assert!(
            recognized_resume_prefix(Provider::CursorCli, &args(&["--resume", "explicit-id"]))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn shim_path_guidance_quotes_shell_metacharacters() {
        assert_eq!(
            shell_quote(Path::new("/tmp/omni's shims")),
            "'/tmp/omni'\"'\"'s shims'"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn owned_shim_validation_rejects_other_targets() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("omni");
        let other = directory.path().join("other");
        std::fs::write(&target, b"omni").expect("write target");
        std::fs::write(&other, b"other").expect("write other");
        #[cfg(unix)]
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
            .expect("make target executable");
        let shim = directory.path().join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });

        create_shim_link(&target, &shim).expect("create owned shim");
        validate_owned_shim(&shim, &target, false).expect("accept owned shim");
        std::fs::remove_file(&shim).expect("remove owned shim");
        create_shim_link(&other, &shim).expect("create foreign shim");
        assert!(validate_owned_shim(&shim, &target, false).is_err());
    }

    #[test]
    fn structured_sensitive_fields_are_redacted() {
        let mut value = json!({
            "nested": {
                "refresh_token": "arbitrary-value",
                "x-api-key": "another-value"
            },
            "safe": "visible"
        });

        assert!(redact_json_secrets(&mut value));
        assert_eq!(
            value["nested"]["refresh_token"],
            "[REDACTED: SENSITIVE_FIELD]"
        );
        assert_eq!(value["nested"]["x-api-key"], "[REDACTED: SENSITIVE_FIELD]");
        assert_eq!(value["safe"], "visible");
    }
}
