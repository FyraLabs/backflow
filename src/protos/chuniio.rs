//! chuniio protocol module, used for handling CHUNITHM input events and feedback.
//!
//! This module defines the protocol for handling CHUNITHM input events,
//! such as coin inputs, test mode, service mode, slider inputs, and IR inputs.
//!
//! Since the game is Windows-only, this module is designed to be used with the `chuniio_proxy` output
//! backend, which exposes a Unix Domain Socket for communication, requiring a dedicated `chuniio.dll`
//! implementation that proxies all input events to this socket.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// API version constants
pub const CHUNI_IO_API_VERSION: u16 = 0x0102;

/// Operator button bit flags
pub const CHUNI_IO_OPBTN_TEST: u8 = 0x01;
pub const CHUNI_IO_OPBTN_SERVICE: u8 = 0x02;
pub const CHUNI_IO_OPBTN_COIN: u8 = 0x04;

/// LED protocol constants
pub const LED_PACKET_FRAMING: u8 = 0xE0;
pub const LED_PACKET_ESCAPE: u8 = 0xD0;
pub const LED_NUM_MAX: usize = 66;
pub const LED_BOARDS_TOTAL: usize = 3;
pub const LED_OUTPUT_HEADER_SIZE: usize = 2;
pub const LED_OUTPUT_DATA_SIZE_MAX: usize = LED_NUM_MAX * 3 * 2; // max if every byte's escaped
pub const LED_OUTPUT_TOTAL_SIZE_MAX: usize = LED_OUTPUT_HEADER_SIZE + LED_OUTPUT_DATA_SIZE_MAX;

/// Number of slider touch regions
pub const CHUNI_SLIDER_REGIONS: usize = 32;

/// Number of IR beam sensors
pub const CHUNI_IR_BEAMS: usize = 6;

/// LED board data lengths: [board 0, board 1, board 2 (slider)]
pub const CHUNI_LED_BOARD_DATA_LENS: [usize; LED_BOARDS_TOTAL] = [53 * 3, 63 * 3, 31 * 3];

/// Message types for chuniio protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// JVS input poll request
    JvsPoll = 0x01,
    /// JVS input poll response
    JvsPollResponse = 0x02,
    /// Coin counter read request
    CoinCounterRead = 0x03,
    /// Coin counter read response
    CoinCounterReadResponse = 0x04,
    /// Slider input callback
    SliderInput = 0x05,
    /// LED update for slider
    SliderLedUpdate = 0x06,
    /// LED update for billboard/air tower
    LedUpdate = 0x07,
    /// Ping/keepalive message
    Ping = 0x08,
    /// Pong response to ping
    Pong = 0x09,
    /// Slider state read request
    SliderStateRead = 0x0A,
    /// Slider state read response
    SliderStateReadResponse = 0x0B,
    /// JVS full IO state read request (Backflow extension)
    ///
    /// This message is a Backflow-specific extension to allow the native DLL to request the entire IO state (opbtn, beams, slider, coin counter) in a single roundtrip.
    JvsFullStateRead = 0x0C,
    /// JVS full IO state read response (Backflow extension)
    ///
    /// This message is a Backflow-specific extension, sent in response to JvsFullStateRead, containing the full IO state (opbtn, beams, slider, coin counter).
    JvsFullStateReadResponse = 0x0D,
}

impl TryFrom<u8> for MessageType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(MessageType::JvsPoll),
            0x02 => Ok(MessageType::JvsPollResponse),
            0x03 => Ok(MessageType::CoinCounterRead),
            0x04 => Ok(MessageType::CoinCounterReadResponse),
            0x05 => Ok(MessageType::SliderInput),
            0x06 => Ok(MessageType::SliderLedUpdate),
            0x07 => Ok(MessageType::LedUpdate),
            0x08 => Ok(MessageType::Ping),
            0x09 => Ok(MessageType::Pong),
            0x0A => Ok(MessageType::SliderStateRead),
            0x0B => Ok(MessageType::SliderStateReadResponse),
            0x0C => Ok(MessageType::JvsFullStateRead),
            0x0D => Ok(MessageType::JvsFullStateReadResponse),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown message type: {value:#04x}"),
            )),
        }
    }
}

/// Chunithm input events that can be sent to the game
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChuniInputEvent {
    /// Operator button press/release
    OperatorButton { button: u8, pressed: bool },
    /// IR beam break/unbreak
    IrBeam { beam: u8, broken: bool },
    /// Slider touch pressure change
    SliderTouch { region: u8, pressure: u8 },
    /// Coin insertion
    CoinInsert,
}

