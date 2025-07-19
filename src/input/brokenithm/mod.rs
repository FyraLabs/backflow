//! A TCP server implementation specifically for Brokenithm clients
//!
//! Brokenithm is a UDP-based mobile virtual controller for CHUNITHM-style games.
//!
//! This module provides a TCP server/client that connects to Brokenithm clients,
//! allowing them to send input events and receive updates.

use crate::feedback::{FeedbackEvent, FeedbackEventPacket, LedEvent};
use crate::input::{InputBackend, InputEvent, InputEventPacket, InputEventStream, KeyboardEvent};
use crate::output::rgb_to_brg;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

pub mod idevice_proxy;
use idevice_proxy::IproxyManager;

/// Shared Brokenithm input state that can be polled by chuniio_proxy
#[derive(Debug, Clone, PartialEq)]
pub struct BrokenithmInputState {
    pub air: Vec<u8>,    // 6 air zones
    pub slider: Vec<u8>, // 32 slider zones
    pub test_button: bool,
    pub service_button: bool,
    pub coin_pulse: bool, // Coin insertion pulse
}

impl BrokenithmInputState {
    pub fn new() -> Self {
        Self {
            air: vec![0; 6],
            slider: vec![0; 32],
            test_button: false,
            service_button: false,
            coin_pulse: false,
        }
    }
}

/// Global shared state for Brokenithm input
static BROKENITHM_SHARED_STATE: OnceLock<RwLock<Option<BrokenithmInputState>>> = OnceLock::new();

/// Enable shared state tracking for Brokenithm
pub async fn enable_brokenithm_shared_state() {
    let state_lock =
        BROKENITHM_SHARED_STATE.get_or_init(|| RwLock::new(Some(BrokenithmInputState::new())));
    let mut state = state_lock.write().await;
    if state.is_none() {
        *state = Some(BrokenithmInputState::new());
    }
    tracing::info!("Brokenithm shared state enabled");
}

/// Disable shared state tracking for Brokenithm
pub async fn disable_brokenithm_shared_state() {
    if let Some(state_lock) = BROKENITHM_SHARED_STATE.get() {
        let mut state = state_lock.write().await;
        *state = None;
        tracing::info!("Brokenithm shared state disabled");
    }
}

/// Get current Brokenithm shared state (for polling by chuniio_proxy)
pub async fn get_brokenithm_state() -> Option<BrokenithmInputState> {
    if let Some(state_lock) = BROKENITHM_SHARED_STATE.get() {
        let state = state_lock.read().await;
        state.clone()
    } else {
        None
    }
}

/// Update Brokenithm shared state (called by input backends)
pub async fn set_brokenithm_state(new_state: BrokenithmInputState) {
    if let Some(state_lock) = BROKENITHM_SHARED_STATE.get() {
        let mut state = state_lock.write().await;
        if state.is_some() {
            *state = Some(new_state);
        }
    }
}

pub struct BrokenithmTcpBackend {
    pub config: BrokenithmTcpConfig,
    pub input_stream: InputEventStream,
}

#[derive(Debug, Clone)]
pub enum BrokenithmTcpConfig {
    /// Listen for incoming connections (server mode)
    Server { bind_addr: SocketAddr },
    /// Connect directly to a TCP address
    Client { connect_addr: SocketAddr },
    /// Connect via iproxy device forwarding
    DeviceProxy {
        local_port: u16,
        device_port: u16,
        udid: Option<String>,
    },
}

impl BrokenithmTcpBackend {
    pub fn new(config: BrokenithmTcpConfig, input_stream: InputEventStream) -> Self {
        Self {
            config,
            input_stream,
        }
    }

    pub fn server(bind_addr: SocketAddr, input_stream: InputEventStream) -> Self {
        Self::new(BrokenithmTcpConfig::Server { bind_addr }, input_stream)
    }

    pub fn client(connect_addr: SocketAddr, input_stream: InputEventStream) -> Self {
        Self::new(BrokenithmTcpConfig::Client { connect_addr }, input_stream)
    }

