//! proxy socket for chuniio passthrough
//!
//! This backend output exposes a dedicated Unix Domain Socket
//! that can be used to send input events as a virtual CHUNITHM controller.
//!
//!
//! This requires a dedicated chuniio.dll implementation that actually proxies
//! all input events to this socket.
//!
//! The socket is created at `/run/user/<uid>/backflow_chuniio`
//! and will pass through all the normal chuniio poll and feedback events.
//!
//! ## Special Keycodes
//!
//! The following keycodes are exclusive to this backend and when passed,
//! will be sent to the game directly through the socket/chuniio
//!
//! - `CHUNIIO_COIN` - Coin input, equivalent to inserting a credit
//! - `CHUNIIO_TEST` - Test mode input, equivalent to pressing the test button
//! - `CHUNIIO_SERVICE` - Service mode input, equivalent to pressing the service button
//! - `CHUNIIO_SLIDER_[0-31]` - Slider input, equivalent to pressing one of the 32 slider touch zones
//! - `CHUNIIO_IR_[0-5]` - IR input, equivalent to blocking one of the 6 IR sensors
//!
//! ## Usage Example
//!
//! ```rust,no_run
//! use backflow::output::chuniio_proxy::{ChuniioProxyServer, create_chuniio_channels};
//! use backflow::protos::chuniio::{ChuniInputEvent, ChuniFeedbackEvent};
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     // Create channels for input/output communication
//!     let (input_tx, mut input_rx, feedback_tx, _feedback_rx) = create_chuniio_channels();
//!
//!     // Create proxy server
//!     let mut server = ChuniioProxyServer::new(
//!         Some(PathBuf::from("/tmp/chuniio.sock")),
//!         input_tx.clone(),
//!         feedback_tx.clone(),
//!     );
//!
//!     // Start server in background task
//!     tokio::spawn(async move {
//!         if let Err(e) = server.start().await {
//!             eprintln!("Server error: {}", e);
//!         }
//!     });
//!
//!     // Handle input events from clients
//!     while let Some(event) = input_rx.recv().await {
//!         match event {
//!             ChuniInputEvent::CoinInsert => println!("Coin inserted!"),
//!             ChuniInputEvent::OperatorButton { button, pressed } => {
//!                 println!("Button {} {}", button, if pressed { "pressed" } else { "released" });
//!             }
//!             ChuniInputEvent::SliderTouch { region, pressure } => {
//!                 println!("Slider region {} touched with pressure {}", region, pressure);
//!             }
//!             ChuniInputEvent::IrBeam { beam, broken } => {
//!                 println!("IR beam {} {}", beam, if broken { "broken" } else { "restored" });
//!             }
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Protocol
//!
//! The chuniio proxy uses a binary protocol with the following message types:
//!
//! - `JvsPoll` (0x01) - Request current JVS state from proxy
//! - `JvsPollResponse` (0x02) - Response with operator buttons and IR beam state  
//! - `CoinCounterRead` (0x03) - Request current coin count
//! - `CoinCounterReadResponse` (0x04) - Response with coin count
//! - `SliderInput` (0x05) - Slider pressure data (32 bytes, one per region)
//! - `SliderLedUpdate` (0x06) - Update slider LEDs (RGB data)
//! - `LedUpdate` (0x07) - Update billboard/air tower LEDs
//! - `Ping` (0x08) - Keepalive ping
//! - `Pong` (0x09) - Keepalive response
//!
//! All multi-byte integers are transmitted in little-endian format.

use crate::feedback::FeedbackEventStream;
use crate::feedback::generators::chuni_jvs::{ChuniLedDataPacket, LedBoardData, Rgb};
use crate::input::{
    InputEvent, InputEventPacket, InputEventReceiver, InputEventStream, KeyboardEvent,
};
use crate::output::OutputBackend;
use crate::protos::chuniio::{
    CHUNI_IO_OPBTN_SERVICE, CHUNI_IO_OPBTN_TEST, ChuniFeedbackEvent, ChuniInputEvent, ChuniMessage,
    ChuniProtocolState,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, trace, warn};

