//! Backend module for Backflow
//!
//! This module handles taking in a configuration, parsing it, and setting up the backend services.

use crate::config::AppConfig;
use crate::device_filter::DeviceFilter;
use crate::feedback::generators::chuni_jvs::ChuniLedDataPacket;
use crate::input::io_server::{IoEventServer, IoOutboundMessage};
use crate::input::{InputBackend, InputEventPacket, InputEventStream};
use crate::output::{OutputBackend, OutputBackendType};
use eyre::Result;
use futures::future::select_all;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
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

/// Routing rule to determine which output backend should handle an event
#[derive(Debug, Clone)]
pub enum RoutingRule {
    /// Route events based on key prefix (e.g., "CHUNIIO_" goes to "chuniio")  
    KeyPrefix { prefix: String, backend: String },
    /// Route all other events to a default backend
    Default { backend: String },
}

/// Event router that determines where events should be sent
/// Can be configured with device-specific routing rules
#[derive(Debug, Clone)]
pub struct EventRouter {
    rules: Vec<RoutingRule>,
}

impl EventRouter {
    /// Create a new router with default rules
    pub fn new() -> Self {
        Self {
            rules: vec![
                RoutingRule::KeyPrefix {
                    prefix: "CHUNIIO_".to_string(),
                    backend: "chuniio".to_string(),
                },
                RoutingRule::Default {
                    backend: "uinput".to_string(),
                },
            ],
        }
    }

    /// Add a new routing rule
    pub fn add_rule(&mut self, rule: RoutingRule) {
        self.rules.push(rule);
    }

    /// Determine which backend should handle an event, optionally using device-specific configuration
    pub fn route_event(
        &self,
        event: &crate::input::InputEvent,
        device_filter: Option<&DeviceFilter>,
        device_id: &str,
    ) -> Option<String> {
        // First check if there's a device-specific backend configuration
        if let Some(filter) = device_filter {
            if let Some(backend) = filter.get_device_backend(device_id) {
                return Some(backend.to_string());
            }
        }

        // Fall back to rule-based routing
        self.route_event_by_rules(event)
    }

    /// Route event based on configured rules (legacy method)
    pub fn route_event_by_rules(&self, event: &crate::input::InputEvent) -> Option<String> {
        match event {
            crate::input::InputEvent::Keyboard(keyboard_event) => {
                let key = match keyboard_event {
                    crate::input::KeyboardEvent::KeyPress { key } => key,
                    crate::input::KeyboardEvent::KeyRelease { key } => key,
                };

                // Check prefix rules first
                for rule in &self.rules {
                    match rule {
                        RoutingRule::KeyPrefix { prefix, backend } => {
                            if key.starts_with(prefix) {
                                return Some(backend.clone());
                            }
                        }
                        RoutingRule::Default { .. } => continue,
                    }
                }

                // Fall back to default rule
                for rule in &self.rules {
                    if let RoutingRule::Default { backend } = rule {
                        return Some(backend.clone());
                    }
                }

                None
            }
            // Route non-keyboard events to default backend
            _ => {
                for rule in &self.rules {
                    if let RoutingRule::Default { backend } = rule {
                        return Some(backend.clone());
                    }
                }
                None
            }
        }
    }
}

impl Default for EventRouter {
    fn default() -> Self {
        Self::new()
    }
}
/// Message streams container maintaining per-output backend input streams
#[derive(Clone)]
pub struct MessageStreams {
    /// Output streams keyed by backend name
    output_streams: std::collections::HashMap<String, InputEventStream>,
}

