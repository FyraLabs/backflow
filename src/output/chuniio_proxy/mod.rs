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
use crate::input::brokenithm::{BrokenithmInputState, get_brokenithm_state};
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
use tracing::{error, info, trace, warn};

/// Default socket path for chuniio proxy
const DEFAULT_SOCKET_PATH: &str = "/tmp/backflow_chuniio.sock";

/// Check if Brokenithm state has changed compared to previous state
fn brokenithm_state_changed(
    current: &BrokenithmInputState,
    previous: &Option<BrokenithmInputState>,
) -> bool {
    match previous {
        None => true,                  // First time, always apply
        Some(prev) => current != prev, // Use PartialEq for comparison
    }
}

/// Convert Brokenithm state to chuniio protocol state
fn apply_brokenithm_state_to_chuniio(
    chuniio_state: &mut ChuniProtocolState,
    brokenithm_state: &BrokenithmInputState,
    last_coin_pulse: &mut bool,
) {
    // Update operator buttons
    let mut opbtn = 0;
    if brokenithm_state.test_button {
        opbtn |= CHUNI_IO_OPBTN_TEST;
    }
    if brokenithm_state.service_button {
        opbtn |= CHUNI_IO_OPBTN_SERVICE;
    }
    chuniio_state.jvs_state.opbtn = opbtn;

    // Update IR beams (air sensors) - invert logic: air sensor active = beam broken
    let mut beams = 0x3F; // All beams clear by default
    for (i, &air_value) in brokenithm_state.air.iter().enumerate() {
        if air_value > 0 && i < 6 {
            beams &= !(1 << i); // Break beam when air sensor is active
        }
    }
    chuniio_state.jvs_state.beams = beams;

    // Update slider pressure - directly copy from Brokenithm state
    for (i, &pressure) in brokenithm_state.slider.iter().enumerate() {
        if i < 32 {
            chuniio_state.slider_state.pressure[i] = pressure;
        }
    }

    // Handle coin pulse - increment coin counter if coin was inserted (edge detection)
    if brokenithm_state.coin_pulse && !*last_coin_pulse {
        chuniio_state.coin_counter += 1;
        trace!(
            "Coin pulse detected from Brokenithm state, coin counter now: {}",
            chuniio_state.coin_counter
        );
    }
    *last_coin_pulse = brokenithm_state.coin_pulse;
}

/// Chuniio proxy server that handles bidirectional communication
pub struct ChuniioProxyServer {
    socket_path: PathBuf,
    protocol_state: Arc<RwLock<ChuniProtocolState>>,
    next_client_id: Arc<RwLock<u64>>,
    input_receiver: InputEventReceiver,
    feedback_stream: FeedbackEventStream,
    led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
    last_coin_pulse: bool, // Track coin pulse state for edge detection
    last_brokenithm_state: Option<BrokenithmInputState>, // Track last applied Brokenithm state for change detection
}

impl ChuniioProxyServer {
    /// Create a new chuniio proxy server
    pub fn new(
        socket_path: Option<PathBuf>,
        input_stream: InputEventStream,
        feedback_stream: FeedbackEventStream,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
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
            last_coin_pulse: false,
            last_brokenithm_state: None,
        }
    }
}

