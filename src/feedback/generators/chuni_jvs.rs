//! JVS-like feedback event generator for CHUNITHM/segatools-chuniio integration.
//!
//! segatools is a "crack" for CHUNITHM and various other Sega ALL.net arcade games
//! that allows users to run those games on generic PC hardware.
//!
//! While the Sega ALLS platform is already based on off-the-shelf PC hardware,
//! the segatools project provides a way to run these games in a "bootleg"
//! environment, allowing users to play these games without the need for official Sega hardware or
//! online infrastructure.
//!
//! ...but I digress.
//!
//! This module takes in JVS frames from a UNIX domain socket (from the Outflow Named Pipe bridge),
//! parses these frames, and generates feedback events that can be sent to devices or clients.
//!
//! segatools' RGB protocol for CHUNITHM is a JVS-like protocol,
//! sending individual frames with RGB data for each light.
//!
//! # Hardware Layout
//!
//! A typical CHUNITHM cabinet has 32+ LEDs across multiple boards:
//! - Board 0: Billboard LEDs (53 LEDs, 159 bytes RGB data)
//! - Board 1: Billboard LEDs (63 LEDs, 189 bytes RGB data)
//! - Board 2: Slider LEDs (31 LEDs, 93 bytes RGB data)
//!
//! What you'll be seeing most of the time though is the Slider LEDs, which have 31
//! bulbs.
//!
//! A typical CHUNITHM slider will be a long touchpad with 2 rows of 16 touch zones each,
//! but the game internally only keeps track of 16 keys, so it will
//! display duplicate data.
//!
//! Keep note that the slider LED indexes are in reverse order, so the first LED is on the right side of the slider,

use crate::config::ChuniIoRgbConfig;
use crate::feedback::{FeedbackEvent, Rgb};
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
#[allow(dead_code)]
const LED_PACKET_FRAMING: u8 = 0xE0;
const LED_PACKET_ESCAPE: u8 = 0xD0;
#[allow(dead_code)]
const LED_NUM_MAX: usize = 66;
const LED_BOARDS_TOTAL: usize = 3;
#[allow(dead_code)]
const LED_OUTPUT_HEADER_SIZE: usize = 2;
#[allow(dead_code)]
const LED_OUTPUT_DATA_SIZE_MAX: usize = LED_NUM_MAX * 3 * 2; // max if every byte's escaped
#[allow(dead_code)]
const LED_OUTPUT_TOTAL_SIZE_MAX: usize = LED_OUTPUT_HEADER_SIZE + LED_OUTPUT_DATA_SIZE_MAX;

// Data lengths for each LED board (in bytes, RGB = 3 bytes per LED)
const CHUNI_LED_BOARD_DATA_LENS: [usize; LED_BOARDS_TOTAL] = [
    53 * 3, // Board 0: Billboard LEDs
    63 * 3, // Board 1: Billboard LEDs
    31 * 3, // Board 2: Slider LEDs
];


/// LED board types with their specific data
#[derive(Debug, Clone)]
pub enum LedBoardData {
    BillboardLeft([Rgb; 53]),
    BillboardRight([Rgb; 63]),
    Slider([Rgb; 31]),
}