/// Default socket path for chuniio proxy
const DEFAULT_SOCKET_PATH: &str = "/tmp/backflow_chuniio.sock";

/// Chuniio proxy server that handles bidirectional communication
pub struct ChuniioProxyServer {
    socket_path: PathBuf,
    protocol_state: Arc<RwLock<ChuniProtocolState>>,
    next_client_id: Arc<RwLock<u64>>,
    input_receiver: InputEventReceiver,
    feedback_stream: FeedbackEventStream,
    led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
}

impl ChuniioProxyServer {
    /// Create a new chuniio proxy server
    pub fn new(
        socket_path: Option<PathBuf>,
        input_stream: InputEventStream,
        feedback_stream: FeedbackEventStream,
        led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
    ) -> Self {
        let socket_path = socket_path.unwrap_or_else(|| {
            // Try to use user runtime directory, fallback to /tmp
            let uid = nix::unistd::Uid::effective().as_raw();
            let runtime_dir = format!("/run/user/{}", uid);
            let runtime_path = format!("{}/backflow_chuniio.sock", runtime_dir);
            if std::path::Path::new(&runtime_dir).exists() {
                PathBuf::from(runtime_path)
            } else {
                PathBuf::from(DEFAULT_SOCKET_PATH)
            }
        });

        Self {
            socket_path,
            protocol_state: Arc::new(RwLock::new(ChuniProtocolState::new())),
            next_client_id: Arc::new(RwLock::new(0)),
            input_receiver: input_stream.subscribe(),
            feedback_stream,
            led_packet_tx,
        }
    }
}

impl OutputBackend for ChuniioProxyServer {
    async fn run(&mut self) -> eyre::Result<()> {
        tracing::info!("Starting chuniio proxy output backend");

        // Create channels for internal communication with larger buffers for better throughput
        let (input_tx, mut input_rx) = mpsc::channel::<ChuniInputEvent>(1000);
        let (feedback_tx, mut feedback_rx) = mpsc::channel::<ChuniFeedbackEvent>(1000);

        // Start the socket server in a background task
        let socket_path = self.socket_path.clone();
        let protocol_state = Arc::clone(&self.protocol_state);
        let next_client_id = Arc::clone(&self.next_client_id);
        let input_tx_clone = input_tx.clone();
        let feedback_tx_clone = feedback_tx.clone();
        let led_packet_tx_clone = self.led_packet_tx.clone();

        let server_handle = tokio::spawn(async move {
            let mut server = InternalChuniioProxyServer::new(
                socket_path,
                protocol_state,
                next_client_id,
                input_tx_clone,
                feedback_tx_clone,
                led_packet_tx_clone,
            );
            if let Err(e) = server.start_server().await {
                tracing::error!("Chuniio proxy server error: {}", e);
            }
        });

        // Start feedback handler task
        let feedback_handle = tokio::spawn(async move {
            while let Some(feedback_event) = feedback_rx.recv().await {
                // Convert feedback to chuniio events and forward to feedback stream
                match feedback_event {
                    ChuniFeedbackEvent::SliderLeds { rgb_data } => {
                        // Convert to feedback stream format if needed
                        // For now, just log it
                        tracing::debug!("Received slider LED feedback: {} bytes", rgb_data.len());
                    }
                    ChuniFeedbackEvent::LedBoard { board, rgb_data } => {
                        tracing::debug!(
                            "Received LED board {} feedback: {} bytes",
                            board,
                            rgb_data.len()
                        );
                    }
                }
            }
        });

        // Main loop: convert input events to chuniio events
        loop {
            tokio::select! {
                // Handle input events from the application
                packet = self.input_receiver.receive() => {
                    match packet {
                        Some(packet) => {
                            if let Err(e) = self.process_input_packet(packet, &input_tx).await {
                                tracing::error!("Failed to process input packet: {}", e);
                            }
                        }
                        None => {
                            tracing::info!("Input stream closed, stopping chuniio proxy backend");
                            break;
                        }
                    }
                }

                // Handle chuniio input events from clients
                chuni_event = input_rx.recv() => {
                    match chuni_event {
                        Some(event) => {
                            trace!("Processing chuniio event in proxy: {:?}", event);
                            // Update internal state - use try_write for non-blocking
                            if let Ok(mut state) = self.protocol_state.try_write() {
                                state.process_input_event(event);
                                trace!("Updated proxy state - opbtn: {}, beams: {}, coins: {}",
                                      state.jvs_state.opbtn, state.jvs_state.beams, state.coin_counter);
                            } else {
                                // If we can't get the lock immediately, skip state update to avoid blocking
                                warn!("Could not acquire state lock, skipping state update for chuniio event");
                            }
                        }
                        None => {
                            tracing::debug!("Chuniio input channel closed");
                        }
                    }
                }
            }
        }

        // Cleanup
        server_handle.abort();
        feedback_handle.abort();

        Ok(())
    }

