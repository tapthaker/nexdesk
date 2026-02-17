mod app;
mod welcome;
mod role;
mod network;
mod screens;
mod certificates;
mod service;
pub mod edge_picker;

use color_eyre::eyre::Result;

pub async fn run_setup() -> Result<()> {
    app::run().await
}
