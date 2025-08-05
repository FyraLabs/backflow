mod backend;
mod config;
mod device_filter;
mod feedback;
mod input;
mod output;
mod protos;
use eyre::Result;

pub const PACKET_PROCESSING_TARGET: &str = "backflow::packet_processing";
pub const CHANNEL_BUFFER_SIZE: usize = 2000; // Size of the channel buffer for input events

pub fn build_logger() -> Result<()> {
    // Create an env filter that defaults to "info" level if RUST_LOG is not set
    // But also specifically filters out noisy zbus and tungstenite logs
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("trace")
            // Silence zbus crates - they can be very verbose
            .add_directive("zbus=warn".parse().unwrap())
            .add_directive("zbus_names=warn".parse().unwrap())
            .add_directive("zvariant=warn".parse().unwrap())
            // Silence tungstenite websocket crate - also very verbose
            .add_directive("tungstenite=warn".parse().unwrap())
            .add_directive("tokio_tungstenite=warn".parse().unwrap())
            .add_directive("async_io=warn".parse().unwrap())
            .add_directive(format!("{PACKET_PROCESSING_TARGET}=warn").parse().unwrap())
        // Keep our application logs at trace level
        // .add_directive("Backflow=trace".parse().unwrap())
    });

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE) // Global ceiling - allows up to TRACE
        .with_env_filter(env_filter) // Runtime filtering with custom rules
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true)
        .try_init()
        .map_err(|e| eyre::eyre!("Failed to initialize logger: {}", e))?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    build_logger()?;
    tracing::debug!("Debug logging is enabled");
    tracing::info!("Backflow version: {}", env!("CARGO_PKG_VERSION"));
    tracing::debug!("Logging configured to suppress zbus and tungstenite verbose output");
    tracing::debug!(
        "Override with RUST_LOG environment variable if needed (e.g., RUST_LOG=zbus=trace)"
    );

    // Load configuration
    let config = config::AppConfig::load_or_default();
    tracing::info!("Configuration loaded successfully");
    tracing::debug!("Active configuration: {:?}", config);

    // Create and run the backend
    backend::setup_and_run_backend(config).await?;

    tracing::info!("Application shutdown complete");
    Ok(())
}
