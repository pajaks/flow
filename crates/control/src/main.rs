use std::net::TcpListener;

use tracing::info;

use control::config;
use control::startup;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    info!("Running in {} mode", config::app_env().as_str());

    let settings = config::settings();
    let listener = TcpListener::bind(settings.application.address())?;
    let server = startup::run(listener)?;

    info!("Listening on http://{}", settings.application.address());

    // The server runs until it receives a shutdown signal.
    server.await?;

    Ok(())
}