/// Decode error types
#[derive(Debug)]
pub enum DecodeError {
    Invalid,
    Incomplete,
    PacketTooShort,
    InvalidFraming,
    InvalidBoardId,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Invalid => write!(f, "Invalid packet data"),
            DecodeError::Incomplete => write!(f, "Incomplete packet"),
            DecodeError::PacketTooShort => write!(f, "Packet too short"),
            DecodeError::InvalidFraming => write!(f, "Invalid framing byte"),
            DecodeError::InvalidBoardId => write!(f, "Invalid board ID"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// CHUNITHM LED data packet structure
#[derive(Debug, Clone)]
pub struct ChuniLedDataPacket {
    /// Board identifier (0-1: billboard, 2: slider)
    pub board: u8,
    /// Parsed LED data for the specific board
    pub data: LedBoardData,
}

/// Utility functions for LED processing
impl ChuniLedDataPacket {
    /// Convert raw bytes to RGB values, handling the BGR format from the protocol
    fn bytes_to_rgb_vec(data: &[u8]) -> Vec<Rgb> {
        data.chunks(3)
            .filter(|chunk| chunk.len() == 3)
            .map(|chunk| Rgb {
                r: chunk[1], // R is at index 1
                g: chunk[2], // G is at index 2
                b: chunk[0], // B is at index 0 (BGR format)
            })
            .collect()
    }

    /// Reverse slider LEDs since they're indexed from right to left
    fn reverse_slider_leds(leds: [Rgb; 31]) -> [Rgb; 31] {
        let mut reversed = leds;
        reversed.reverse();
        reversed
    }

    /// Clamp slider LEDs to a smaller number of zones by selecting key LEDs only
    ///
    /// The 31 slider LEDs alternate between keys and dividers. For clamping, we select
    /// only the key LEDs (every other LED) to skip the divider LEDs at the edges.
    /// This gives us up to 16 key LEDs (indices 0, 2, 4, 6, ..., 30).
    pub fn clamp_slider_to_zones(leds: [Rgb; 31], zones: usize) -> Vec<Rgb> {
        if zones == 0 {
            return Vec::new();
        }

        // Extract key LEDs only (every other LED, starting from index 0)
        // This skips the divider LEDs and gives us the 16 key LEDs
        let key_leds: Vec<Rgb> = leds
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 2 == 0) // Take every other LED (keys, not dividers)
            .map(|(_, led)| *led)
            .collect();

        // If zones >= number of key LEDs, just return all key LEDs
        if zones >= key_leds.len() {
            return key_leds;
        }

        let mut clamped = Vec::with_capacity(zones);

        for zone_idx in 0..zones {
            let start_key = (zone_idx * key_leds.len()) / zones;
            let end_key = ((zone_idx + 1) * key_leds.len()) / zones;

            // Average the RGB values of the key LEDs in this zone
            let mut total_r = 0u32;
            let mut total_g = 0u32;
            let mut total_b = 0u32;
            let mut count = 0u32;

            for led in &key_leds[start_key..end_key] {
                total_r += led.r as u32;
                total_g += led.g as u32;
                total_b += led.b as u32;
                count += 1;
            }

            let avg_rgb = if count > 0 {
                Rgb {
                    r: (total_r / count) as u8,
                    g: (total_g / count) as u8,
                    b: (total_b / count) as u8,
                }
            } else {
                Rgb { r: 0, g: 0, b: 0 }
            };

            clamped.push(avg_rgb);
        }

        clamped
    }

    /// Parse a packet from raw bytes with proper escape sequence handling
    pub fn try_parse_packet(buf: &[u8]) -> Result<(ChuniLedDataPacket, usize), DecodeError> {
        if buf.len() < 2 {
            return Err(DecodeError::PacketTooShort);
        }

        if buf[0] != LED_PACKET_FRAMING {
            return Err(DecodeError::InvalidFraming);
        }

        let board = buf[1];
        if (board as usize) >= LED_BOARDS_TOTAL {
            return Err(DecodeError::InvalidBoardId);
        }

        let expected_data_len = CHUNI_LED_BOARD_DATA_LENS[board as usize];
        let mut decoded = Vec::with_capacity(expected_data_len);
        let mut i = 2;

        // Handle escape sequences
        while i < buf.len() && decoded.len() < expected_data_len {
            let b = buf[i];
            if b == LED_PACKET_ESCAPE {
                i += 1;
                if i >= buf.len() {
                    return Err(DecodeError::Incomplete);
                }
                decoded.push(buf[i].wrapping_add(1));
            } else {
                decoded.push(b);
            }
            i += 1;
        }

        if decoded.len() < expected_data_len {
            return Err(DecodeError::Incomplete);
        }

        let rgb_vec = Self::bytes_to_rgb_vec(&decoded);
        let data = match board {
            0 => {
                if rgb_vec.len() < 53 {
                    return Err(DecodeError::Invalid);
                }
                LedBoardData::BillboardLeft(
                    rgb_vec[..53].try_into().map_err(|_| DecodeError::Invalid)?,
                )
            }
            1 => {
                if rgb_vec.len() < 63 {
                    return Err(DecodeError::Invalid);
                }
                LedBoardData::BillboardRight(
                    rgb_vec[..63].try_into().map_err(|_| DecodeError::Invalid)?,
                )
            }
            2 => {
                if rgb_vec.len() < 31 {
                    return Err(DecodeError::Invalid);
                }
                let slider_leds: [Rgb; 31] =
                    rgb_vec[..31].try_into().map_err(|_| DecodeError::Invalid)?;
                LedBoardData::Slider(Self::reverse_slider_leds(slider_leds))
            }
            _ => return Err(DecodeError::InvalidBoardId),
        };

        Ok((ChuniLedDataPacket { board, data }, i))
    }

    /// Get RGB values as a vector (useful for generic processing)
    pub fn to_rgb_vec(&self) -> Vec<Rgb> {
        match &self.data {
            LedBoardData::BillboardLeft(leds) => leds.to_vec(),
            LedBoardData::BillboardRight(leds) => leds.to_vec(),
            LedBoardData::Slider(leds) => leds.to_vec(),
        }
    }

    /// Get the number of LEDs in this packet
    pub fn led_count(&self) -> usize {
        match &self.data {
            LedBoardData::BillboardLeft(_) => 53,
            LedBoardData::BillboardRight(_) => 63,
            LedBoardData::Slider(_) => 31,
        }
    }
}

/// Board type enumeration for better type safety
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChuniLedBoard {
    Billboard0 = 0,
    Billboard1 = 1,
    Slider = 2,
}

impl TryFrom<u8> for ChuniLedBoard {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ChuniLedBoard::Billboard0),
            1 => Ok(ChuniLedBoard::Billboard1),
            2 => Ok(ChuniLedBoard::Slider),
            _ => Err("Invalid board ID"),
        }
    }
}

