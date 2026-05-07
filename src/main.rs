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

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::from_env();
    info!(?config, "starting spectre");

    if let Some(target) = cli_open_target(&config) {
        nvim::log_socket_warning(&config.nvim_socket_path);
        nvim::open_file(&config.nvim_socket_path, &config.working_directory, target).await?;
        return Ok(());
    }

    let mediator = Mediator::new(config.clone());
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