impl OutputBackend for ChuniioProxyServer {
    async fn run(&mut self) -> eyre::Result<()> {
        tracing::info!("Starting chuniio proxy output backend");

        // Create channels for internal communication with unbounded capacity for better throughput
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<ChuniInputEvent>();
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<ChuniFeedbackEvent>();

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

        // Create a polling timer for Brokenithm state (60 FPS for smooth updates)
        let mut poll_interval = tokio::time::interval(tokio::time::Duration::from_millis(16));
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Main loop: convert input events to chuniio events and poll Brokenithm state
        loop {
            tokio::select! {
                // Poll Brokenithm shared state periodically
                _ = poll_interval.tick() => {
                    if let Some(brokenithm_state) = get_brokenithm_state().await {
                        // Only apply state if it has changed to avoid redundant updates
                        if brokenithm_state_changed(&brokenithm_state, &self.last_brokenithm_state) {
                            let mut chuniio_state = self.protocol_state.write().await;
                            tracing::trace!(
                                "Brokenithm state changed, applying to chuniio state: air={:?}, slider={:?}, test={}, service={}, coin_pulse={}",
                                brokenithm_state.air,
                                brokenithm_state.slider,
                                brokenithm_state.test_button,
                                brokenithm_state.service_button,
                                brokenithm_state.coin_pulse
                            );
                            apply_brokenithm_state_to_chuniio(&mut chuniio_state, &brokenithm_state, &mut self.last_coin_pulse);

                            // Store the applied state for future comparison
                            self.last_brokenithm_state = Some(brokenithm_state);
                        } else {
                            // State hasn't changed, no need to apply
                            // tracing::trace!("Brokenithm state unchanged, skipping state application");
                        }
                    }
                }

                // Handle input events from the application (for non-Brokenithm devices)
                packet = self.input_receiver.receive() => {
                    // Skip processing input events if Brokenithm shared state is available
                    // to avoid interference between polling-based and event-based state updates
                    if get_brokenithm_state().await.is_some() {
                        tracing::trace!("Skipping input packet processing - Brokenithm shared state is active");
                        continue;
                    }

                    tracing::trace!("Received input packet from application");
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

                // Handle chuniio input events from clients (legacy event-based path)
                chuni_event = input_rx.recv() => {
                    match chuni_event {
                        Some(event) => {
                            // Skip processing chuniio events if Brokenithm shared state is available
                            // to avoid interference with polling-based state updates
                            if get_brokenithm_state().await.is_some() {
                                tracing::trace!("Skipping chuniio event processing - Brokenithm shared state is active: {:?}", event);
                                continue;
                            }

                            trace!("Processing chuniio event in proxy: {:?}", event);
                            // Update internal state - use blocking write to ensure all events are processed
                            let mut state = self.protocol_state.write().await;
                            state.process_input_event(event);
                            trace!("Updated proxy state - opbtn: {}, beams: {}, coins: {}\nslider: {:?}",
                                  state.jvs_state.opbtn, state.jvs_state.beams, state.coin_counter, state.slider_state.pressure);
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
        input_tx: mpsc::UnboundedSender<ChuniInputEvent>,
        feedback_tx: mpsc::UnboundedSender<ChuniFeedbackEvent>,
        protocol_state: Arc<RwLock<ChuniProtocolState>>,
        next_client_id: Arc<RwLock<u64>>,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
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
        input_tx: &mpsc::UnboundedSender<ChuniInputEvent>,
    ) -> eyre::Result<()> {
        trace!(
            "Processing input packet from device {}: {} events",
            packet.device_id,
            packet.events.len()
        );

        // Since events are pre-routed, all events should be CHUNIIO events.
        // We can process them as a batch if there are many, or individually.
        if packet.events.len() > 1 {
            trace!(
                "Processing as atomic batch: {} events",
                packet.events.len()
            );
            return self.process_input_packet_batch(packet).await;
        }

        // Process events individually
        for event in &packet.events {
            match &event {
                InputEvent::Keyboard(KeyboardEvent::KeyPress { key })
                | InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }) => {
                    let pressed =
                        matches!(event, InputEvent::Keyboard(KeyboardEvent::KeyPress { .. }));
                    if let Some(chuni_event) = self.parse_chuniio_event(key, pressed).await {
                        // Also update our own state when processing individual events
                        let mut state = self.protocol_state.write().await;
                        state.process_input_event(chuni_event.clone());

                        if let Err(e) = input_tx.send(chuni_event) {
                            warn!(
                                "Failed to send chuniio input event ({}): {}. Event dropped! Key: {} Pressed: {}",
                                if pressed { "key press" } else { "key release" },
                                e,
                                key,
                                pressed
                            );
                        }
                    }
                }
                InputEvent::Analog(analog_event) => {
                    // All analog events are assumed to be CHUNIIO_SLIDER events
                    if let Some(key) = analog_event.keycode.strip_prefix("CHUNIIO_SLIDER_") {
                        if let Ok(region) = key.parse::<u8>() {
                            if region < 32 {
                                let pressure = analog_event.value as u8;
                                let chuni_event = ChuniInputEvent::SliderTouch { region, pressure };
                                if let Err(e) = input_tx.send(chuni_event) {
                                    warn!(
                                        "Failed to send chuniio analog slider event (region {}): {}. Event dropped!",
                                        region, e
                                    );
                                }
                            }
                        }
                    }
                }
                // Handle other input types as needed
                _ => {
                    trace!("Ignoring non-keyboard/analog event: {:?}", event);
                }
            }
        }
        Ok(())
    }

    /// Process input packet as an atomic batch (for complete state updates from web interface)
    async fn process_input_packet_batch(&mut self, packet: InputEventPacket) -> eyre::Result<()> {
        trace!(
            "Processing batch packet with {} events atomically",
            packet.events.len()
        );

        // Extract current state and apply all changes atomically
        let mut state = self.protocol_state.write().await;

        // Apply all events in the batch
        for event in &packet.events {
            match &event {
                InputEvent::Keyboard(KeyboardEvent::KeyPress { key })
                | InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }) => {
                    let pressed =
                        matches!(event, InputEvent::Keyboard(KeyboardEvent::KeyPress { .. }));

                    // Apply state changes directly without sending individual events
                    if key == "CHUNIIO_COIN" {
                        if pressed {
                            state.coin_counter += 1;
                            trace!("Batch: Coin inserted, counter now: {}", state.coin_counter);
                        }
                    } else if key == "CHUNIIO_TEST" {
                        if pressed {
                            state.jvs_state.opbtn |= CHUNI_IO_OPBTN_TEST;
                        } else {
                            state.jvs_state.opbtn &= !CHUNI_IO_OPBTN_TEST;
                        }
                        trace!(
                            "Batch: Test button {}",
                            if pressed { "pressed" } else { "released" }
                        );
                    } else if key == "CHUNIIO_SERVICE" {
                        if pressed {
                            state.jvs_state.opbtn |= CHUNI_IO_OPBTN_SERVICE;
                        } else {
                            state.jvs_state.opbtn &= !CHUNI_IO_OPBTN_SERVICE;
                        }
                        trace!(
                            "Batch: Service button {}",
                            if pressed { "pressed" } else { "released" }
                        );
                    } else if let Some(region_str) = key.strip_prefix("CHUNIIO_SLIDER_") {
                        if let Ok(region) = region_str.parse::<u8>() {
                            if region < 32 {
                                let pressure = if pressed { 255 } else { 0 };
                                state.slider_state.pressure[region as usize] = pressure;
                                trace!("Batch: Slider region {} pressure {}", region, pressure);
                            }
                        }
                    } else if let Some(beam_str) = key.strip_prefix("CHUNIIO_IR_") {
                        if let Ok(beam) = beam_str.parse::<u8>() {
                            if beam < 6 {
                                if pressed {
                                    state.jvs_state.beams &= !(1 << beam); // Break beam
                                } else {
                                    state.jvs_state.beams |= 1 << beam; // Restore beam
                                }
                                trace!(
                                    "Batch: IR beam {} {}",
                                    beam,
                                    if pressed { "broken" } else { "restored" }
                                );
                            }
                        }
                    }
                }
                _ => {
                    // Ignore non-keyboard events in batch processing
                }
            }
        }

        let active_regions: Vec<usize> = state
            .slider_state
            .pressure
            .iter()
            .enumerate()
            .filter_map(|(i, &p)| if p > 0 { Some(i) } else { None })
            .collect();

        trace!(
            "Batch processing complete - opbtn: {}, beams: {:#06b}, slider active regions: {} ({:?})",
            state.jvs_state.opbtn,
            state.jvs_state.beams,
            active_regions.len(),
            active_regions
        );

        Ok(())
    }

    /// Convert keyboard events to chuniio events
    async fn parse_chuniio_event(&mut self, key: &str, pressed: bool) -> Option<ChuniInputEvent> {
        // trace!("Parsing event: {} -> {}", key, pressed);
        let event = if key == "CHUNIIO_COIN" {
            if pressed {
                Some(ChuniInputEvent::CoinInsert)
            } else {
                None // Coin release is not an event
            }
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
                .map(|region| {
                    let pressure = if pressed { 255 } else { 0 };
                    trace!(
                        "Generated slider event: region={}, pressure={}, pressed={}",
                        region, pressure, pressed
                    );
                    ChuniInputEvent::SliderTouch { region, pressure }
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
            warn!("Received non-CHUNIIO key in chuniio backend: {}", key);
            None
        };

        if let Some(ref _evt) = event {
            // trace!("Generated chuniio event: {:?}", evt);
        } else if !key.starts_with("CHUNIIO_") {
            // Only warn if it's not a CHUNIIO key, to avoid noise from coin release etc.
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
    input_tx: mpsc::UnboundedSender<ChuniInputEvent>,
    feedback_tx: mpsc::UnboundedSender<ChuniFeedbackEvent>,
    led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
}

impl InternalChuniioProxyServer {
    /// Create internal server instance
    fn new(
        socket_path: PathBuf,
        protocol_state: Arc<RwLock<ChuniProtocolState>>,
        next_client_id: Arc<RwLock<u64>>,
        input_tx: mpsc::UnboundedSender<ChuniInputEvent>,
        feedback_tx: mpsc::UnboundedSender<ChuniFeedbackEvent>,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
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
        input_tx: mpsc::UnboundedSender<ChuniInputEvent>,
        _feedback_tx: mpsc::UnboundedSender<ChuniFeedbackEvent>,
        led_packet_tx: mpsc::UnboundedSender<ChuniLedDataPacket>,
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
            mpsc::unbounded_channel::<ChuniFeedbackEvent>();

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
        input_tx: &mpsc::UnboundedSender<ChuniInputEvent>,
        _feedback_tx: &mpsc::UnboundedSender<ChuniFeedbackEvent>,
        led_packet_tx: &mpsc::UnboundedSender<ChuniLedDataPacket>,
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
                    ChuniMessage::SliderStateReadResponse { pressure: [0; 32] }
                };
                Ok(Some(response))
            }
            // --- Expose full IO state to native client ---
            ChuniMessage::JvsFullStateRead => {
                let response = if let Ok(state) = protocol_state.try_read() {
                    ChuniMessage::JvsFullStateReadResponse {
                        opbtn: state.jvs_state.opbtn,
                        beams: state.jvs_state.beams,
                        pressure: state.slider_state.pressure,
                        coin_counter: state.coin_counter,
                    }
                } else {
                    ChuniMessage::JvsFullStateReadResponse {
                        opbtn: 0,
                        beams: 0x3F,
                        pressure: [0; 32],
                        coin_counter: 0,
                    }
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
                        if let Err(e) = input_tx.send(ChuniInputEvent::SliderTouch {
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

                // Use send for unbounded LED packet transmission
                if let Err(e) = led_packet_tx.send(packet) {
                    warn!("Failed to send slider LED packet: {}", e);
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
                if let Err(e) = led_packet_tx.send(packet) {
                    warn!("Failed to send LED board {} packet: {}", board, e);
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
    ) -> Result<(), mpsc::error::SendError<ChuniFeedbackEvent>> {
        self.feedback_tx.send(event)
    }

    /// Send input event (for external use)
    pub async fn send_input(
        &self,
        event: ChuniInputEvent,
    ) -> Result<(), mpsc::error::SendError<ChuniInputEvent>> {
        // Update internal state - always block for lock
        let mut state = self.protocol_state.write().await;
        state.process_input_event(event.clone());

        // Forward to input handler
        self.input_tx.send(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_creation() {
        let input_stream = InputEventStream::new();
        let feedback_stream = FeedbackEventStream::new();
        let (led_packet_tx, _led_packet_rx) = mpsc::unbounded_channel::<ChuniLedDataPacket>();
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
