use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::nvim::{self, DiagnosticsOutput};
use crate::session;

pub struct SessionCli {
    pub subcommand: SessionSubcommand,
}

pub enum SessionSubcommand {
    List { json: bool },
    Get { json: bool },
    Diagnostics {
        file: Option<PathBuf>,
        json: bool,
    },
}

impl SessionCli {
    pub fn parse(args: &[String]) -> Result<Self> {
        let Some(subcommand) = args.first().map(String::as_str) else {
            bail!("missing session subcommand (list, get, diagnostics)");
        };

        let subcommand = match subcommand {
            "list" => SessionSubcommand::List {
                json: args.contains(&"--json".to_string()),
            },
            "get" => SessionSubcommand::Get {
                json: args.contains(&"--json".to_string()),
            },
            "diagnostics" => SessionSubcommand::Diagnostics {
                file: optional_flag_value(args, "--file")?,
                json: args.contains(&"--json".to_string()),
            },
            other => bail!("unknown session subcommand `{other}`"),
        };

        Ok(Self { subcommand })
    }
}

pub async fn run(command: SessionCli) -> Result<()> {
    match command.subcommand {
        SessionSubcommand::List { json } => run_list(json),
        SessionSubcommand::Get { json } => run_get(json),
        SessionSubcommand::Diagnostics { file, json } => {
            run_diagnostics(file.as_deref(), json).await
        }
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

fn optional_flag_value(args: &[String], flag: &str) -> Result<Option<PathBuf>> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    let Some(value) = args.get(index + 1) else {
        bail!("missing value for `{flag}`");
    };
    Ok(Some(PathBuf::from(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_list() {
        let command = SessionCli::parse(&["list".to_string(), "--json".to_string()]).unwrap();
        assert!(matches!(
            command.subcommand,
            SessionSubcommand::List { json: true }
        ));
    }

    #[test]
    fn parses_session_diagnostics() {
        let command = SessionCli::parse(&[
            "diagnostics".to_string(),
            "--file".to_string(),
            "src/main.rs".to_string(),
            "--json".to_string(),
        ])
        .unwrap();
        assert!(matches!(
            command.subcommand,
            SessionSubcommand::Diagnostics {
                json: true,
                file: Some(_),
                ..
            }
        ));
    }
}