    pub fn device_proxy(
        local_port: u16,
        device_port: u16,
        udid: Option<String>,
        input_stream: InputEventStream,
    ) -> Self {
        Self::new(
            BrokenithmTcpConfig::DeviceProxy {
                local_port,
                device_port,
                udid,
            },
            input_stream,
        )
    }
}

fn led_feedback_to_cled(feedback: &FeedbackEventPacket) -> Vec<u8> {
    let mut leds = [[0u8, 0u8, 0u8]; 32];
    for event in &feedback.events {
        if let FeedbackEvent::Led(LedEvent::Set {
            led_id,
            rgb: Some((r, g, b)),
            ..
        }) = event
        {
            if (*led_id as usize) < 32 {
                leds[*led_id as usize] = [*r, *g, *b];
            }
        }
    }
    let mut led_msg = Vec::with_capacity(4 + 32 * 3);
    led_msg.extend_from_slice(&[99, 76, 69, 68]); // "cLED"
    for rgb in leds.iter() {
        led_msg.extend_from_slice(&rgb_to_brg(rgb));
    }
    led_msg
}

/// LED pattern: set only active slider zones to white, others off
fn led_slider_active_pattern(slider: &[u8]) -> Vec<u8> {
    let mut led_msg = Vec::with_capacity(4 + 32 * 3);
    led_msg.extend_from_slice(&[99, 76, 69, 68]); // "cLED"
    for i in 0..32 {
        // Offset index by 1, wrapping around to align LEDs with slider zones
        let idx = (i + 1) % 32;
        let val = slider.get(idx).copied().unwrap_or(0);
        if val >= 128 {
            led_msg.extend_from_slice(&rgb_to_brg(&[255, 255, 255])); // White for active
        } else {
            led_msg.extend_from_slice(&rgb_to_brg(&[0, 0, 0])); // Off for inactive
        }
    }
    led_msg
}

#[async_trait::async_trait]
impl InputBackend for BrokenithmTcpBackend {
    async fn run(&mut self) -> eyre::Result<()> {
        // Enable shared state for chuniio_proxy polling
        enable_brokenithm_shared_state().await;

        match self.config.clone() {
            BrokenithmTcpConfig::Server { bind_addr } => self.run_server(bind_addr).await,
            BrokenithmTcpConfig::Client { connect_addr } => self.run_client(connect_addr).await,
            BrokenithmTcpConfig::DeviceProxy {
                local_port,
                device_port,
                udid,
            } => {
                self.run_device_proxy(local_port, device_port, udid.as_deref())
                    .await
            }
        }
    }
}

impl BrokenithmTcpBackend {
    async fn run_server(&mut self, bind_addr: SocketAddr) -> eyre::Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        tracing::info!("Brokenithm TCP backend listening on {}", bind_addr);

        // Shared client list for feedback broadcast
        let clients: Arc<RwLock<Vec<Arc<tokio::sync::Mutex<tokio::net::TcpStream>>>>> =
            Arc::new(RwLock::new(Vec::new()));
        let feedback_stream = crate::feedback::FeedbackEventStream::default();
        let feedback_stream_clone = feedback_stream.clone();
        let clients_clone = clients.clone();

        // tokio::spawn(async move {
        //     loop {
        //         match feedback_stream_clone.receive().await {
        //             Some(feedback) => {
        //                 let led_msg = led_feedback_to_cled(&feedback);
        //                 let mut to_remove = Vec::new();
        //                 let clients_guard = clients_clone.read().await;
        //                 for (idx, client_mutex) in clients_guard.iter().enumerate() {
        //                     let mut client = client_mutex.lock().await;
        //                     if let Err(e) = client.write_all(&led_msg).await {
        //                         tracing::warn!("Failed to send feedback to client: {}", e);
        //                         to_remove.push(idx);
        //                     }
        //                 }
        //                 drop(clients_guard);
        //                 if !to_remove.is_empty() {
        //                     let mut clients_guard = clients_clone.write().await;
        //                     for &idx in to_remove.iter().rev() {
        //                         clients_guard.remove(idx);
        //                     }
        //                 }
        //             }
        //             None => {
        //                 tracing::info!("Feedback stream closed, stopping feedback broadcast task");
        //                 break;
        //             }
        //         }
        //     }
        // });

