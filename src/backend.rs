//! Backend module for Plumbershim
//!
//! This module handles taking in a configuration, parsing it, and setting up the backend services.

use crate::config::AppConfig;
use crate::device_filter::DeviceFilter;
use crate::feedback::FeedbackEventStream;
use crate::feedback::generators::chuni_jvs::ChuniLedDataPacket;
use crate::input::{InputBackend, InputEventStream};
use crate::output::{OutputBackend, OutputBackendType};
use eyre::Result;
use futures::future::select_all;
use std::future::Future;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Manages the lifecycle of background services (tasks)
struct ServiceManager {
    handles: Vec<JoinHandle<()>>,
}

impl ServiceManager {
    /// Creates a new, empty ServiceManager.
    fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    /// Spawns a new task and adds its handle to the manager.
    fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handles.push(tokio::spawn(future));
    }

    /// Aborts all managed tasks.
    fn shutdown(&self) {
        tracing::info!("Aborting all service tasks...");
        for handle in &self.handles {
            handle.abort();
        }
    }

    /// Waits for any of the managed services to complete.
    /// This is useful for detecting unexpected shutdowns.
    async fn wait_for_any_completion(&mut self) {
        if self.handles.is_empty() {
            // If there are no tasks, wait indefinitely.
            std::future::pending::<()>().await;
            return;
        }
        // `select_all` waits for the first future to complete.
        let (result, index, _) = select_all(self.handles.iter_mut()).await;
        tracing::warn!("Service task at index {} completed unexpectedly.", index);
        if let Err(e) = result {
            if e.is_panic() {
                tracing::error!("The task panicked!");
            }
        }
    }
}

/// Message streams container for passing around shared streams
pub struct MessageStreams {
    /// Raw input events from all input backends
    pub raw_input: InputEventStream,
    /// Feedback events for all backends
    pub feedback: FeedbackEventStream,
    /// Dedicated stream for uinput backend (standard evdev events)
    pub uinput_input: InputEventStream,
    /// Dedicated stream for chuniio backend (CHUNIIO_ prefixed events)
    pub chuniio_input: InputEventStream,
}

/// Represents the actual backend service
pub struct Backend {
    config: AppConfig,
    streams: MessageStreams,
    device_filter: DeviceFilter,
    service_manager: ServiceManager,
}

impl Backend {
    /// Create a new backend from configuration
    pub fn new(config: AppConfig) -> Self {
        let device_filter = DeviceFilter::new(&config);
        let streams = MessageStreams {
            raw_input: InputEventStream::new(),
            feedback: FeedbackEventStream::new(),
            uinput_input: InputEventStream::new(),
            chuniio_input: InputEventStream::new(),
        };

        Self {
            config,
            streams,
            device_filter,
            service_manager: ServiceManager::new(),
        }
    }

    /// Start all configured backend services
    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting backend services...");

        let (led_packet_tx, led_packet_rx) =
            crate::feedback::generators::chuni_jvs::create_chuni_led_channel();

        // Start all services, managed by the ServiceManager
        self.start_input_services();
        self.start_device_filter_service();
        self.start_output_services(led_packet_tx);
        self.start_feedback_services(led_packet_rx);