impl ChuniLedDataPacket {
    /// Get board type as enum
    pub fn board_type(&self) -> Result<ChuniLedBoard, &'static str> {
        ChuniLedBoard::try_from(self.board)
    }
}

/// CHUNITHM LED packet parser
pub struct ChuniLedParser {
    // TODO: Add internal state for handling partial packets, escape sequences, etc.
}

impl ChuniLedParser {
    /// Create a new parser instance
    pub fn new() -> Self {
        Self {}
    }

    /// Parse LED packets from a byte stream with proper escape sequence handling
    pub fn parse_packets(&mut self, data: &[u8]) -> Result<Vec<ChuniLedDataPacket>, DecodeError> {
        let mut packets = Vec::new();
        let mut window = data;

        while !window.is_empty() {
            // Look for framing byte
            if let Some(frame_pos) = window.iter().position(|&b| b == LED_PACKET_FRAMING) {
                if frame_pos > 0 {
                    window = &window[frame_pos..]; // Skip garbage before sync
                }

                match ChuniLedDataPacket::try_parse_packet(window) {
                    Ok((packet, used)) => {
                        packets.push(packet);
                        window = &window[used..];
                    }
                    Err(DecodeError::Incomplete) => break, // Need more data
                    Err(_) => {
                        window = &window[1..]; // Skip bad byte
                    }
                }
            } else {
                break; // No more framing bytes found
            }
        }

        Ok(packets)
    }

    /// Convert LED packet to feedback events
    pub fn packet_to_feedback_events(&self, packet: &ChuniLedDataPacket) -> Vec<FeedbackEvent> {
        let mut events = Vec::new();
        let rgb_values = packet.to_rgb_vec();

        // Convert RGB values to feedback events
        for (led_index, rgb) in rgb_values.iter().enumerate() {
            // Map LED positions to feedback events
            events.push(FeedbackEvent::Led(crate::feedback::LedEvent::Set {
                led_id: led_index as u8,
                on: rgb.r > 0 || rgb.g > 0 || rgb.b > 0, // LED is on if any color component is non-zero
                brightness: Some(rgb.r.max(rgb.g).max(rgb.b)), // Use max RGB component as brightness
                rgb: Some((rgb.r, rgb.g, rgb.b)),
            }));
        }

        events
    }
}

