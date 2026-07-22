mod app;
mod certificates;
pub mod edge_picker;
mod flow;
mod network;
mod permissions;
mod role;
mod screens;
mod service;
mod welcome;

use color_eyre::eyre::Result;

pub async fn run_setup() -> Result<()> {
    app::run().await
}