    async fn stop(&mut self) -> eyre::Result<()> {
        tracing::info!("Stopping chuniio proxy output backend");
        Ok(())
    }
}

impl ChuniioProxyServer {
    /// Create internal server with channels (used by the OutputBackend implementation)
    fn new_internal(
        socket_path: PathBuf,
        input_tx: mpsc::Sender<ChuniInputEvent>,
        feedback_tx: mpsc::Sender<ChuniFeedbackEvent>,
        protocol_state: Arc<RwLock<ChuniProtocolState>>,
        next_client_id: Arc<RwLock<u64>>,
        led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
    ) -> InternalChuniioProxyServer {
        InternalChuniioProxyServer {
            socket_path,
            protocol_state,
            next_client_id,
            input_tx,
            feedback_tx,
            led_packet_tx,
        }
    }

    /// Process input packets from the application and convert to chuniio events
    async fn process_input_packet(
        &mut self,
        packet: InputEventPacket,
        input_tx: &mpsc::Sender<ChuniInputEvent>,
    ) -> eyre::Result<()> {
        trace!(
            "Processing input packet from device {}: {} events",
            packet.device_id,
            packet.events.len()
        );
        for event in packet.events {
            match &event {
                InputEvent::Keyboard(KeyboardEvent::KeyPress { key })
                | InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }) => {
                    // Only process CHUNIIO_ prefixed keys in chuniio backend
                    if !key.starts_with("CHUNIIO_") {
                        trace!("Skipping non-CHUNIIO key in chuniio backend: {}", key);
                        continue;
                    }
                    let pressed =
                        matches!(event, InputEvent::Keyboard(KeyboardEvent::KeyPress { .. }));
                    trace!(
                        "Processing key {}: {}",
                        if pressed { "press" } else { "release" },
                        key
                    );
                    if let Some(chuni_event) = self.parse_chuniio_event(key, pressed).await {
                        trace!("Sending chuniio event to proxy: {:?}", chuni_event);
                        if let Err(e) = input_tx.try_send(chuni_event) {
                            warn!(
                                "Failed to send chuniio input event ({}): {}",
                                if pressed { "key press" } else { "key release" },
                                e
                            );
                        }
                    }
                }

                InputEvent::Analog(analog_event) => {
                    // Only process CHUNIIO_SLIDER_ analog keys in chuniio backend
                    if let Some(key) = analog_event.keycode.strip_prefix("CHUNIIO_SLIDER_") {
                        if let Ok(region) = key.parse::<u8>() {
                            if region < 32 {
                                let pressure = analog_event.value as u8;
                                trace!(
                                    "Processing analog slider region {} with pressure {}",
                                    region,
                                    pressure
                                );
                                let chuni_event = ChuniInputEvent::SliderTouch { region, pressure };
                                if let Err(e) = input_tx.try_send(chuni_event) {
                                    warn!(
                                        "Failed to send chuniio analog slider event (region {}): {}",
                                        region, e
                                    );
                                }
                            }
                        }
                    } else {
                        trace!("Skipping non-CHUNIIO analog event in chuniio backend: {:?}", analog_event);
                    }
                }
                // Handle other input types as needed
                _ => {
                    trace!("Ignoring non-keyboard event: {:?}", event);
                }
            }
        }
        Ok(())
    }

    /// Convert keyboard events to chuniio events
    async fn parse_chuniio_event(&mut self, key: &str, pressed: bool) -> Option<ChuniInputEvent> {
        trace!("Parsing event: {} -> {}", key, pressed);
        let event = if key == "CHUNIIO_COIN" {
            Some(ChuniInputEvent::CoinInsert)
        } else if key == "CHUNIIO_TEST" {
            Some(ChuniInputEvent::OperatorButton {
                button: CHUNI_IO_OPBTN_TEST,
                pressed,
            })
        } else if key == "CHUNIIO_SERVICE" {
            Some(ChuniInputEvent::OperatorButton {
                button: CHUNI_IO_OPBTN_SERVICE,
                pressed,
            })
        } else if let Some(region_str) = key.strip_prefix("CHUNIIO_SLIDER_") {
            region_str
                .parse::<u8>()
                .ok()
                .filter(|&region| region < 32)
                .map(|region| ChuniInputEvent::SliderTouch {
                    region,
                    pressure: if pressed { 255 } else { 0 },
                })
        } else if let Some(beam_str) = key.strip_prefix("CHUNIIO_IR_") {
            beam_str
                .parse::<u8>()
                .ok()
                .filter(|&beam| beam < 6)
                .map(|beam| ChuniInputEvent::IrBeam {
                    beam,
                    broken: pressed,
                })
        } else {
            None
        };

        if let Some(ref evt) = event {
            trace!("Generated chuniio event: {:?}", evt);
        } else {
            warn!("No chuniio event generated for key: {}", key);
        }

        event
    }
}