impl MessageStreams {
    pub fn new() -> Self {
        let mut output_streams = std::collections::HashMap::new();
        // Default backends
        output_streams.insert("uinput".to_string(), InputEventStream::new());
        output_streams.insert("chuniio".to_string(), InputEventStream::new());
        Self { output_streams }
    }
    /// Ensure a stream exists for the given backend name; returns true if newly created.
    pub fn ensure_stream(&mut self, backend_name: &str) -> bool {
        if self.output_streams.contains_key(backend_name) {
            return false;
        }
        self.output_streams
            .insert(backend_name.to_string(), InputEventStream::new());
        true
    }
    pub fn register_output_stream(&mut self, backend_name: &str) -> InputEventStream {
        let stream = InputEventStream::new();
        self.output_streams
            .insert(backend_name.to_string(), stream.clone());
        stream
    }
    pub fn clone_output_stream(&self, backend_name: &str) -> Option<InputEventStream> {
        self.output_streams.get(backend_name).cloned()
    }
    #[allow(dead_code)]
    pub fn output_backend_names(&self) -> impl Iterator<Item = &String> {
        self.output_streams.keys()
    }
}

impl Default for MessageStreams {
    fn default() -> Self {
        Self::new()
    }
}

/// Represents the actual backend service
pub struct Backend {
    config: AppConfig,
    device_filter: DeviceFilter,
    event_router: EventRouter,
    service_manager: ServiceManager,
    io_server: Arc<IoEventServer>,
    streams: MessageStreams,
}

impl Backend {
    /// Create a new backend from configuration
    pub fn new(config: AppConfig) -> Self {
        let device_filter = DeviceFilter::new(&config);
        let event_router = EventRouter::new();

        let io_server = Arc::new(IoEventServer::new());
        // Spawn unified IO server processing loop now (bridged below)
        let io_server_clone = io_server.clone();
        tokio::spawn(async move {
            if let Err(e) = io_server_clone.run_forever().await {
                tracing::error!("IoEventServer error: {}", e);
            }
        });
        // Initialize message streams (defaults for built-in backends)
        let mut streams = MessageStreams::new();
        // Auto-register any additional backend names referenced in device config
        // so routing can emit to them without needing an output service yet.
        // This makes adding future output backends config-first.
        for (_dev_id, dev_cfg) in &config.device {
            let backend_name = dev_cfg.map_backend.as_str();
            // Ensure a stream exists for each referenced backend.
            streams.ensure_stream(backend_name);
        }
        Self {
            config,
            device_filter,
            event_router,
            service_manager: ServiceManager::new(),
            io_server,
            streams,
        }
    }

    /// Add a custom routing rule for event handling
    pub fn add_routing_rule(&mut self, rule: RoutingRule) {
        self.event_router.add_rule(rule);
    }

    /// Register a new output backend stream
    pub fn register_output_backend(&mut self, backend_name: &str) -> InputEventStream {
        self.streams.register_output_stream(backend_name)
    }

    /// Spawn a service task with automatic management
    fn spawn_service<F>(&mut self, name: &str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tracing::debug!("Spawning service: {}", name);
        self.service_manager.spawn(future);
    }

    /// Start all configured backend services
    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting backend services...");

        let (led_packet_tx, led_packet_rx) =
            crate::feedback::generators::chuni_jvs::create_chuni_led_channel();
        self.start_input_services();
        self.start_output_services(led_packet_tx);
        self.start_feedback_services(led_packet_rx);
        self.start_routing_service();