/// Chunithm feedback events for LED control
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChuniFeedbackEvent {
    /// Update slider LEDs (31 LEDs, BRG format)
    SliderLeds { rgb_data: Vec<u8> },
    /// Update billboard/air tower LEDs
    LedBoard { board: u8, rgb_data: Vec<u8> },
}

/// JVS poll state
#[derive(Debug, Clone, Default)]
pub struct JvsState {
    /// Operator button state (bit flags)
    pub opbtn: u8,
    /// IR beam state (bit flags)
    pub beams: u8,
}

/// Slider state with pressure values for each region
#[derive(Debug, Clone)]
pub struct SliderState {
    /// Pressure values for 32 slider regions (0-255)
    pub pressure: [u8; CHUNI_SLIDER_REGIONS],
}

impl Default for SliderState {
    fn default() -> Self {
        Self {
            pressure: [0; CHUNI_SLIDER_REGIONS],
        }
    }
}

/// LED data buffer for chuniio protocol
#[derive(Debug, Clone)]
pub struct LedDataBuffer {
    /// Sync byte (LED_PACKET_FRAMING)
    pub framing: u8,
    /// LED board ID (0-1: billboard, 2: slider)
    pub board: u8,
    /// LED data buffer
    pub data: Vec<u8>,
    /// Number of bytes to output from the buffer
    pub data_len: u8,
}

impl LedDataBuffer {
    /// Create a new LED data buffer
    pub fn new(board: u8, data: Vec<u8>) -> Self {
        let data_len = data.len().min(255) as u8;
        Self {
            framing: LED_PACKET_FRAMING,
            board,
            data,
            data_len,
        }
    }

    /// Serialize to binary format
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LED_OUTPUT_TOTAL_SIZE_MAX);
        bytes.push(self.framing);
        bytes.push(self.board);

        // Apply byte stuffing for LED data
        for &byte in &self.data[..self.data_len as usize] {
            if byte == LED_PACKET_FRAMING || byte == LED_PACKET_ESCAPE {
                bytes.push(LED_PACKET_ESCAPE);
            }
            bytes.push(byte);
        }

        bytes
    }

    /// Deserialize from binary format
    pub fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < LED_OUTPUT_HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Buffer too small for LED header",
            ));
        }

        let framing = bytes[0];
        let board = bytes[1];

        if framing != LED_PACKET_FRAMING {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid framing byte",
            ));
        }

        if board >= LED_BOARDS_TOTAL as u8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid board ID",
            ));
        }

        // Decode byte stuffing
        let mut data = Vec::new();
        let mut i = LED_OUTPUT_HEADER_SIZE;
        while i < bytes.len() {
            if bytes[i] == LED_PACKET_ESCAPE && i + 1 < bytes.len() {
                data.push(bytes[i + 1]);
                i += 2;
            } else {
                data.push(bytes[i]);
                i += 1;
            }
        }

        let data_len = data.len().min(255) as u8;

        Ok(Self {
            framing,
            board,
            data,
            data_len,
        })
    }
}

/// Protocol message for chuniio communication
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ChuniMessage {
    /// JVS poll request (from game)
    JvsPoll,
    /// JVS poll response (to game)
    JvsPollResponse { opbtn: u8, beams: u8 },
    /// Coin counter read request (from game)
    CoinCounterRead,
    /// Coin counter read response (to game)
    CoinCounterReadResponse { count: u16 },
    /// Slider input data (from proxy)
    SliderInput {
        pressure: [u8; CHUNI_SLIDER_REGIONS],
    },
    /// Slider state read request (from game)
    SliderStateRead,
    /// Slider state read response (to game)
    SliderStateReadResponse {
        pressure: [u8; CHUNI_SLIDER_REGIONS],
    },
    /// Slider LED update (to proxy)
    SliderLedUpdate { rgb_data: Vec<u8> },
    /// LED board update (to proxy)
    LedUpdate { board: u8, rgb_data: Vec<u8> },
    /// Ping keepalive
    Ping,
    /// Pong response
    Pong,
    /// JVS full IO state read request (from game)
    JvsFullStateRead,
    /// JVS full IO state read response (to game)
    JvsFullStateReadResponse {
        opbtn: u8,
        beams: u8,
        pressure: [u8; CHUNI_SLIDER_REGIONS],
        coin_counter: u16,
    },
}