        tracing::info!("All backend services started successfully");
        Ok(())
    }

    /// Start input services based on configuration
    fn start_input_services(&mut self) {
        // Start web input backend if configured and enabled
        if let Some(web_config) = &self.config.input.web {
            if web_config.enabled {
                tracing::info!(
                    "Starting web input backend on {}:{}",
                    web_config.host,
                    web_config.port
                );

                let bind_addr_str = format!("{}:{}", web_config.host, web_config.port);
                let input_stream = self.streams.raw_input.clone();
                let feedback_stream = self.streams.feedback.clone();

                self.service_manager.spawn(async move {
                    match bind_addr_str.parse::<SocketAddr>() {
                        Ok(bind_addr) => {
                            use crate::input::web::WebServer;
                            let mut ws_backend = WebServer::auto_detect_web_ui(
                                bind_addr,
                                input_stream,
                                feedback_stream,
                            );
                            if let Err(e) = ws_backend.run().await {
                                tracing::error!("WebSocket backend error: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Invalid web backend address: {}", e);
                        }
                    }
                });
            } else {
                tracing::info!("Web input backend is disabled");
            }
        }

        // Start unix domain socket input backend if configured
        if let Some(unix_config) = &self.config.input.unix {
            tracing::info!(
                "Starting unix socket input backend at {}",
                unix_config.path.display()
            );
            let input_stream = self.streams.raw_input.clone();
            let feedback_stream = self.streams.feedback.clone();
            let socket_path = unix_config.path.clone();
            self.service_manager.spawn(async move {
                use crate::input::unix_socket::UnixSocketServer;
                let mut unix_backend =
                    UnixSocketServer::new(socket_path, input_stream, feedback_stream);
                if let Err(e) = unix_backend.run().await {
                    tracing::error!("Unix socket backend error: {}", e);
                }
            });
        }

        // Start Brokenithm backend if configured and enabled
        if let Some(brokenithm_config) = &self.config.input.brokenithm {
            if brokenithm_config.enabled {
                tracing::info!(
                    "Starting Brokenithm TCP client backend to {}:{}",
                    brokenithm_config.host,
                    brokenithm_config.port
                );
                let connect_addr_str =
                    format!("{}:{}", brokenithm_config.host, brokenithm_config.port);
                let input_stream = self.streams.raw_input.clone();
                let feedback_stream = self.streams.feedback.clone();
                self.service_manager.spawn(async move {
                    match connect_addr_str.parse::<SocketAddr>() {
                        Ok(connect_addr) => {
                            use crate::input::brokenithm::{
                                BrokenithmTcpBackend, BrokenithmTcpConfig,
                            };
                            let mut backend = BrokenithmTcpBackend::new(
                                BrokenithmTcpConfig::Client { connect_addr },
                                input_stream,
                                feedback_stream,
                            );
                            if let Err(e) = backend.run().await {
                                tracing::error!("Brokenithm TCP client backend error: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Invalid Brokenithm TCP backend address: {}", e);
                        }
                    }
                });

                // Also start iDevice client backend if configured
                if let Some(idevice_config) = &brokenithm_config.idevice {
                    if idevice_config.enabled {
                        tracing::info!(
                            "Starting Brokenithm iDevice client backend: device_port={} local_port={} udid={:?}",
                            idevice_config.device_port,
                            idevice_config.local_port,
                            idevice_config.udid
                        );
                        let input_stream = self.streams.raw_input.clone();
                        let feedback_stream = self.streams.feedback.clone();
                        let local_port = idevice_config.local_port;
                        let device_port = idevice_config.device_port;
                        let udid = idevice_config.udid.clone();
                        self.service_manager.spawn(async move {
                            use crate::input::brokenithm::{
                                BrokenithmTcpBackend, BrokenithmTcpConfig,
                            };
                            let mut backend = BrokenithmTcpBackend::new(
                                BrokenithmTcpConfig::DeviceProxy {
                                    local_port,
                                    device_port,
                                    udid,
                                },
                                input_stream,
                                feedback_stream,
                            );
                            if let Err(e) = backend.run().await {
                                tracing::error!("Brokenithm iDevice client backend error: {}", e);
                            }
                        });
                    } else {
                        tracing::info!("Brokenithm iDevice client backend is disabled");
                    }
                }
            } else {
                tracing::info!("Brokenithm TCP client backend is disabled");
            }
        }
    }

    /// Start device filter and routing service that transforms raw input events and routes them to appropriate backends
    fn start_device_filter_service(&mut self) {
        tracing::info!("Starting device filter and routing service");

        let raw_input_stream = self.streams.raw_input.clone();
        let uinput_output_stream = self.streams.uinput_input.clone();
        let chuniio_output_stream = self.streams.chuniio_input.clone();
        let device_filter = self.device_filter.clone();

        self.service_manager.spawn(async move {
            loop {
                // Receive from raw input stream
                if let Some(packet) = raw_input_stream.receive().await {
                    match device_filter.transform_packet(packet) {
                        Ok(transformed_packet) => {
                            // Route events based on their type and device configuration
                            let mut uinput_events = Vec::new();
                            let mut chuniio_events = Vec::new();

                            for event in &transformed_packet.events {
                                match event {
                                    crate::input::InputEvent::Keyboard(keyboard_event) => {
                                        let key = match keyboard_event {
                                            crate::input::KeyboardEvent::KeyPress { key } => key,
                                            crate::input::KeyboardEvent::KeyRelease { key } => key,
                                        };

                                        if key.starts_with("CHUNIIO_") {
                                            // Route CHUNIIO events to chuniio backend
                                            chuniio_events.push(event.clone());
                                        } else {
                                            // Route standard evdev events to uinput backend
                                            uinput_events.push(event.clone());
                                        }
                                    }
                                    // Route non-keyboard events to uinput by default
                                    _ => {
                                        uinput_events.push(event.clone());
                                    }
                                }
                            }

                            // Send to appropriate backends only if they have events
                            if !uinput_events.is_empty() {
                                let uinput_packet = crate::input::InputEventPacket {
                                    device_id: transformed_packet.device_id.clone(),
                                    timestamp: transformed_packet.timestamp,
                                    events: uinput_events,
                                };

                                tracing::trace!(
                                    "Routing {} events from device '{}' to uinput backend",
                                    uinput_packet.events.len(),
                                    uinput_packet.device_id
                                );

                                if let Err(e) = uinput_output_stream.send(uinput_packet).await {
                                    tracing::error!(
                                        "Failed to send packet to uinput backend: {}",
                                        e
                                    );
                                }
                            }

                            if !chuniio_events.is_empty() {
                                let chuniio_packet = crate::input::InputEventPacket {
                                    device_id: transformed_packet.device_id.clone(),
                                    timestamp: transformed_packet.timestamp,
                                    events: chuniio_events,
                                };

                                tracing::trace!(
                                    "Routing {} events from device '{}' to chuniio backend",
                                    chuniio_packet.events.len(),
                                    chuniio_packet.device_id
                                );

                                if let Err(e) = chuniio_output_stream.send(chuniio_packet).await {
                                    tracing::error!(
                                        "Failed to send packet to chuniio backend: {}",
                                        e
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to transform packet: {}", e);
                        }
                    }
                } else {
                    tracing::debug!("Raw input stream closed, shutting down device filter");
                    break;
                }
            }
        });
    }

    /// Start output services based on configuration
    fn start_output_services(
        &mut self,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>, // switched to unbounded Sender
    ) {
        if self.config.output.uinput.enabled {
            tracing::info!("Starting uinput output backend");

            let input_stream = self.streams.uinput_input.clone();
            self.service_manager.spawn(async move {
                let output_result = crate::output::udev::UdevOutput::new(input_stream);
                match output_result {
                    Ok(udev_output) => {
                        let mut output = OutputBackendType::Udev(udev_output);
                        if let Err(e) = output.run().await {
                            tracing::error!("Udev output backend runtime error: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to create UdevOutput: {}. This is usually due to missing /dev/uinput device, insufficient permissions, or uinput kernel module not loaded. Try: sudo modprobe uinput && sudo chmod 666 /dev/uinput",
                            e
                        );
                        tracing::warn!(
                            "Udev output backend disabled due to initialization failure"
                        );
                    }
                }
            });
        } else {
            tracing::info!("Uinput output backend is disabled");
        }

        // Start chuniio proxy backend if configured and enabled
        if let Some(chuniio_config) = &self.config.output.chuniio_proxy {
            if chuniio_config.enabled {
                tracing::info!(
                    "Starting chuniio proxy output backend on socket: {:?}",
                    chuniio_config.socket_path
                );

                let input_stream = self.streams.chuniio_input.clone();
                let feedback_stream = self.streams.feedback.clone();
                let socket_path = chuniio_config.socket_path.clone();

                self.service_manager.spawn(async move {
                    let mut output = OutputBackendType::ChuniioProxy(
                        crate::output::chuniio_proxy::ChuniioProxyServer::new(
                            Some(socket_path),
                            input_stream,
                            feedback_stream,
                            led_packet_tx,
                        ),
                    );

                    if let Err(e) = output.run().await {
                        tracing::error!("Chuniio proxy backend error: {}", e);
                    }
                });
            } else {
                tracing::info!("Chuniio proxy output backend is disabled");
            }
        }
    }

    /// Start feedback services based on configuration
    fn start_feedback_services(
        &mut self,
        led_packet_rx: mpsc::UnboundedReceiver<ChuniLedDataPacket>, // switched to unbounded Receiver
    ) {
        // Check if chuniio feedback is configured
        if let Some(chuniio_config) = &self.config.feedback.chuniio {
            if let Some(socket_path) = &chuniio_config.socket_path {
                // Case 1: Read from socket and parse to feedback flow
                tracing::info!(
                    "Starting ChuniIO RGB feedback service with JVS reader on socket: {:?}",
                    socket_path
                );

                // Start both the JVS reader and the RGB service
                let config = chuniio_config.clone();
                let feedback_stream = self.streams.feedback.clone();
                let socket_path = socket_path.clone();

                // Create a new channel for the JVS reader to send packets
                let (jvs_led_tx, jvs_led_rx) =
                    crate::feedback::generators::chuni_jvs::create_chuni_led_channel();

                // Start JVS reader
                self.service_manager.spawn(async move {
                    use crate::feedback::generators::chuni_jvs::ChuniJvsReader;
                    let mut reader = ChuniJvsReader::new(socket_path, jvs_led_tx);
                    if let Err(e) = reader.run().await {
                        tracing::error!("ChuniIO JVS reader error: {}", e);
                    }
                });

                // Start RGB service fed by JVS reader
                self.service_manager.spawn(async move {
                    use crate::feedback::generators::chuni_jvs::run_chuniio_service;
                    if let Err(e) = run_chuniio_service(config, feedback_stream, jvs_led_rx).await {
                        tracing::error!("ChuniIO RGB feedback service error: {}", e);
                    }
                });

                // Discard LED packets from chuniio_proxy since we're using JVS reader
                self.service_manager.spawn(async move {
                    let mut led_packet_rx = led_packet_rx;
                    while let Some(_packet) = led_packet_rx.recv().await {
                        // Discard packets to prevent channel closure
                    }
                    // Channel closed - keep the service alive as a no-op
                    tracing::debug!(
                        "LED packet channel closed for JVS-only mode, continuing as no-op service"
                    );
                    std::future::pending::<()>().await;
                });
            } else {
                // Case 2: Use chuniio_proxy to feed RGB feedback data (socket_path is None)
                tracing::info!(
                    "Starting ChuniIO RGB feedback service (fed by chuniio_proxy, no socket)"
                );

                // Start the unified service that processes packets from chuniio_proxy
                let config = chuniio_config.clone();
                let feedback_stream = self.streams.feedback.clone();

                self.service_manager.spawn(async move {
                    use crate::feedback::generators::chuni_jvs::run_chuniio_service;
                    if let Err(e) =
                        run_chuniio_service(config, feedback_stream, led_packet_rx).await
                    {
                        tracing::error!("ChuniIO RGB feedback service error: {}", e);
                    }
                });
            }
        } else {
            // No chuniio feedback configured - start a dummy service to consume LED packets
            // This prevents "channel closed" errors in chuniio_proxy
            tracing::debug!("No ChuniIO feedback configured, starting LED packet drain service");
            self.service_manager.spawn(async move {
                let mut led_packet_rx = led_packet_rx;
                loop {
                    match led_packet_rx.recv().await {
                        Some(_packet) => {
                            // Discard packets to prevent channel closure
                        }
                        None => {
                            // Channel closed - this is expected when no chuniio_proxy is configured
                            // Keep the service running as a no-op to avoid unexpected completion
                            tracing::debug!(
                                "LED packet channel closed, continuing as no-op service"
                            );
                            std::future::pending::<()>().await;
                        }
                    }
                }
            });
        }
    }

    /// Wait for all services to complete or handle shutdown
    pub async fn wait_for_shutdown(&mut self) -> Result<()> {
        tracing::info!("Waiting for shutdown signal...");

        tokio::select! {
            // Wait for Ctrl+C
            signal_result = tokio::signal::ctrl_c() => {
                match signal_result {
                    Ok(_) => tracing::info!("Received Ctrl+C, shutting down gracefully..."),
                    Err(e) => tracing::error!("Failed to listen for Ctrl+C: {}", e),
                }
            }
            // Wait for any service to complete (which might indicate an error)
            _ = self.service_manager.wait_for_any_completion() => {
                tracing::warn!("One or more services completed unexpectedly, shutting down...");
            }
        }

        self.shutdown().await?;
        Ok(())
    }

    /// Gracefully shutdown all services
    pub async fn shutdown(&mut self) -> Result<()> {
        tracing::info!("Shutting down backend services...");
        self.service_manager.shutdown();
        tracing::info!("Backend shutdown complete");
        Ok(())
    }
}

/// Convenience function to create and start a backend from configuration
pub async fn setup_and_run_backend(config: AppConfig) -> Result<()> {
    let mut backend = Backend::new(config);
    backend.start().await?;
    backend.wait_for_shutdown().await?;
    Ok(())
}