        tracing::info!("All backend services started successfully");
        Ok(())
    }

    /// Start input services based on configuration
    fn start_input_services(&mut self) {
        // Start web input backend if configured and enabled
        if let Some(web_config) = &self.config.input.web {
            if web_config.enabled {
                let host = web_config.host.clone();
                let port = web_config.port;
                self.start_web_input_service_with_params(host, port);
            } else {
                tracing::info!("Web input backend is disabled");
            }
        }

        // Start unix domain socket input backend if configured
        if let Some(unix_config) = &self.config.input.unix {
            let path = unix_config.path.clone();
            self.start_unix_socket_service_with_params(path);
        }

        // Start Brokenithm backend if configured and enabled
        if let Some(brokenithm_config) = &self.config.input.brokenithm {
            if brokenithm_config.enabled {
                let host = brokenithm_config.host.clone();
                let port = brokenithm_config.port;

                // Extract idevice config parameters if present
                let idevice_params = if let Some(idevice_config) = &brokenithm_config.idevice {
                    if idevice_config.enabled {
                        Some((
                            idevice_config.local_port,
                            idevice_config.device_port,
                            idevice_config.udid.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                };

                self.start_brokenithm_services_with_params(host, port, idevice_params);
            } else {
                tracing::info!("Brokenithm TCP client backend is disabled");
            }
        }
    }

    /// Start web input service with parameters
    fn start_web_input_service_with_params(&mut self, host: String, port: u16) {
        let bind_addr_str = format!("{host}:{port}");
        tracing::info!("Starting web input backend on {bind_addr_str}");

        let io_server = self.io_server.clone();
        self.spawn_service("web_input", async move {
            match bind_addr_str.parse::<SocketAddr>() {
                Ok(bind_addr) => {
                    use crate::input::web::WebServer;
                    let mut ws_backend = WebServer::auto_detect_web_ui(bind_addr, io_server);
                    if let Err(e) = ws_backend.run().await {
                        tracing::error!("WebSocket backend error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Invalid web backend address: {}", e);
                }
            }
        });
    }

    /// Start Unix socket input service with parameters
    fn start_unix_socket_service_with_params(&mut self, path: std::path::PathBuf) {
        tracing::info!("Starting unix socket input backend at {}", path.display());
        let io_server = self.io_server.clone();
        self.spawn_service("unix_socket", async move {
            use crate::input::unix_socket::UnixSocketServer;
            let mut unix_backend = UnixSocketServer::new(path, io_server);
            if let Err(e) = unix_backend.run().await {
                tracing::error!("Unix socket backend error: {}", e);
            }
        });
    }

    /// Start Brokenithm-related services with parameters
    fn start_brokenithm_services_with_params(
        &mut self,
        host: String,
        port: u16,
        idevice_params: Option<(u16, u16, Option<String>)>,
    ) {
        // Main TCP client
        self.start_brokenithm_tcp_service_with_params(host, port);

        // iDevice proxy if configured
        if let Some((local_port, device_port, udid)) = idevice_params {
            self.start_brokenithm_idevice_service_with_params(local_port, device_port, udid);
        } else {
            tracing::info!("Brokenithm iDevice client backend is disabled or not configured");
        }
    }

    /// Start Brokenithm TCP service with parameters
    fn start_brokenithm_tcp_service_with_params(&mut self, host: String, port: u16) {
        let connect_addr_str = format!("{host}:{port}");
        tracing::info!("Starting Brokenithm TCP client backend to {connect_addr_str}");
        let io_server = self.io_server.clone();
        self.spawn_service("brokenithm_tcp", async move {
            match connect_addr_str.parse::<SocketAddr>() {
                Ok(connect_addr) => {
                    use crate::input::brokenithm::BrokenithmTcpBackend;
                    let inbound_tx = io_server.inbound_sender();
                    let mut backend = BrokenithmTcpBackend::client(connect_addr, inbound_tx);
                    if let Err(e) = backend.run().await {
                        tracing::error!("Brokenithm TCP client backend error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Invalid Brokenithm TCP backend address: {}", e);
                }
            }
        });
    }

    /// Start Brokenithm iDevice service with parameters
    fn start_brokenithm_idevice_service_with_params(
        &mut self,
        local_port: u16,
        device_port: u16,
        udid: Option<String>,
    ) {
        tracing::info!(
            "Starting Brokenithm iDevice client backend: device_port={} local_port={} udid={:?}",
            device_port,
            local_port,
            udid
        );

        let io_server = self.io_server.clone();
        self.spawn_service("brokenithm_idevice", async move {
            use crate::input::brokenithm::BrokenithmTcpBackend;
            let inbound_tx = io_server.inbound_sender();
            let mut backend =
                BrokenithmTcpBackend::device_proxy(local_port, device_port, udid, inbound_tx);
            if let Err(e) = backend.run().await {
                tracing::error!("Brokenithm iDevice client backend error: {}", e);
            }
        });
    }

    /// Start device filter and routing service that transforms raw input events and routes them to appropriate backends
    fn start_routing_service(&mut self) {
        tracing::info!("Starting unified routing service (IoEventServer -> output backends)");
        let io_server = self.io_server.clone();
        let device_filter = self.device_filter.clone();
        let event_router = self.event_router.clone();
        let streams = self.streams.clone();
        self.spawn_service("routing_service", async move {
            if let Some(mut outbound_rx) = io_server.take_outbound_receiver().await {
                while let Some(msg) = outbound_rx.recv().await {
                    match msg {
                        IoOutboundMessage::Input(pkt) => {
                            match device_filter.transform_packet(pkt) {
                                Ok(transformed_packet) => {
                                    // Group by backend
                                    let mut by_backend: std::collections::HashMap<String, Vec<crate::input::InputEvent>> = std::collections::HashMap::new();
                                    for ev in &transformed_packet.events {
                                        if let Some(backend_name) = event_router.route_event(ev, Some(&device_filter), &transformed_packet.device_id) {
                                            by_backend.entry(backend_name).or_default().push(ev.clone());
                                        }
                                    }
                                    for (backend, events) in by_backend {
                                        if events.is_empty() { continue; }
                                        if let Some(stream) = streams.clone_output_stream(&backend) {
                                            let packet = InputEventPacket { device_id: transformed_packet.device_id.clone(), timestamp: transformed_packet.timestamp, events };
                                            if let Err(e) = stream.send(packet.clone()).await { tracing::error!("Failed to send packet to backend {backend}: {e}"); }
                                            // Mirror to client subscribers of this backend/topic
                                            io_server.broadcast_stream_input(&backend, &packet);
                                            // Also mirror to device listeners (clients that did not register streams)
                                            io_server.broadcast_device_input(&packet);
                                        } else {
                                            tracing::warn!("No stream registered for backend {backend}, dropping events");
                                        }
                                    }
                                }
                                Err(e) => tracing::error!("Failed to transform packet: {e}"),
                            }
                        }
                        IoOutboundMessage::Feedback(_fb) => {
                            // Already dispatched to clients; no output backend consumes feedback currently
                        }
                    }
                }
            } else {
                tracing::warn!("Failed to obtain IoEventServer outbound receiver; routing inactive");
            }
        });
    }

    /// Start output services based on configuration
    fn start_output_services(&mut self, led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>) {
        if self.config.output.uinput.enabled {
            self.start_uinput_service();
        } else {
            tracing::info!("Uinput output backend is disabled");
        }

        // Start chuniio proxy backend if configured and enabled
        if let Some(chuniio_config) = &self.config.output.chuniio_proxy {
            if chuniio_config.enabled {
                let socket_path = chuniio_config.socket_path.clone();
                self.start_chuniio_proxy_service_with_params(Some(socket_path), led_packet_tx);
            } else {
                tracing::info!("Chuniio proxy output backend is disabled");
            }
        }
    }

    /// Start uinput output service
    fn start_uinput_service(&mut self) {
        tracing::info!("Starting uinput output backend");

        let input_stream = self
            .streams
            .clone_output_stream("uinput")
            .expect("uinput stream registered");
        self.spawn_service("uinput_output", async move {
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
    }

    /// Start chuniio proxy output service with parameters
    fn start_chuniio_proxy_service_with_params(
        &mut self,
        socket_path: Option<std::path::PathBuf>,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
    ) {
        tracing::info!(
            "Starting chuniio proxy output backend on socket: {:?}",
            socket_path
        );

        let input_stream = self
            .streams
            .clone_output_stream("chuniio")
            .expect("chuniio stream registered");

        self.spawn_service("chuniio_proxy", async move {
            let mut output = OutputBackendType::ChuniioProxy(
                crate::output::chuniio_proxy::ChuniioProxyServer::new(
                    socket_path,
                    input_stream,
                    led_packet_tx,
                ),
            );

            if let Err(e) = output.run().await {
                tracing::error!("Chuniio proxy backend error: {}", e);
            }
        });
    }

    /// Start feedback services based on configuration
    fn start_feedback_services(
        &mut self,
        led_packet_rx: mpsc::UnboundedReceiver<ChuniLedDataPacket>,
    ) {
        // Check if chuniio feedback is configured
        if let Some(chuniio_config) = &self.config.feedback.chuniio {
            if let Some(socket_path) = &chuniio_config.socket_path {
                // Case 1: Read from socket and parse to feedback flow
                let chuniio_config = chuniio_config.clone();
                let socket_path = socket_path.clone();
                self.start_chuniio_jvs_feedback_service(
                    &chuniio_config,
                    &socket_path,
                    led_packet_rx,
                );
            } else {
                // Case 2: Use chuniio_proxy to feed RGB feedback data (socket_path is None)
                let chuniio_config = chuniio_config.clone();
                self.start_chuniio_proxy_feedback_service(&chuniio_config, led_packet_rx);
            }
        } else {
            // No chuniio feedback configured - start a dummy service to consume LED packets
            self.start_led_packet_drain_service(led_packet_rx);
        }
    }

    /// Start ChuniIO JVS reader feedback service
    fn start_chuniio_jvs_feedback_service(
        &mut self,
        chuniio_config: &crate::config::ChuniIoRgbConfig,
        socket_path: &std::path::PathBuf,
        led_packet_rx: mpsc::UnboundedReceiver<ChuniLedDataPacket>,
    ) {
        tracing::info!(
            "Starting ChuniIO RGB feedback service with JVS reader on socket: {:?}",
            socket_path
        );

        let config = chuniio_config.clone();
        let io_server = self.io_server.clone();
        let socket_path = socket_path.clone();

        // Create a new channel for the JVS reader to send packets
        let (jvs_led_tx, jvs_led_rx) =
            crate::feedback::generators::chuni_jvs::create_chuni_led_channel();

        // Start JVS reader
        self.spawn_service("chuniio_jvs_reader", async move {
            use crate::feedback::generators::chuni_jvs::ChuniJvsReader;
            let mut reader = ChuniJvsReader::new(socket_path, jvs_led_tx);
            if let Err(e) = reader.run().await {
                tracing::error!("ChuniIO JVS reader error: {}", e);
            }
        });

        // Start RGB service fed by JVS reader
        self.spawn_service("chuniio_rgb_service", async move {
            use crate::feedback::generators::chuni_jvs::run_chuniio_service;
            if let Err(e) = run_chuniio_service(config, io_server, jvs_led_rx).await {
                tracing::error!("ChuniIO RGB feedback service error: {}", e);
            }
        });

        // Discard LED packets from chuniio_proxy since we're using JVS reader
        self.start_led_packet_drain_service(led_packet_rx);
    }

    /// Start ChuniIO proxy-fed feedback service
    fn start_chuniio_proxy_feedback_service(
        &mut self,
        chuniio_config: &crate::config::ChuniIoRgbConfig,
        led_packet_rx: mpsc::UnboundedReceiver<ChuniLedDataPacket>,
    ) {
        tracing::info!("Starting ChuniIO RGB feedback service (fed by chuniio_proxy, no socket)");

        let config = chuniio_config.clone();
        let io_server = self.io_server.clone();

        self.spawn_service("chuniio_rgb_service", async move {
            use crate::feedback::generators::chuni_jvs::run_chuniio_service;
            if let Err(e) = run_chuniio_service(config, io_server, led_packet_rx).await {
                tracing::error!("ChuniIO RGB feedback service error: {}", e);
            }
        });
    }

    /// Start LED packet drain service (no-op consumer)
    fn start_led_packet_drain_service(
        &mut self,
        led_packet_rx: mpsc::UnboundedReceiver<ChuniLedDataPacket>,
    ) {
        tracing::debug!("Starting LED packet drain service");

        self.spawn_service("led_packet_drain", async move {
            let mut led_packet_rx = led_packet_rx;
            loop {
                match led_packet_rx.recv().await {
                    Some(_packet) => {
                        // Discard packets to prevent channel closure
                    }
                    None => {
                        // Channel closed - this is expected when no chuniio_proxy is configured
                        // Keep the service running as a no-op to avoid unexpected completion
                        tracing::debug!("LED packet channel closed, continuing as no-op service");
                        std::future::pending::<()>().await;
                    }
                }
            }
        });
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