        // loop {
        //     let (socket, addr) = listener.accept().await?;
        //     tracing::info!("Accepted TCP connection from {}", addr);
        //     let client_mutex = Arc::new(tokio::sync::Mutex::new(socket));

        //     // Register this client for feedback
        //     {
        //         let mut clients_guard = clients.write().await;
        //         clients_guard.push(client_mutex.clone());
        //     }

        //     let clients_for_removal = clients.clone();
        //     let client_mutex_for_removal = client_mutex.clone();

        //     tokio::spawn(async move {
        //         Self::handle_connection(
        //             client_mutex,
        //             Some((clients_for_removal, client_mutex_for_removal)),
        //             Some(addr),
        //         )
        //         .await;
        //     });
        // }

        Ok(())
    }

    async fn run_client(&mut self, connect_addr: SocketAddr) -> eyre::Result<()> {
        // loop {
        //     match TcpStream::connect(connect_addr).await {
        //         Ok(socket) => {
        //             tracing::info!("Connected to Brokenithm device at {}", connect_addr);
        //             let socket = Arc::new(tokio::sync::Mutex::new(socket));
        //             Self::handle_connection(socket, None, None).await;
        //         }
        //         Err(e) => {
        //             tracing::error!(
        //                 "Failed to connect to {}: {}. Retrying in 1s...",
        //                 connect_addr,
        //                 e
        //             );
        //             tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        //         }
        //     }
        // }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    async fn run_device_proxy(
        &mut self,
        _local_port: u16,
        device_port: u16,
        udid: Option<&str>,
    ) -> eyre::Result<()> {
        // Connect directly to the device using IproxyManager instead of spawning iproxy CLI
        tracing::info!(
            "Connecting to iOS device port {} (UDID: {:?})",
            device_port,
            udid
        );

        loop {
            match IproxyManager::get_default_connection(device_port, "backflow-brokenithm").await {
                Ok(mut manager) => {
                    tracing::info!("Connected to iOS device: {:?}", manager.device);

                    // Take the idevice connection out of the manager
                    if let Some(idevice) = manager.take_idevice() {
                        tracing::info!(
                            "Successfully obtained device connection, starting Brokenithm protocol handler"
                        );

                        // Handle the idevice connection directly
                        Self::handle_idevice_connection(idevice).await;
                    } else {
                        tracing::error!("Failed to take idevice from manager");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect to iOS device: {}. Retrying in 1s...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn handle_connection(
        socket: Arc<tokio::sync::Mutex<TcpStream>>,
        client_cleanup: Option<(
            Arc<RwLock<Vec<Arc<tokio::sync::Mutex<tokio::net::TcpStream>>>>>,
            Arc<tokio::sync::Mutex<tokio::net::TcpStream>>,
        )>,
        addr: Option<SocketAddr>,
    ) {
        let socket_for_feedback = socket.clone();

        // Start feedback listening task
        let feedback_stream = crate::feedback::FeedbackEventStream::default();
        let feedback_stream_clone = feedback_stream.clone();
        let feedback_task = tokio::spawn(async move {
            loop {
                match feedback_stream_clone.receive().await {
                    Some(feedback) => {
                        let led_msg = led_feedback_to_cled(&feedback);
                        let mut socket_guard = socket_for_feedback.lock().await;
                        if let Err(e) = socket_guard.write_all(&led_msg).await {
                            tracing::warn!("Failed to send feedback: {}", e);
                            break;
                        }
                    }
                    None => {
                        tracing::info!("Feedback stream closed");
                        break;
                    }
                }
            }
        });

        let mut buffer = Vec::new();
        let mut read_buf = [0u8; 256];
        let mut state_tracker = BrokenithmInputStateTracker::new();

        loop {
            let mut socket_guard = socket.lock().await;
            match socket_guard.read(&mut read_buf).await {
                Ok(0) => {
                    if let Some(addr) = addr {
                        tracing::info!("Connection closed by {}", addr);
                    } else {
                        tracing::info!("Connection closed by remote");
                    }
                    feedback_task.abort();

                    // Remove this client from the shared list if it's a server connection
                    if let Some((clients_for_removal, client_mutex_for_removal)) = client_cleanup {
                        let mut clients_guard = clients_for_removal.write().await;
                        if let Some(pos) = clients_guard
                            .iter()
                            .position(|c| Arc::ptr_eq(c, &client_mutex_for_removal))
                        {
                            clients_guard.remove(pos);
                        }
                    }
                    break;
                }
                Ok(len) => {
                    // Release lock before processing
                    drop(socket_guard);

                    // Append new data to buffer
                    buffer.extend_from_slice(&read_buf[..len]);

                    // Extract and process all complete messages
                    let (messages, consumed) = extract_brokenithm_messages(&buffer);
                    for message in messages {
                        if addr.is_some() {
                            tracing::trace!("Parsed message from {:?}: {:?}", addr, message);
                        }

                        match message {
                            BrokenithmMessage::Input {
                                air,
                                slider,
                                test_button,
                                service_button,
                            } => {
                                if let Some(_packet) = state_tracker.diff_and_packet(
                                    &air,
                                    &slider,
                                    test_button,
                                    service_button,
                                ) {
                                    // Don't send input events when using shared state polling
                                    // let _ = input_stream.send(packet).await;
                                }
                                // Always update shared state regardless of event emission
                                state_tracker.update_shared_state(test_button, service_button);
                                // Send LED pattern for active sliders using raw values
                                let led_msg = led_slider_active_pattern(&slider);
                                let mut socket_guard = socket.lock().await;
                                let _ = socket_guard.write_all(&led_msg).await;
                            }
                            BrokenithmMessage::InsertCoin => {
                                state_tracker.pulse_coin();
                                let air = state_tracker.prev_air.clone();
                                let slider = state_tracker.prev_slider.clone();
                                let test_button = state_tracker.prev_test;
                                let service_button = state_tracker.prev_service;
                                if let Some(_packet) = state_tracker.diff_and_packet(
                                    &air,
                                    &slider,
                                    test_button,
                                    service_button,
                                ) {
                                    // Don't send input events when using shared state polling
                                    // let _ = input_stream.send(packet).await;
                                }
                                // Always update shared state after coin pulse
                                state_tracker.update_shared_state(test_button, service_button);
                            }
                            BrokenithmMessage::Welcome => {
                                // Don't send test pattern after WEL message
                                // Only slider-active pattern is sent after input updates
                            }
                            BrokenithmMessage::EnableAir(_)
                            | BrokenithmMessage::TapCard
                            | BrokenithmMessage::Unknown(_) => {
                                // Do nothing
                            }
                        }
                    }

                    // Remove consumed bytes from buffer
                    if consumed > 0 {
                        buffer.drain(..consumed);
                    }
                }
                Err(e) => {
                    tracing::error!("TCP read error: {}", e);
                    feedback_task.abort();
                    break;
                }
            }
        }
    }

    async fn handle_idevice_connection(idevice: idevice::Idevice) {
        // Start feedback listening task
        let feedback_stream = crate::feedback::FeedbackEventStream::default();
        let feedback_stream_clone = feedback_stream.clone();

        // Create a shared reference to the idevice for feedback writes
        let idevice_arc = Arc::new(tokio::sync::Mutex::new(idevice));
        let idevice_arc_feedback = idevice_arc.clone();

        let feedback_task = tokio::spawn(async move {
            loop {
                match feedback_stream_clone.receive().await {
                    Some(feedback) => {
                        // Skip LED feedback for now - idevice might not support writes in this mode
                        let led_msg = led_feedback_to_cled(&feedback);
                        let mut idevice_guard = idevice_arc_feedback.lock().await;
                        if let Err(e) = idevice_guard.send_raw(&led_msg).await {
                            tracing::warn!("Failed to send feedback to idevice: {}", e);
                            break;
                        }
                    }
                    None => {
                        tracing::info!("Feedback stream closed");
                        break;
                    }
                }
            }
        });

        let mut buffer = Vec::new();
        let mut state_tracker = BrokenithmInputStateTracker::new();

        loop {
            let mut idevice_guard = idevice_arc.lock().await;
            let read_result = idevice_guard.read_raw(256).await;
            drop(idevice_guard); // Always drop immediately after reading
            match read_result {
                Ok(data) if data.is_empty() => {
                    tracing::info!("iDevice connection closed");
                    feedback_task.abort();
                    break;
                }
                Ok(data) => {
                    // Append new data to buffer
                    buffer.extend_from_slice(&data);

                    // Extract and process all complete messages
                    let (messages, consumed) = extract_brokenithm_messages(&buffer);
                    for message in messages {
                        tracing::trace!("Parsed message from iDevice: {:?}", message);

                        match message {
                            BrokenithmMessage::Input {
                                air,
                                slider,
                                test_button,
                                service_button,
                            } => {
                                if let Some(_packet) = state_tracker.diff_and_packet(
                                    &air,
                                    &slider,
                                    test_button,
                                    service_button,
                                ) {
                                    // Don't send input events when using shared state polling
                                    // let _ = input_stream.send(packet).await;
                                }
                                // Always update shared state regardless of event emission
                                state_tracker.update_shared_state(test_button, service_button);
                                // Skip LED pattern for idevice - no write support in current implementation
                                let led_msg = led_slider_active_pattern(&slider);
                                let mut idevice_guard = idevice_arc.lock().await;
                                let _ = idevice_guard.send_raw(&led_msg).await;
                            }
                            BrokenithmMessage::InsertCoin => {
                                state_tracker.pulse_coin();
                                let air = state_tracker.prev_air.clone();
                                let slider = state_tracker.prev_slider.clone();
                                let test_button = state_tracker.prev_test;
                                let service_button = state_tracker.prev_service;
                                if let Some(_packet) = state_tracker.diff_and_packet(
                                    &air,
                                    &slider,
                                    test_button,
                                    service_button,
                                ) {
                                    // Don't send input events when using shared state polling
                                    // let _ = input_stream.send(packet).await;
                                }
                                // Always update shared state after coin pulse
                                state_tracker.update_shared_state(test_button, service_button);
                            }
                            BrokenithmMessage::Welcome => {
                                // Don't send test pattern after WEL message
                                // Only slider-active pattern is sent after input updates
                            }
                            BrokenithmMessage::EnableAir(_)
                            | BrokenithmMessage::TapCard
                            | BrokenithmMessage::Unknown(_) => {
                                // Do nothing
                            }
                        }
                    }

                    // Remove consumed bytes from buffer
                    if consumed > 0 {
                        buffer.drain(..consumed);
                    }
                }
                Err(e) => {
                    tracing::error!("iDevice read error: {}", e);
                    feedback_task.abort();
                    break;
                }
            }
        }
    }
}

// Legacy type aliases for backward compatibility
pub type BrokenithmTcpServer = BrokenithmTcpBackend;
pub type BrokenithmTcpClient = BrokenithmTcpBackend;
pub type BrokenithmIdeviceClient = BrokenithmTcpBackend;

#[derive(Debug)]
pub enum BrokenithmMessage {
    Welcome,
    InsertCoin,
    TapCard,
    EnableAir(bool),
    Input {
        air: Vec<u8>,
        slider: Vec<u8>,
        test_button: bool,
        service_button: bool,
    },
    Unknown(Vec<u8>),
}

fn parse_brokenithm_message(data: &[u8]) -> Option<BrokenithmMessage> {
    if data.len() < 4 {
        return None;
    }

    let length = data[0] as usize;
    let packet_name = &data[1..4];

    // Check if we have enough data for the full message
    if data.len() < length + 1 {
        return None;
    }

    // Debug logging
    // trace!(
    //     "Parsing message: len={}, name={:?} (ascii: {}), data length={}",
    //     length,
    //     packet_name,
    //     String::from_utf8_lossy(packet_name),
    //     data.len()
    // );

    match packet_name {
        [0x57, 0x45, 0x4C] if length == 3 => Some(BrokenithmMessage::Welcome), // "WEL"
        [0x46, 0x4E, 0x43] if length >= 4 => {
            // "FNC" - Function button
            let func_btn = data[4];
            match func_btn {
                1 => Some(BrokenithmMessage::InsertCoin),
                2 => Some(BrokenithmMessage::TapCard),
                _ => Some(BrokenithmMessage::Unknown(data.to_vec())),
            }
        }
        [0x41, 0x49, 0x52] if length >= 4 => {
            // "AIR" - Air enable/disable
            let air_enabled = data[4] != 0;
            // Restore to just EnableAir, let state tracker handle event emission
            Some(BrokenithmMessage::EnableAir(air_enabled))
        }
        [0x49, 0x4E, 0x50] if length >= 38 => {
            // "INP" - Input packet
            // Minimum: 6 air + 32 slider = 38 bytes payload
            // Full: 6 air + 32 slider + 1 test + 1 service = 40 bytes payload
            let payload = &data[4..];
            let payload_len = payload.len();

            // trace!(
            //     "INP message: expected_payload_len={}, actual_payload_len={}, total_len={}",
            //     length - 3,
            //     payload_len,
            //     data.len()
            // );

            if payload_len >= 38 {
                let air = reorder_air_zones(&payload[0..6]);
                let slider = payload[6..38].to_vec();

                // Test and service buttons are optional (may not be present in shorter messages)
                let test_button = if payload_len >= 39 {
                    payload[38] != 0
                } else {
                    false
                };
                let service_button = if payload_len >= 40 {
                    payload[39] != 0
                } else {
                    false
                };
                Some(BrokenithmMessage::Input {
                    air,
                    slider,
                    test_button,
                    service_button,
                })
            } else {
                tracing::warn!(
                    "INP message too short: {} bytes (need at least 38)",
                    payload.len()
                );
                Some(BrokenithmMessage::Unknown(data.to_vec()))
            }
        }
        _ => {
            tracing::debug!(
                "Unknown message: len={} name={:?} data={:x?}",
                length,
                packet_name,
                data
            );
            Some(BrokenithmMessage::Unknown(data.to_vec()))
        }
    }
}

/// Tracks previous input state to emit only changed events
struct BrokenithmInputStateTracker {
    prev_air: Vec<u8>,
    prev_slider: Vec<u8>,
    prev_test: bool,
    prev_service: bool,
    coin_pulse: bool,
}

impl BrokenithmInputStateTracker {
    fn new() -> Self {
        Self {
            prev_air: vec![0; 6],
            prev_slider: vec![0; 32],
            prev_test: false,
            prev_service: false,
            coin_pulse: false,
        }
    }

    /// Returns only the changed events as InputEventPacket (simplified, no debouncing)
    fn diff_and_packet(
        &mut self,
        air: &[u8],
        slider: &[u8],
        test_button: bool,
        service_button: bool,
    ) -> Option<InputEventPacket> {
        let mut events = Vec::new();

        // Air zones (CHUNIIO_IR_N as KeyPress/KeyRelease)
        for (i, (&prev, &curr)) in self.prev_air.iter().zip(air.iter()).enumerate() {
            let key = format!("CHUNIIO_IR_{}", i);
            if prev == 0 && curr > 0 {
                events.push(InputEvent::Keyboard(KeyboardEvent::KeyPress {
                    key: key.clone(),
                }));
            } else if prev > 0 && curr == 0 {
                events.push(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                    key: key.clone(),
                }));
            }
        }

        // Slider - direct comparison without debouncing (like C implementation)
        for (i, (&prev, &curr)) in self.prev_slider.iter().zip(slider.iter()).enumerate() {
            let key = format!("CHUNIIO_SLIDER_{}", i);
            if prev < 128 && curr >= 128 {
                events.push(InputEvent::Keyboard(KeyboardEvent::KeyPress {
                    key: key.clone(),
                }));
            } else if prev >= 128 && curr < 128 {
                events.push(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                    key: key.clone(),
                }));
            }
        }

        // Test button
        const TEST_BUTTON: &str = "CHUNIIO_TEST";
        if self.prev_test != test_button {
            events.push(InputEvent::Keyboard(if test_button {
                KeyboardEvent::KeyPress {
                    key: TEST_BUTTON.to_string(),
                }
            } else {
                KeyboardEvent::KeyRelease {
                    key: TEST_BUTTON.to_string(),
                }
            }));
        }

        // Service button
        const SERVICE_BUTTON: &str = "CHUNIIO_SERVICE";
        if self.prev_service != service_button {
            events.push(InputEvent::Keyboard(if service_button {
                KeyboardEvent::KeyPress {
                    key: SERVICE_BUTTON.to_string(),
                }
            } else {
                KeyboardEvent::KeyRelease {
                    key: SERVICE_BUTTON.to_string(),
                }
            }));
        }

        // Coin input (pulse)
        if self.coin_pulse {
            events.push(InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "CHUNIIO_COIN".to_string(),
            }));
            events.push(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                key: "CHUNIIO_COIN".to_string(),
            }));
            self.coin_pulse = false;
        }

        // Update state - use raw slider values directly (like C implementation)
        self.prev_air.copy_from_slice(air);
        self.prev_slider.copy_from_slice(slider);
        self.prev_test = test_button;
        self.prev_service = service_button;

        if events.is_empty() {
            None
        } else {
            let device_id = "brokenithm".to_string();
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            Some(InputEventPacket {
                device_id,
                timestamp,
                events,
            })
        }
    }

    /// Update shared state for chuniio_proxy polling (called after every input)
    fn update_shared_state(&self, test_button: bool, service_button: bool) {
        let prev_air = self.prev_air.clone();
        let prev_slider = self.prev_slider.clone();
        let coin_pulse = self.coin_pulse;
        tokio::spawn(async move {
            let new_state = BrokenithmInputState {
                air: prev_air,
                slider: prev_slider,
                test_button,
                service_button,
                coin_pulse,
            };
            // Only update if state actually changed
            let current = get_brokenithm_state().await;
            if current.as_ref() != Some(&new_state) {
                set_brokenithm_state(new_state).await;
            }
        });
    }

    /// Call this to pulse a coin event
    fn pulse_coin(&mut self) {
        self.coin_pulse = true;
    }
}