impl ChuniMessage {
    /// Serialize message to binary format
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        match self {
            ChuniMessage::JvsPoll => {
                bytes.push(MessageType::JvsPoll as u8);
            }
            ChuniMessage::JvsPollResponse { opbtn, beams } => {
                bytes.push(MessageType::JvsPollResponse as u8);
                bytes.push(*opbtn);
                bytes.push(*beams);
            }
            ChuniMessage::CoinCounterRead => {
                bytes.push(MessageType::CoinCounterRead as u8);
            }
            ChuniMessage::CoinCounterReadResponse { count } => {
                bytes.push(MessageType::CoinCounterReadResponse as u8);
                bytes.extend_from_slice(&count.to_le_bytes());
            }
            ChuniMessage::SliderInput { pressure } => {
                bytes.push(MessageType::SliderInput as u8);
                bytes.extend_from_slice(pressure);
            }
            ChuniMessage::SliderStateRead => {
                bytes.push(MessageType::SliderStateRead as u8);
            }
            ChuniMessage::SliderStateReadResponse { pressure } => {
                bytes.push(MessageType::SliderStateReadResponse as u8);
                bytes.extend_from_slice(pressure);
            }
            ChuniMessage::SliderLedUpdate { rgb_data } => {
                bytes.push(MessageType::SliderLedUpdate as u8);
                bytes.push(rgb_data.len() as u8);
                bytes.extend_from_slice(rgb_data);
            }
            ChuniMessage::LedUpdate { board, rgb_data } => {
                bytes.push(MessageType::LedUpdate as u8);
                bytes.push(*board);
                bytes.push(rgb_data.len() as u8);
                bytes.extend_from_slice(rgb_data);
            }
            ChuniMessage::Ping => {
                bytes.push(MessageType::Ping as u8);
            }
            ChuniMessage::Pong => {
                bytes.push(MessageType::Pong as u8);
            }
            ChuniMessage::JvsFullStateRead => {
                bytes.push(MessageType::JvsFullStateRead as u8);
            }
            ChuniMessage::JvsFullStateReadResponse {
                opbtn,
                beams,
                pressure,
                coin_counter,
            } => {
                bytes.push(MessageType::JvsFullStateReadResponse as u8);
                bytes.push(*opbtn);
                bytes.push(*beams);
                bytes.extend_from_slice(pressure);
                bytes.extend_from_slice(&coin_counter.to_le_bytes());
            }
        }

        bytes
    }

    /// Deserialize message from binary format
    pub async fn from_reader<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Self> {
        let msg_type = reader.read_u8().await?;
        let msg_type = MessageType::try_from(msg_type)?;

        match msg_type {
            MessageType::JvsPoll => Ok(ChuniMessage::JvsPoll),
            MessageType::JvsPollResponse => {
                let opbtn = reader.read_u8().await?;
                let beams = reader.read_u8().await?;
                Ok(ChuniMessage::JvsPollResponse { opbtn, beams })
            }
            MessageType::CoinCounterRead => Ok(ChuniMessage::CoinCounterRead),
            MessageType::CoinCounterReadResponse => {
                let count = reader.read_u16_le().await?;
                Ok(ChuniMessage::CoinCounterReadResponse { count })
            }
            MessageType::SliderInput => {
                let mut pressure = [0u8; CHUNI_SLIDER_REGIONS];
                reader.read_exact(&mut pressure).await?;
                Ok(ChuniMessage::SliderInput { pressure })
            }
            MessageType::SliderStateRead => Ok(ChuniMessage::SliderStateRead),
            MessageType::SliderStateReadResponse => {
                let mut pressure = [0u8; CHUNI_SLIDER_REGIONS];
                reader.read_exact(&mut pressure).await?;
                Ok(ChuniMessage::SliderStateReadResponse { pressure })
            }
            MessageType::SliderLedUpdate => {
                let len = reader.read_u8().await? as usize;
                let mut rgb_data = vec![0u8; len];
                reader.read_exact(&mut rgb_data).await?;
                Ok(ChuniMessage::SliderLedUpdate { rgb_data })
            }
            MessageType::LedUpdate => {
                let board = reader.read_u8().await?;
                let len = reader.read_u8().await? as usize;
                let mut rgb_data = vec![0u8; len];
                reader.read_exact(&mut rgb_data).await?;
                Ok(ChuniMessage::LedUpdate { board, rgb_data })
            }
            MessageType::Ping => Ok(ChuniMessage::Ping),
            MessageType::Pong => Ok(ChuniMessage::Pong),
            MessageType::JvsFullStateRead => Ok(ChuniMessage::JvsFullStateRead),
            MessageType::JvsFullStateReadResponse => {
                let opbtn = reader.read_u8().await?;
                let beams = reader.read_u8().await?;
                let mut pressure = [0u8; CHUNI_SLIDER_REGIONS];
                reader.read_exact(&mut pressure).await?;
                let coin_counter = reader.read_u16_le().await?;
                Ok(ChuniMessage::JvsFullStateReadResponse {
                    opbtn,
                    beams,
                    pressure,
                    coin_counter,
                })
            }
        }
    }

    /// Write message to async writer
    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let bytes = self.to_bytes();
        writer.write_all(&bytes).await
    }
}

