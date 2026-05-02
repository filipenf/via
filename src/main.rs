mod config;
mod event;
mod logging;
mod mediator;
mod pty;
mod ui;

use anyhow::Result;
use tracing::info;

use crate::config::Config;
use crate::mediator::Mediator;
use crate::ui::ghostty::GhosttyUi;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::from_env();
    info!(?config, "starting spectre");

    let ui = GhosttyUi::new(config.clone());
    ui.describe_backend();

    let mediator = Mediator::new(config);
    let handle = mediator.spawn();

    ui.run()?;
    info!("window closed; shutdown requested");

    handle.shutdown().await;
    Ok(())
}
