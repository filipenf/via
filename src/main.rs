mod acp;
mod bootstrap;
mod config;
mod editor;
mod event;
mod logging;
mod lsp_bridge;
mod mediator;
mod nvim;
mod pty;
mod ui;

use anyhow::Result;
use tracing::info;

use crate::config::Config;
use crate::mediator::Mediator;
use crate::nvim::FileTarget;
use crate::ui::ghostty::GhosttyUi;

fn main() -> Result<()> {
    bootstrap::maybe_detach()?;
    async_main()
}

#[tokio::main]
async fn async_main() -> Result<()> {
    logging::init();

    let config = Config::from_env();
    info!(?config, "starting via");

    if let Some(target) = cli_open_target(&config) {
        nvim::log_socket_warning(&config.nvim_socket_path);
        nvim::open_file(&config.nvim_socket_path, &config.working_directory, target).await?;
        return Ok(());
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
                    mediator.connect_acp(&command, &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
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

fn cli_open_target(config: &Config) -> Option<FileTarget> {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        Some("--open") => args
            .next()
            .map(|target| FileTarget::parse(&target, &config.working_directory)),
        _ => None,
    }
}
