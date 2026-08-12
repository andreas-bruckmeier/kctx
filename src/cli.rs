//! Command line surface. Only implemented functionality is exposed here.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand, ValueEnum};

use crate::app::AppState;
use crate::kubeconfig::{ContextCatalog, ContextEntry, DiscoverySources, KubeconfigError};
use crate::kubernetes::client::Timeouts;
use crate::kubernetes::{ConnectionState, inspection};
use crate::{output, overlay, ui};

/// Exit code used when the user cancels an interactive selection.
pub const EXIT_CANCELLED: u8 = 130;

/// Fast Kubernetes context switcher and read-only cluster inspector.
///
/// kctx never modifies Kubernetes resources and never writes to your kubeconfig files.
#[derive(Debug, Parser)]
#[command(name = "kctx", version, about, long_about = None)]
pub struct Cli {
    /// What to do. Defaults to `select`.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Write debug logs to this file (logs never go to the terminal).
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Log filter used with --log-file, e.g. `debug` or `kctx=trace`.
    #[arg(long, global = true, value_name = "FILTER")]
    pub log_level: Option<String>,
}

/// Implemented subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List discovered contexts (tab-separated: name, cluster, namespace, source, active).
    List {
        /// Emit JSON instead of tab-separated records.
        #[arg(long)]
        json: bool,
    },

    /// Print the effective context name.
    Current {
        /// Emit JSON instead of a bare name.
        #[arg(long)]
        json: bool,
    },

    /// Select a context and print a KUBECONFIG value for a shell wrapper to export.
    ///
    /// Without an argument this opens the interactive selector on stderr, leaving stdout for
    /// the result.
    Select {
        /// Context to select. Omit to choose interactively.
        context: Option<String>,

        /// Disambiguate a context name defined by more than one kubeconfig.
        #[arg(long, value_name = "PATH")]
        source: Option<PathBuf>,

        /// What to print on stdout.
        #[arg(long, value_enum, default_value_t = PrintKind::Kubeconfig)]
        print: PrintKind,
    },

    /// Inspect a context through the Kubernetes API, read-only.
    ///
    /// Defaults to the effective context. Exits non-zero when the cluster could not be reached,
    /// while still reporting whatever was readable.
    Inspect {
        /// Context to inspect. Omit to inspect the current one.
        context: Option<String>,

        /// Disambiguate a context name defined by more than one kubeconfig.
        #[arg(long, value_name = "PATH")]
        source: Option<PathBuf>,

        /// Namespace to inspect instead of the context's own.
        #[arg(long, short = 'n', value_name = "NAMESPACE")]
        namespace: Option<String>,

        /// Give up after this many seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = 8)]
        timeout: u64,

        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// What `kctx select` writes to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PrintKind {
    /// A `KUBECONFIG` value: the generated current-context overlay plus the source file.
    Kubeconfig,
    /// Only the kubeconfig file that defines the context.
    Path,
    /// Only the context name.
    Context,
}

/// Run the requested command and return the process exit code.
pub fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command.unwrap_or(Command::Select {
        context: None,
        source: None,
        print: PrintKind::Kubeconfig,
    }) {
        Command::List { json } => list(json),
        Command::Current { json } => current(json),
        Command::Select {
            context,
            source,
            print,
        } => select(context.as_deref(), source.as_deref(), print),
        Command::Inspect {
            context,
            source,
            namespace,
            timeout,
            json,
        } => inspect(
            context.as_deref(),
            source.as_deref(),
            namespace.as_deref(),
            Duration::from_secs(timeout),
            json,
        ),
    }
}

/// A single-threaded runtime, built only for the commands that need one.
///
/// The work is purely I/O-bound, so one thread keeps startup cheap.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")
}

/// Load everything discoverable and report unusable files on stderr.
fn load_catalog() -> ContextCatalog {
    let catalog = ContextCatalog::load(&DiscoverySources::from_env());
    report_problems(&catalog);
    catalog
}

/// Warn about kubeconfigs that could not be used. Never fatal: stderr only.
pub fn report_problems(catalog: &ContextCatalog) {
    for problem in &catalog.problems {
        // "Not a kubeconfig" is expected while scanning ~/.kube and is not worth a warning.
        if matches!(problem, KubeconfigError::NotKubeconfig { .. }) {
            continue;
        }
        eprintln!("kctx: warning: {problem}");
    }
}

