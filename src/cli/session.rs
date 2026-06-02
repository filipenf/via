use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

use crate::nvim::{self, DiagnosticsOutput};
use crate::session;

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List live via sessions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the session referenced by `VIA_SESSION`.
    Get {
        #[arg(long)]
        json: bool,
    },
    /// Export Neovim diagnostics for the current session.
    Diagnostics {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::List { json } => run_list(json),
        SessionCommand::Get { json } => run_get(json),
        SessionCommand::Diagnostics { file, json } => run_diagnostics(file.as_deref(), json).await,
    }
}

fn run_list(json: bool) -> Result<()> {
    let sessions = session::list_live_sessions()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    if sessions.is_empty() {
        println!("no live via sessions found");
        return Ok(());
    }

    for session in sessions {
        println!(
            "pid={} cwd={} nvim_socket={}",
            session.pid,
            session.cwd.display(),
            session.nvim_socket.display()
        );
    }
    Ok(())
}

fn run_get(json: bool) -> Result<()> {
    let session = session::resolve_session()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&session)?);
    } else {
        println!(
            "pid={} cwd={} nvim_socket={} editor_socket={}",
            session.pid,
            session.cwd.display(),
            session.nvim_socket.display(),
            session.editor_socket.display()
        );
    }
    Ok(())
}

async fn run_diagnostics(file: Option<&Path>, json: bool) -> Result<()> {
    let session = session::resolve_session()?;
    let report = nvim::get_diagnostics(&session.nvim_socket, file).await?;
    let output = DiagnosticsOutput {
        repo: session.cwd.clone(),
        report,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    print_human_diagnostics(&output);
    Ok(())
}

fn print_human_diagnostics(output: &DiagnosticsOutput) {
    let summary = &output.report.summary;
    println!(
        "repo={} path={} errors={} warnings={} infos={} hints={}",
        output.repo.display(),
        output.report.path.display(),
        summary.errors,
        summary.warnings,
        summary.infos,
        summary.hints
    );

    for item in &output.report.items {
        println!(
            "{}:{}:{} {}: {}",
            output.report.path.display(),
            item.lnum,
            item.col,
            item.severity,
            item.message
        );
    }
}