fn extract_brokenithm_messages(buf: &[u8]) -> (Vec<BrokenithmMessage>, usize) {
    let mut messages = Vec::new();
    let mut consumed = 0;
    let mut pos = 0;

    while pos < buf.len() {
        // Need at least 4 bytes for length + 3-byte packet name
        if pos + 4 > buf.len() {
            break;
        }

        let length = buf[pos] as usize;

        // Check if we have enough data for the full message
        if pos + 1 + length > buf.len() {
            break;
        }

        let message_data = &buf[pos..pos + 1 + length];
        if let Some(parsed) = parse_brokenithm_message(message_data) {
            messages.push(parsed);
        }

        pos += 1 + length;
        consumed = pos;
    }

    (messages, consumed)
}

// Reorder air zones to logical order: 1,2,3,4,5,6 from the received 2,1,4,3,6,5
fn reorder_air_zones(air: &[u8]) -> Vec<u8> {
    match air.len() {
        6 => vec![air[1], air[0], air[3], air[2], air[5], air[4]],
        _ => air.to_vec(),
    }
}

fn led_test_pattern() -> Vec<u8> {
    let mut led_msg = Vec::with_capacity(4 + 32 * 3);
    // Header: 99, 'L', 'E', 'D' (cLED pattern expected by Swift client)
    led_msg.extend_from_slice(&[99, 76, 69, 68]); // "cLED"
    // 32 zones, RGB triplets
    for i in 0..32 {
        if i % 2 == 0 {
            led_msg.extend_from_slice(&rgb_to_brg(&[255, 0, 0])); // Red
        } else {
            led_msg.extend_from_slice(&rgb_to_brg(&[0, 0, 255])); // Blue
        }
    }
    led_msg
}