/// Find exactly one context by name, refusing to guess between identically named ones.
pub fn resolve<'a>(
    catalog: &'a ContextCatalog,
    name: &str,
    source: Option<&std::path::Path>,
) -> anyhow::Result<&'a ContextEntry> {
    let mut candidates = catalog.find_all(name);
    if let Some(source) = source {
        candidates.retain(|entry| entry.source.as_ref() == source);
    }

    match candidates.as_slice() {
        [only] => Ok(only),
        [] => {
            let suggestions = catalog.suggestions(name);
            if suggestions.is_empty() {
                anyhow::bail!("no context named {name:?} was found (try `kctx list`)");
            }
            anyhow::bail!(
                "no context named {name:?}; did you mean: {}",
                suggestions.join(", ")
            );
        }
        many => {
            let sources: Vec<String> = many
                .iter()
                .map(|entry| entry.source.display().to_string())
                .collect();
            anyhow::bail!(
                "context {name:?} is defined by {} kubeconfigs; disambiguate with --source:\n  {}",
                many.len(),
                sources.join("\n  ")
            );
        }
    }
}

/// `kctx list`
fn list(json: bool) -> anyhow::Result<ExitCode> {
    let catalog = load_catalog();

    if catalog.entries.is_empty() {
        anyhow::bail!("no Kubernetes contexts found (looked at $KUBECONFIG and ~/.kube)");
    }

    let mut stdout = std::io::stdout().lock();
    if json {
        writeln!(
            stdout,
            "{}",
            output::list_json(&catalog).context("serialising contexts")?
        )?;
    } else {
        write!(stdout, "{}", output::list_tsv(&catalog))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `kctx current`
fn current(json: bool) -> anyhow::Result<ExitCode> {
    let catalog = load_catalog();

    let Some(name) = catalog.active_name.clone() else {
        anyhow::bail!("no current context is set in any discovered kubeconfig");
    };

    let mut stdout = std::io::stdout().lock();
    if json {
        let rendered =
            output::current_json(catalog.active(), &name).context("serialising context")?;
        writeln!(stdout, "{rendered}")?;
    } else {
        writeln!(stdout, "{name}")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `kctx select [context]`
fn select(
    name: Option<&str>,
    source: Option<&std::path::Path>,
    print: PrintKind,
) -> anyhow::Result<ExitCode> {
    let catalog = load_catalog();

    if let Some(name) = name {
        let entry = resolve(&catalog, name, source)?;
        return print_selection(entry, print);
    }

    if catalog.entries.is_empty() {
        anyhow::bail!("no Kubernetes contexts found (looked at $KUBECONFIG and ~/.kube)");
    }

    let mut state = AppState::new(catalog);
    let chosen = runtime()?.block_on(ui::select(&mut state))?;

    match chosen {
        Some(entry) => print_selection(&entry, print),
        // Nothing on stdout, so `cfg="$(kctx select)" || return` leaves KUBECONFIG alone.
        None => Ok(ExitCode::from(EXIT_CANCELLED)),
    }
}

/// `kctx inspect [context]`
fn inspect(
    name: Option<&str>,
    source: Option<&std::path::Path>,
    namespace: Option<&str>,
    timeout: Duration,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let catalog = load_catalog();

    let entry = match name {
        Some(name) => resolve(&catalog, name, source)?,
        None => match catalog.active() {
            Some(entry) => entry,
            None => match catalog.active_name.as_deref() {
                Some(name) => anyhow::bail!(
                    "the current context {name:?} is not defined by any discovered kubeconfig"
                ),
                None => {
                    anyhow::bail!("no current context is set; name one: `kctx inspect <context>`")
                }
            },
        },
    };

    let timeouts = Timeouts::with_overall(timeout);
    let snapshot = runtime()?.block_on(inspection::inspect(entry, namespace, timeouts));

    let mut stdout = std::io::stdout().lock();
    if json {
        let rendered = output::inspection_json(&snapshot).context("serialising the snapshot")?;
        writeln!(stdout, "{rendered}")?;
    } else {
        write!(stdout, "{}", output::inspection_text(&snapshot))?;
    }
    stdout.flush()?;

    // A snapshot of an unreachable cluster is still worth printing, but scripts need to know.
    Ok(if snapshot.state == ConnectionState::Connected {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Emit the selection on stdout. This is the only thing a shell wrapper consumes.
pub fn print_selection(entry: &ContextEntry, print: PrintKind) -> anyhow::Result<ExitCode> {
    let line = match print {
        PrintKind::Kubeconfig => overlay::prepare(entry)?.kubeconfig,
        PrintKind::Path => entry.source.display().to_string(),
        PrintKind::Context => entry.name.clone(),
    };

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}