impl Default for ChuniLedParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Create FeedbackEvent from an array of LED RGB values
/// note: Must already be prepared in proper RGB, transform if needed
pub fn rgb_to_feedback_event(rgb: Rgb, led_id: u8) -> FeedbackEvent {
    FeedbackEvent::Led(crate::feedback::LedEvent::Set {
        led_id,
        on: rgb.r > 0 || rgb.g > 0 || rgb.b > 0,
        brightness: Some(rgb.r.max(rgb.g).max(rgb.b)),
        rgb: Some((rgb.r, rgb.g, rgb.b)),
    })
}

/// Create and run a CHUNITHM RGB lightsync service
pub async fn run_chuniio_service(
    config: ChuniIoRgbConfig,
    feedback_stream: crate::feedback::FeedbackEventStream,
    rgb_stream: ChuniRgbStream,
) -> eyre::Result<()> {
    let mut service = ChuniRgbService::new(config, feedback_stream, rgb_stream);
    service.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_parsing() {
        let mut data = vec![LED_PACKET_FRAMING, 2]; // Board 2 (slider)
        // Add RGB data for 31 LEDs (red color in BGR format: B=0, G=0, R=255)
        for _ in 0..31 {
            data.extend_from_slice(&[0, 255, 0]); // BGR format: B=0, G=0, R=255 -> RGB: (255, 0, 0) red
        }

        let (packet, _used) = ChuniLedDataPacket::try_parse_packet(&data).unwrap();
        assert_eq!(packet.board, 2);
        assert_eq!(packet.board_type().unwrap(), ChuniLedBoard::Slider);
        assert_eq!(packet.led_count(), 31);

        // Check that we have slider data
        if let LedBoardData::Slider(leds) = &packet.data {
            assert_eq!(leds.len(), 31);
            // Check that LEDs were parsed correctly from BGR: chunk[0,1,2] -> RGB(chunk[1], chunk[2], chunk[0])
            // Input BGR [0, 255, 0] -> RGB (255, 0, 0) which is red
            assert_eq!(leds[0].r, 255); // Should be red
            assert_eq!(leds[0].g, 0); // Should be no green
            assert_eq!(leds[0].b, 0); // Should be no blue
        } else {
            panic!("Expected slider data");
        }
    }

    #[test]
    fn test_parser() {
        let mut parser = ChuniLedParser::new();
        let mut data = vec![LED_PACKET_FRAMING, 0]; // Board 0 (billboard)
        // Add RGB data for 53 LEDs (green color in BGR format)
        for _ in 0..53 {
            data.extend_from_slice(&[0, 0, 255]); // BGR: blue=0, green=0, red=255 -> RGB: red=255
        }

        let packets = parser.parse_packets(&data).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].board, 0);
        assert_eq!(packets[0].led_count(), 53);
    }

    #[test]
    fn test_led_clamping() {
        // Test the LED clamping utility
        let test_leds = [Rgb { r: 255, g: 0, b: 0 }; 31]; // All red LEDs

        // Clamp to 8 zones
        let clamped = ChuniLedDataPacket::clamp_slider_to_zones(test_leds, 8);
        assert_eq!(clamped.len(), 8);

        // All zones should be red since all input LEDs are red
        for zone in &clamped {
            assert_eq!(zone.r, 255);
            assert_eq!(zone.g, 0);
            assert_eq!(zone.b, 0);
        }

        // Test with 0 zones
        let empty = ChuniLedDataPacket::clamp_slider_to_zones(test_leds, 0);
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_escape_sequences() {
        let mut data = vec![LED_PACKET_FRAMING, 2]; // Board 2 (slider)

        // Add some RGB data with escape sequences
        data.extend_from_slice(&[LED_PACKET_ESCAPE, 0xDF, 100, 200]); // Escaped 0xE0, then normal 100, 200
        data.extend_from_slice(&[50, LED_PACKET_ESCAPE, 0xCF, 150]); // Normal 50, escaped 0xD0, then 150

        // Fill the rest to make a complete packet (31 LEDs * 3 bytes = 93 bytes total needed)
        // We already have 8 bytes of actual data (after escape processing: 0xE0, 100, 200, 50, 0xD0, 150)
        // So we need 93 - 6 = 87 more bytes (since escapes will be processed to 6 bytes)
        let remaining_bytes_needed = 93 - 6; // 87 bytes
        data.extend(vec![0; remaining_bytes_needed]);

        let result = ChuniLedDataPacket::try_parse_packet(&data);
        if let Err(ref e) = result {
            eprintln!("Parse error: {:?}", e);
            eprintln!("Data length: {}, Expected: {}", data.len(), 93 + 2);
        }
        assert!(result.is_ok());

        let (packet, _used) = result.unwrap();
        assert_eq!(packet.board, 2);

        if let LedBoardData::Slider(leds) = &packet.data {
            assert_eq!(leds.len(), 31);
        } else {
            panic!("Expected slider data");
        }
    }
}

