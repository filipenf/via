use tracing::info;

pub struct GhosttyUi;

impl GhosttyUi {
    pub fn new() -> Self {
        Self
    }

    pub fn describe_backend(&self) {
        info!("libghostty UI backend selected");
    }
}
