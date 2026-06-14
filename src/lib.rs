mod acp;
mod agent_skill;
mod bootstrap;
mod cli;
mod config;
mod editor;
mod event;
mod logging;
mod lsp_bridge;
mod mediator;
mod nvim;
mod pty;
mod session;
pub mod ui;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use crate::cli::Cli;
use crate::mediator::Mediator;
use crate::nvim::FileTarget;
use crate::ui::ghostty::GhosttyUi;

/// Library entry point so that benches (and a thin binary) can link against `via`
/// as a crate and reach internal modules via `pub(crate)` items.
pub fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.persist {
        let path = config::persist_resolved(cli.config_overrides())?;
        eprintln!("wrote {}", path.display());
    }

    if cli.command.is_some() {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_headless(cli));
    }

    bootstrap::maybe_detach()?;
    // Single-threaded here (before the runtime starts), so exporting the session handle is
    // safe. Child panes/agents capture the process environment when they are spawned, so they
    // inherit VIA_SESSION and can resolve diagnostics without a `--repo` argument.
    unsafe {
        std::env::set_var(session::VIA_SESSION_ENV, session::manifest_path());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(cli))
}

async fn run_headless(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        return Ok(());
    };
    cli::run(command).await
}

async fn async_main(cli: Cli) -> Result<()> {
    logging::init();
    config::ensure_runtime_dir()?;

    let config = config::Config::load(cli.config_overrides())?;
    info!(?config, "starting via");

    if let Some(open) = cli.open {
        nvim::log_socket_warning(&config.nvim_socket_path);
        let target = FileTarget::parse(&open, &config.working_directory);
        nvim::open_file(&config.nvim_socket_path, &config.working_directory, target).await?;
        return Ok(());
    }

    let _session_guard = session::SessionGuard::create(&config)?;

    if let Some(agent_command) = &config.agent_command {
        let family = agent_skill::detect_agent_family(agent_command);
        match agent_skill::ensure_global_skill(family) {
            Ok(paths) if !paths.is_empty() => {
                info!(
                    agent = %agent_command,
                    count = paths.len(),
                    "installed via-editor skill for agent"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    agent = %agent_command,
                    %err,
                    "failed to install via-editor skill for agent"
                );
            }
        }
    }

    let mut mediator = Mediator::new(config.clone());

    if config.is_acp_agent() {
        if let Some(cmd) = &config.agent_command {
            let tokens: Vec<&str> = cmd.split_whitespace().collect();
            if let [command, args @ ..] = tokens.as_slice() {
                let command = command.to_string();
                let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
                let cmd_for_log = cmd.clone();

                // Connect with a timeout so a non-responsive agent doesn't hang startup.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    mediator.connect_acp(
                        &command,
                        &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    ),
                )
                .await
                {
                    Ok(Ok(session_id)) => {
                        info!(agent = %cmd_for_log, session_id, "ACP agent connected");
                    }
                    Ok(Err(err)) => {
                        tracing::error!(agent = %cmd_for_log, %err, "failed to connect ACP agent");
                    }
                    Err(_) => {
                        tracing::error!(agent = %cmd_for_log, "ACP agent did not respond within timeout");
                    }
                }
            }
        }
    } else if let Some(cmd) = &config.agent_command {
        info!(agent = %cmd, "legacy PTY agent");
    }

    let mut handle = mediator.spawn();
    let ui = GhosttyUi::new(config.clone(), handle.events(), handle.take_ui_commands());
    ui.describe_backend();

    ui.run()?;
    info!("window closed; shutdown requested");

    handle.shutdown().await;
    Ok(())
}
