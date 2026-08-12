//! kctx — a fast, read-only Kubernetes context switcher and inspector.
//!
//! Layering, from the bottom up:
//!
//! * [`kubeconfig`] — local discovery, parsing and the context model. Never opens a socket.
//! * [`kubernetes`] — read-only cluster access, inspection and health analysis.
//! * [`output`] — machine-readable and human-readable renderers for stdout.
//! * [`cli`] — argument parsing and the non-interactive commands.
//!
//! stdout carries results a shell can consume; the TUI and every diagnostic use stderr.

mod app;
mod cli;
mod filter;
mod kubeconfig;
mod kubernetes;
mod logging;
mod output;
mod overlay;
mod paths;
mod ui;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();

    if let Err(error) = logging::init(cli.log_file.as_deref(), cli.log_level.as_deref()) {
        eprintln!("kctx: warning: could not open log file: {error}");
    }

    match cli::run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("kctx: error: {error}");
            for cause in error.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}