/// Chunithm protocol server state
#[derive(Debug, Default)]
pub struct ChuniProtocolState {
    /// Current JVS state
    pub jvs_state: JvsState,
    /// Current slider state
    pub slider_state: SliderState,
    /// Coin counter
    pub coin_counter: u16,
    /// API version
    pub api_version: u16,
}

impl ChuniProtocolState {
    /// Create new protocol state
    pub fn new() -> Self {
        Self {
            api_version: CHUNI_IO_API_VERSION,
            ..Default::default()
        }
    }

    /// Update operator button state
    pub fn set_operator_button(&mut self, button: u8, pressed: bool) {
        if pressed {
            self.jvs_state.opbtn |= button;
        } else {
            self.jvs_state.opbtn &= !button;
        }
    }

    /// Update IR beam state
    pub fn set_ir_beam(&mut self, beam: u8, broken: bool) {
        if beam < CHUNI_IR_BEAMS as u8 {
            if broken {
                self.jvs_state.beams |= 1 << beam;
            } else {
                self.jvs_state.beams &= !(1 << beam);
            }
        }
    }

    /// Update slider touch pressure
    pub fn set_slider_pressure(&mut self, region: u8, pressure: u8) {
        if (region as usize) < CHUNI_SLIDER_REGIONS {
            self.slider_state.pressure[region as usize] = pressure;
        }
    }

    /// Increment coin counter
    pub fn add_coin(&mut self) {
        self.coin_counter = self.coin_counter.saturating_add(1);
    }

    /// Process input event
    pub fn process_input_event(&mut self, event: ChuniInputEvent) {
        match event {
            ChuniInputEvent::OperatorButton { button, pressed } => {
                self.set_operator_button(button, pressed);
            }
            ChuniInputEvent::IrBeam { beam, broken } => {
                self.set_ir_beam(beam, broken);
            }
            ChuniInputEvent::SliderTouch { region, pressure } => {
                self.set_slider_pressure(region, pressure);
            }
            ChuniInputEvent::CoinInsert => {
                self.add_coin();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_message_serialization() {
        let msg = ChuniMessage::JvsPollResponse {
            opbtn: 0x01,
            beams: 0x3F,
        };
        let bytes = msg.to_bytes();
        assert_eq!(bytes[0], MessageType::JvsPollResponse as u8);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 0x3F);

        // For testing, we'll use tokio_util::io::ReaderStream or similar
        // For now, let's just test serialization
        let serialized = msg.to_bytes();
        assert_eq!(
            serialized,
            vec![MessageType::JvsPollResponse as u8, 0x01, 0x3F]
        );
    }

    #[test]
    fn test_led_data_buffer() {
        let data = vec![0xFF, LED_PACKET_FRAMING, 0x00, LED_PACKET_ESCAPE, 0x42];
        let buffer = LedDataBuffer::new(2, data.clone());

        let bytes = buffer.to_bytes();
        let decoded = LedDataBuffer::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.board, 2);
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn test_protocol_state() {
        let mut state = ChuniProtocolState::new();

        // Test operator button
        state.set_operator_button(CHUNI_IO_OPBTN_TEST, true);
        assert_eq!(
            state.jvs_state.opbtn & CHUNI_IO_OPBTN_TEST,
            CHUNI_IO_OPBTN_TEST
        );

        // Test IR beam
        state.set_ir_beam(3, true);
        assert_eq!(state.jvs_state.beams & (1 << 3), 1 << 3);

        // Test slider
        state.set_slider_pressure(15, 128);
        assert_eq!(state.slider_state.pressure[15], 128);

        // Test coin
        state.add_coin();
        assert_eq!(state.coin_counter, 1);
    }
}