/// Internal server structure that matches the original implementation
struct InternalChuniioProxyServer {
    socket_path: PathBuf,
    protocol_state: Arc<RwLock<ChuniProtocolState>>,
    next_client_id: Arc<RwLock<u64>>,
    input_tx: mpsc::Sender<ChuniInputEvent>,
    feedback_tx: mpsc::Sender<ChuniFeedbackEvent>,
    led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
}

impl InternalChuniioProxyServer {
    /// Create internal server instance
    fn new(
        socket_path: PathBuf,
        protocol_state: Arc<RwLock<ChuniProtocolState>>,
        next_client_id: Arc<RwLock<u64>>,
        input_tx: mpsc::Sender<ChuniInputEvent>,
        feedback_tx: mpsc::Sender<ChuniFeedbackEvent>,
        led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
    ) -> Self {
        Self {
            socket_path,
            protocol_state,
            next_client_id,
            input_tx,
            feedback_tx,
            led_packet_tx,
        }
    }

    /// Start the internal server
    async fn start_server(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Remove existing socket file if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Create the socket listener
        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Chuniio proxy server listening on {:?}", self.socket_path);

        // Make socket accessible to everyone (for compatibility with Windows DLL)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.socket_path)?.permissions();
            perms.set_mode(0o666);
            std::fs::set_permissions(&self.socket_path, perms)?;
        }

        // Accept client connections
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let client_id = {
                        let mut next_id = self.next_client_id.write().await;
                        let id = *next_id;
                        *next_id += 1;
                        id
                    };

                    info!("New chuniio client connected: {}", client_id);