/// Stream for receiving parsed CHUNITHM RGB packets (not just raw RGB values)
#[derive(Clone)]
pub struct ChuniRgbStream {
    /// Sender for LED data packets
    pub tx: tokio::sync::mpsc::Sender<ChuniLedDataPacket>,
    /// Receiver for LED data packets (wrapped for async use)
    pub rx: std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ChuniLedDataPacket>>>,
}

impl ChuniRgbStream {
    /// Create a new stream with a bounded channel
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        Self {
            tx,
            rx: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }

    /// Send a LED data packet through the stream
    pub async fn send(
        &self,
        packet: ChuniLedDataPacket,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<ChuniLedDataPacket>> {
        self.tx.send(packet).await
    }
}

/// CHUNITHM RGB feedback service that listens on a Unix domain socket
/// and processes JVS-like LED data packets
pub struct ChuniRgbService {
    config: ChuniIoRgbConfig,
    packet_stream: ChuniRgbStream,
    parser: ChuniLedParser,
    feedback_stream: crate::feedback::FeedbackEventStream,
}

impl ChuniRgbService {
    /// Create a new CHUNITHM RGB feedback service
    pub fn new(
        config: ChuniIoRgbConfig,
        feedback_stream: crate::feedback::FeedbackEventStream,
        rgb_stream: ChuniRgbStream,
    ) -> Self {
        Self {
            config,
            parser: ChuniLedParser::new(),
            feedback_stream,
            packet_stream: rgb_stream,
        }
    }
    