                    // Spawn client handler
                    let protocol_state = Arc::clone(&self.protocol_state);
                    let input_tx = self.input_tx.clone();
                    let feedback_tx = self.feedback_tx.clone();
                    let led_packet_tx = self.led_packet_tx.clone();

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_client(
                            client_id,
                            stream,
                            protocol_state,
                            input_tx,
                            feedback_tx,
                            led_packet_tx,
                        )
                        .await
                        {
                            warn!("Client {} error: {}", client_id, e);
                        }
                        info!("Client {} disconnected", client_id);
                    });
                }
                Err(e) => {
                    error!("Failed to accept client connection: {}", e);
                }
            }
        }
    }

    /// Handle a single client connection with bidirectional communication
    async fn handle_client(
        client_id: u64,
        stream: UnixStream,
        protocol_state: Arc<RwLock<ChuniProtocolState>>,
        input_tx: mpsc::Sender<ChuniInputEvent>,
        _feedback_tx: mpsc::Sender<ChuniFeedbackEvent>,
        led_packet_tx: mpsc::Sender<ChuniLedDataPacket>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "Client {} connected and starting packet processing",
            client_id
        );

        let (reader, writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let writer = Arc::new(tokio::sync::Mutex::new(writer));

        // Channel for receiving feedback events to send to this client
        let (_client_feedback_tx, mut client_feedback_rx) =
            mpsc::channel::<ChuniFeedbackEvent>(100);

        // Spawn task to handle outgoing feedback messages
        let writer_clone = Arc::clone(&writer);
        let feedback_task = tokio::spawn(async move {
            while let Some(feedback_event) = client_feedback_rx.recv().await {
                info!(
                    "Client {} received feedback event: {:?}",
                    client_id, feedback_event
                );
                if let Some(message) = Self::feedback_event_to_message(feedback_event) {
                    info!(
                        "Client {} sending feedback message: {:?}",
                        client_id, message
                    );
                    let mut writer_guard = writer_clone.lock().await;
                    if let Err(e) = message.write_to(&mut *writer_guard).await {
                        warn!("Failed to send feedback to client {}: {}", client_id, e);
                        break;
                    } else {
                        info!("Client {} feedback message sent successfully", client_id);
                    }
                } else {
                    warn!(
                        "Client {} could not convert feedback event to message",
                        client_id
                    );
                }
            }
        });

        // Handle incoming messages from client
        loop {
            match ChuniMessage::from_reader(&mut reader).await {
                Ok(message) => {
                    // Log all incoming packets except JvsPoll and CoinCounterRead for debugging
                    // match &message {
                    //     ChuniMessage::JvsPoll | ChuniMessage::CoinCounterRead => {}
                    //     _ => info!("Client {} received packet: {:?}", client_id, message),
                    // }

                    match Self::handle_client_message(
                        client_id,
                        message,
                        &protocol_state,
                        &input_tx,
                        &_feedback_tx,
                        &led_packet_tx,
                    )
                    .await
                    {
                        Ok(Some(response)) => {
                            // Log outgoing response
                            // info!("Client {} sending response: {:?}", client_id, response);

                            // Send response back to client
                            let mut writer_guard = writer.lock().await;
                            if let Err(e) = response.write_to(&mut *writer_guard).await {
                                warn!("Failed to send response to client {}: {}", client_id, e);
                                break;
                            } else {
                                // info!("Client {} response sent successfully", client_id);
                            }
                        }
                        Ok(None) => {
                            // No response needed
                            // info!("Client {} no response needed for packet", client_id);
                        }
                        Err(e) => {
                            warn!("Error handling message from client {}: {}", client_id, e);
                        }
                    }
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        info!("Client {} disconnected cleanly", client_id);
                    } else {
                        warn!("Error reading from client {}: {}", client_id, e);
                    }
                    break;
                }
            }
        }

        info!("Client {} packet processing ended, cleaning up", client_id);
        feedback_task.abort();
        Ok(())
    }

    /// Handle a message from a client and optionally return a response
    async fn handle_client_message(
        client_id: u64,
        message: ChuniMessage,
        protocol_state: &Arc<RwLock<ChuniProtocolState>>,
        input_tx: &mpsc::Sender<ChuniInputEvent>,
        _feedback_tx: &mpsc::Sender<ChuniFeedbackEvent>,
        led_packet_tx: &mpsc::Sender<ChuniLedDataPacket>,
    ) -> Result<Option<ChuniMessage>, Box<dyn std::error::Error + Send + Sync>> {
        match message {
            ChuniMessage::JvsPoll => {
                // Return current JVS state - use try_read for non-blocking access
                let response = if let Ok(state) = protocol_state.try_read() {
                    ChuniMessage::JvsPollResponse {
                        opbtn: state.jvs_state.opbtn,
                        beams: state.jvs_state.beams,
                    }
                } else {
                    // If we can't get the lock, return default state to avoid blocking
                    ChuniMessage::JvsPollResponse {
                        opbtn: 0,
                        beams: 0x3F, // All beams clear by default
                    }
                };
                Ok(Some(response))
            }
            ChuniMessage::CoinCounterRead => {
                // Return current coin count - use try_read for non-blocking access
                let response = if let Ok(state) = protocol_state.try_read() {
                    ChuniMessage::CoinCounterReadResponse {
                        count: state.coin_counter,
                    }
                } else {
                    // If we can't get the lock, return 0 to avoid blocking
                    ChuniMessage::CoinCounterReadResponse { count: 0 }
                };
                Ok(Some(response))
            }
            ChuniMessage::SliderStateRead => {
                // Return current slider state - use try_read for non-blocking access
                let response = if let Ok(state) = protocol_state.try_read() {
                    ChuniMessage::SliderStateReadResponse {
                        pressure: state.slider_state.pressure,
                    }
                } else {
                    // If we can't get the lock, return empty pressure array
                    ChuniMessage::SliderStateReadResponse { pressure: [0; 32] }
                };
                Ok(Some(response))
            }
            ChuniMessage::SliderInput { pressure } => {
                // Update slider state and send input events
                info!("Client {} slider input: pressure={:?}", client_id, pressure);
                // Use try_write for non-blocking state update
                if let Ok(mut state) = protocol_state.try_write() {
                    state.slider_state.pressure = pressure;
                } else {
                    warn!("Could not acquire state lock for slider input update");
                }

                // Send slider touch events for changed regions
                for (region, &pressure_value) in pressure.iter().enumerate() {
                    if pressure_value > 0 {
                        if let Err(e) = input_tx.try_send(ChuniInputEvent::SliderTouch {
                            region: region as u8,
                            pressure: pressure_value,
                        }) {
                            warn!("Failed to send slider touch event: {}", e);
                        }
                    }
                }
                Ok(Some(ChuniMessage::Pong)) // Send immediate ACK to prevent game from waiting
            }
            ChuniMessage::SliderLedUpdate { rgb_data } => {
                // Fast path for slider LED updates - minimal processing
                let board = 2;
                let mut led_blocks: Vec<&[u8]> = rgb_data.chunks(3).collect();
                led_blocks.reverse();

                // Pre-allocate RGB array for better performance
                let mut rgb_array = [Rgb { r: 0, g: 0, b: 0 }; 31];
                for (i, brg_chunk) in led_blocks.iter().enumerate().take(31) {
                    if brg_chunk.len() == 3 {
                        rgb_array[i] = Rgb {
                            r: brg_chunk[1],
                            g: brg_chunk[2],
                            b: brg_chunk[0],
                        };
                    }
                }

                let packet = ChuniLedDataPacket {
                    board,
                    data: LedBoardData::Slider(rgb_array),
                };

                // Use try_send for non-blocking LED packet transmission
                if let Err(e) = led_packet_tx.try_send(packet) {
                    // Only warn on full channel, not other errors to reduce log spam
                    if matches!(e, mpsc::error::TrySendError::Full(_)) {
                        trace!("LED packet channel full, dropping frame");
                    } else {
                        warn!("Failed to send slider LED packet: {}", e);
                    }
                }

                // Immediate ACK - no waiting
                Ok(Some(ChuniMessage::Pong))
            }
            ChuniMessage::LedUpdate { board, rgb_data } => {
                // Fast path for LED updates - only process board 2 (slider)
                if board != 2 {
                    return Ok(Some(ChuniMessage::Pong));
                }

                let mut led_blocks: Vec<&[u8]> = rgb_data.chunks(3).collect();
                led_blocks.reverse(); // Only reverse for slider

                // Pre-allocate RGB array for better performance
                let mut rgb_array = [Rgb { r: 0, g: 0, b: 0 }; 31];
                for (i, brg_chunk) in led_blocks.iter().enumerate().take(31) {
                    if brg_chunk.len() == 3 {
                        rgb_array[i] = Rgb {
                            r: brg_chunk[1],
                            g: brg_chunk[2],
                            b: brg_chunk[0],
                        };
                    }
                }

                let packet = ChuniLedDataPacket {
                    board,
                    data: LedBoardData::Slider(rgb_array),
                };

                // Use try_send for non-blocking LED packet transmission
                if let Err(e) = led_packet_tx.try_send(packet) {
                    // Only warn on full channel, not other errors to reduce log spam
                    if matches!(e, mpsc::error::TrySendError::Full(_)) {
                        trace!("LED packet channel full, dropping frame");
                    } else {
                        warn!("Failed to send LED board {} packet: {}", board, e);
                    }
                }

                // Immediate ACK - no waiting
                Ok(Some(ChuniMessage::Pong))
            }
            ChuniMessage::Ping => {
                info!("Client {} ping received, sending pong", client_id);
                Ok(Some(ChuniMessage::Pong))
            }
            ChuniMessage::Pong => {
                info!("Client {} pong received", client_id);
                Ok(None)
            }
            _ => {
                info!(
                    "Client {} received unhandled message: {:?}",
                    client_id, message
                );
                // Other messages are responses or feedback, not handled here
                Ok(None)
            }
        }
    }

    /// Convert feedback event to chuniio message
    fn feedback_event_to_message(event: ChuniFeedbackEvent) -> Option<ChuniMessage> {
        match event {
            ChuniFeedbackEvent::SliderLeds { rgb_data } => {
                Some(ChuniMessage::SliderLedUpdate { rgb_data })
            }
            ChuniFeedbackEvent::LedBoard { board, rgb_data } => {
                Some(ChuniMessage::LedUpdate { board, rgb_data })
            }
        }
    }

    /// Send feedback event to all connected clients
    pub async fn send_feedback(
        &self,
        event: ChuniFeedbackEvent,
    ) -> Result<(), mpsc::error::TrySendError<ChuniFeedbackEvent>> {
        self.feedback_tx.try_send(event)
    }

    /// Send input event (for external use)
    pub async fn send_input(
        &self,
        event: ChuniInputEvent,
    ) -> Result<(), mpsc::error::TrySendError<ChuniInputEvent>> {
        // Update internal state - use try_write for non-blocking
        if let Ok(mut state) = self.protocol_state.try_write() {
            state.process_input_event(event.clone());
        } else {
            // If we can't get the lock immediately, skip state update to avoid blocking
            warn!("Could not acquire state lock for input event processing");
        }

        // Forward to input handler
        self.input_tx.try_send(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_creation() {
        let input_stream = InputEventStream::new();
        let feedback_stream = FeedbackEventStream::new();
        let (led_packet_tx, _led_packet_rx) = mpsc::channel::<ChuniLedDataPacket>(16);
        let server = ChuniioProxyServer::new(
            Some(PathBuf::from("/tmp/test_chuniio.sock")),
            input_stream,
            feedback_stream,
            led_packet_tx,
        );

        assert!(
            server
                .socket_path
                .to_str()
                .unwrap()
                .contains("test_chuniio.sock")
        );
    }

    #[tokio::test]
    async fn test_feedback_message_conversion() {
        let slider_event = ChuniFeedbackEvent::SliderLeds {
            rgb_data: vec![255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        let message = InternalChuniioProxyServer::feedback_event_to_message(slider_event);

        assert!(matches!(
            message,
            Some(ChuniMessage::SliderLedUpdate { .. })
        ));
    }
}