    /// Start the service and listen for LED data on the Unix domain socket
    pub async fn run(&mut self) -> eyre::Result<()> {
        // todo: Don't actually manually create a socket listener here, just feed in packets from packet_stream
        tracing::info!(
            "Starting CHUNITHM RGB service listening on: {:?}",
            self.config.socket_path
        );

        // Remove socket file if it exists
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
            tracing::debug!("Removed existing socket file");
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = self.config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&self.config.socket_path)?;
        tracing::info!("CHUNITHM RGB service bound to socket, waiting for connections...");

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    tracing::info!("New CHUNITHM RGB client connected");
                    if let Err(e) = self.handle_client(stream).await {
                        tracing::error!("Error handling CHUNITHM RGB client: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Error accepting CHUNITHM RGB connection: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Handle a single client connection
    async fn handle_client(&mut self, mut stream: tokio::net::UnixStream) -> eyre::Result<()> {
        let mut buffer = vec![0u8; 4096];
        let mut accumulated_data = Vec::new();

        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => {
                    tracing::info!("CHUNITHM RGB client disconnected");
                    break;
                }
                Ok(bytes_read) => {
                    accumulated_data.extend_from_slice(&buffer[..bytes_read]);

                    // Try to parse packets from accumulated data
                    match self.parser.parse_packets(&accumulated_data) {
                        Ok(packets) => {
                            for packet in packets {
                                self.process_led_packet(&packet).await?;
                            }
                            // Clear processed data - in a real implementation, you'd want to
                            // track how much data was consumed and only clear that portion
                            accumulated_data.clear();
                        }
                        Err(e) => {
                            tracing::debug!("Failed to parse LED packet: {}", e);
                            // Don't clear data immediately in case we need more bytes
                            // Only clear if buffer gets too large
                            if accumulated_data.len() > 8192 {
                                tracing::warn!(
                                    "Clearing large accumulated buffer due to parse errors"
                                );
                                accumulated_data.clear();
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Error reading from CHUNITHM RGB client: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process a single LED packet and generate feedback events
    async fn process_led_packet(&mut self, packet: &ChuniLedDataPacket) -> eyre::Result<()> {
        tracing::trace!(
            target: crate::PACKET_PROCESSING_TARGET,
            "Processing LED packet for board {} with {} LEDs",
            packet.board,
            packet.led_count()
        );

        let (device_id, events) = match packet.board {
            0 => (
                "chunithm_billboard_left".to_string(),
                self.generate_billboard_feedback_events(packet, 0)?,
            ),
            1 => (
                "chunithm_billboard_right".to_string(),
                self.generate_billboard_feedback_events(packet, 1)?,
            ),
            2 => (
                "chunithm_slider".to_string(),
                self.generate_slider_feedback_events(packet)?,
            ),
            _ => {
                tracing::warn!("Unknown board ID {}, skipping packet", packet.board);
                return Ok(());
            }
        };

        // Send all LED events in one packet - this is more efficient for the web UI
        // which can batch process all the LED updates at once
        if !events.is_empty() {
            let event_count = events.len();
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            let feedback_packet = crate::feedback::FeedbackEventPacket {
                device_id,
                timestamp,
                events, // Send all LED events together for better performance
            };

            if let Err(e) = self.feedback_stream.send(feedback_packet).await {
                tracing::error!("Failed to send feedback packet: {}", e);
            } else {
                tracing::trace!(
                    target: crate::PACKET_PROCESSING_TARGET,
                    "Successfully sent feedback packet with {} LED events for board {}",
                    event_count,
                    packet.board
                );
            }
        }

        Ok(())
    }

    /// Generate feedback events specifically for slider LED data
    fn generate_slider_feedback_events(
        &self,
        packet: &ChuniLedDataPacket,
    ) -> eyre::Result<Vec<FeedbackEvent>> {
        let mut events = Vec::new();

        if let LedBoardData::Slider(slider_leds) = &packet.data {
            // Apply clamping if configured
            let processed_leds = if self.config.slider_clamp_lights < 31 {
                ChuniLedDataPacket::clamp_slider_to_zones(
                    *slider_leds,
                    self.config.slider_clamp_lights as usize,
                )
            } else {
                slider_leds.to_vec()
            };

            // Generate LED events with ID offset
            for (index, rgb) in processed_leds.iter().enumerate() {
                let led_id = (index as u32 + self.config.slider_id_offset) as u8;

                events.push(rgb_to_feedback_event(*rgb, led_id));
            }

            tracing::trace!(
                target: crate::PACKET_PROCESSING_TARGET,
                "Generated {} LED feedback events for slider (clamped from {} to {} lights, LED ID offset: {})",
                events.len(),
                31,
                self.config.slider_clamp_lights,
                self.config.slider_id_offset
            );
        }

        Ok(events)
    }

    /// Generate feedback events for billboard LED data
    fn generate_billboard_feedback_events(
        &self,
        packet: &ChuniLedDataPacket,
        board_id: u8,
    ) -> eyre::Result<Vec<FeedbackEvent>> {
        let mut events = Vec::new();

        let leds = match &packet.data {
            LedBoardData::BillboardLeft(leds) => leds.as_slice(),
            LedBoardData::BillboardRight(leds) => leds.as_slice(),
            LedBoardData::Slider(_) => {
                return Err(eyre::eyre!("Expected billboard data, got slider data"));
            }
        };

        // Generate LED events with board-specific ID offset
        let id_offset = match board_id {
            0 => 0,   // Billboard left starts at LED ID 0
            1 => 100, // Billboard right starts at LED ID 100
            _ => return Err(eyre::eyre!("Invalid billboard board ID: {}", board_id)),
        };

        for (index, rgb) in leds.iter().enumerate() {
            let led_id = (index + id_offset) as u8;

            events.push(FeedbackEvent::Led(crate::feedback::LedEvent::Set {
                led_id,
                on: rgb.r > 0 || rgb.g > 0 || rgb.b > 0,
                brightness: Some(rgb.r.max(rgb.g).max(rgb.b)),
                rgb: Some((rgb.r, rgb.g, rgb.b)),
            }));
        }

        tracing::trace!(
            target: crate::PACKET_PROCESSING_TARGET,
            "Generated {} LED feedback events for billboard board {} (LED ID offset: {})",
            events.len(),
            board_id,
            id_offset
        );

        Ok(events)
    }
}
