//! Unified I/O server implementation
//!
//! # Overview
//! A single asynchronous hub that normalizes all inbound messages coming from any
//! transport/backend (WebSocket, Unix socket, plugin stdio, etc.) into a shared
//! processing pipeline. It performs:
//! * Per-client input buffering + atomic batching (via [`AtomicInputProcessor`])
//! * Feedback (LED / other) rebroadcast + filtering
//! * Control plane for dynamic stream/topic subscription & management
//! * Outbound fan-out to output backends (uinput, chuniio, …) and interested clients
//!
//! # Terminology
//! * **Client**: A connected producer/consumer (web page, plugin process, future TCP peer)
//! * **Stream / Topic ID**: Arbitrary logical channel name a client can register for.
//!   Output routing uses backend names as stream IDs (e.g. `"uinput"`, `"chuniio"`).
//! * **Device ID**: The logical input device name contained inside each packet.
//!
//! # Inbound Message JSON Schemas (Text frames)
//! Transports send UTF‑8 JSON. Binary frames are currently ignored.
//!
//! ## 1. Input Packet
//! ```json
//! {
//!   "device_id": "keyboard1",
//!   "timestamp": 1755488100050,
//!   "events": [
//!     { "Keyboard": { "KeyPress": { "key": "KEY_A" } } },
//!     { "Keyboard": { "KeyRelease": { "key": "KEY_A" } } }
//!   ]
//! }
//! ```
//! Accepted fields map to [`InputEventPacket`]. Additional fields are ignored.
//!
//! ## 2. Feedback Packet
//! ```json
//! {
//!   "device_id": "rgb-test",
//!   "timestamp": 1755488100050,
//!   "events": [
//!     { "Led": { "Set": { "led_id": 0, "on": true, "brightness": 255, "rgb": [255,0,128] } } }
//!   ]
//! }
//! ```
//! Matches [`FeedbackEventPacket`]. Forwarded to all non write-only clients that either:
//! * Have an empty `feedback_listener_device_ids` filter (subscribe to all), **or**
//! * Explicitly include `device_id` in their filter list.
//!
//! ## 3. Control Messages
//! Control messages allow a client to mutate subscriptions. Envelope:
//! ```json
//! { "type": "control", "cmd": <command object ...> }
//! ```
//! Supported commands (snake_case):
//! * `register_streams` – Declaratively set the complete stream/topic ID set this client wants to receive.
//!   ```json
//!   { "type": "control", "cmd": "register_streams", "streams": ["uinput","foo"] }
//!   ```
//!   Response:
//!   ```json
//!   { "type": "control_ack", "cmd": "update_stream_registration", "registered_streams": ["uinput","foo"] }
//!   ```
//! * `list_streams` – Request the current union of all known registered stream IDs:
//!   ```json
//!   { "type": "control", "cmd": "list_streams" }
//!   ```
//!   Response:
//!   ```json
//!   { "type": "streams", "streams": ["uinput","chuniio","foo"] }
//!   ```
//!
//! (Additional planned control commands: input / feedback filter updates & unregister.)
//!
//! # Outbound Messages to Clients
//! Tagged union (serde `#[serde(tag = "type")]`) represented here conceptually:
//! ```jsonc
//! // Feedback echo/broadcast
//! { "type": "Feedback", "data": { /* FeedbackEventPacket */ } }
//! // Input packet routed to a subscribed stream/topic
//! { "type": "Input", "data": { /* InputEventPacket */ } }
//! // Error notification
//! { "type": "Error", "data": { "code": "parse_error", "message": "...", "raw": "..." } }
//! // Control responses (NOT part of IoClientMessage, emitted directly):
//! { "type": "control_ack", ... }
//! { "type": "streams", ... }
//! ```
//! NOTE: The `control_ack` & `streams` payloads are transport-level convenience responses,
//! not serialized via `IoClientMessage` enum (they're sent directly in the websocket layer).
//!
//! # Stream / Topic Semantics
//! * A client that registers stream `foo` will receive every outbound `Input` packet that the
//!   routing service mirrors to stream `foo` (currently backend name == stream ID).
//! * Multiple clients may register the same stream ID; each receives identical copies.
//! * Registration is declarative (the provided list replaces any previous set; duplicates ignored).
//! * To clear all stream registrations (reverting to device filter mode), send an empty list: `{ "type":"control", "cmd":"register_streams", "streams":[] }`.
//! * No explicit unregister command necessary.
//! * Precedence: If a client has registered at least one stream/topic, device ID
//!   based `input_listener_device_ids` filtering is ignored for input delivery (topic mode).
//!   If no streams are registered, device filters apply. IMPORTANT: An empty
//!   `input_listener_device_ids` list now means "subscribe to NONE" (opt‑in model).
//!   Provide at least one device id to receive packets without using stream/topics.
//!
//! # Routing & Broadcasting Path
//! 1. Client sends raw input → `IoInboundMessage::Input`.
//! 2. Atomic processor batches; emits `IoOutboundMessage::Input`.
//! 3. Backend routing service:
//!    * Device filter transforms keys (remap / whitelist).
//!    * Events partitioned by backend (e.g. `uinput`, `chuniio`).
//!    * Packet delivered to output backend stream AND mirrored to any subscribed stream/topic.
//! 4. Clients who registered that backend/topic receive `IoClientMessage::Input`.
//! 5. Feedback packets (`IoInboundMessage::Feedback`) immediately rebroadcast & surfaced to all eligible clients.
//!
//! # Error Handling
//! * Malformed JSON that does not match any known schema currently produces a warning only.
//! * Future: explicit `Error` packet back to origin with code `parse_error`.
//!
//! # Versioning & Forward Compatibility
//! This informal schema is considered unstable until a `protocol_version` control query
//! is added. Add fields using `#[serde(skip_serializing_if = "Option::is_none")]` for
//! forward compatibility.
//!
//! # Implementation Notes
//! * The server internally tags device IDs temporarily with `@client_id` for atomic grouping
//!   then strips the suffix before outbound emission.
//! * Stream listing is derived dynamically (union over all client registrations).
//! * `write_only` clients never receive feedback or input stream packets.
//!
//! # Adding a New Transport
//! 1. Register client (choose an `id`).
//! 2. Forward parsed input as `IoInboundMessage::Input`.
//! 3. Forward feedback as `IoInboundMessage::Feedback`.
//! 4. Parse & forward control JSON to `IoInboundMessage::Control` (wrapping client id).
//! 5. Relay outbound `IoClientMessage` variants as text frames.
//!
//! Keep this documentation in sync when adding new control commands or message variants.
use std::{collections::HashMap, sync::Arc};

use crate::error::{AppError, ClientFacingError};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    feedback::FeedbackEventPacket,
    input::{InputBackend, InputEventPacket, atomic::AtomicInputProcessor},
};

/// A Client "node" in Backflow's I/O stream
///
/// This struct represents a single client connection in the I/O stream,
/// allowing for the routing of I/O events
///
/// The client can be either a producer or a consumer of events,
///
/// and contributes to the overall pipeline
///
/// Backflow makes no distinctions between
/// plugins and normal clients, as to any "client"
/// can generate or consume events in any order they wish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientNode {
    pub id: String,
    pub client_settings: ClientSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientSettings {
    /// Device IDs to subscribe feedback events from
    /// By default, all feedback events will be subscribed to
    pub feedback_listener_device_ids: Vec<String>,

    /// Device IDs to subscribe input events from
    /// By default, no events will be subscribed to
    pub input_listener_device_ids: Vec<String>,

    /// Registered stream IDs for the client, for filtering based on stream type instead
    /// of device ID
    ///
    /// In other MQ systems, this is typically just called a "topic".
    pub registered_stream_ids: Vec<String>,

    /// Whether the client is write-only,
    /// meaning they will not receive any feedback events
    pub write_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
#[serde(rename_all = "snake_case")]
pub enum ClientControlMessage {
    /// Request that the server stop sending feedback events to this client
    #[serde(alias = "StopFeedback")] // legacy alias
    StopFeedback,

    /// Request that the server stop sending input events to this client
    #[serde(alias = "StopInput")] // legacy alias
    StopInput,

    /// Update (replace) the device ID filter for input events (whitelist of device_ids)
    #[serde(
        alias = "UpdateInputDIDFilter",
        alias = "update_input_did_filter",
        alias = "update_input_didfilter"
    )]
    UpdateInputDIDFilter {
        #[serde(default, alias = "devices", alias = "device_ids")]
        devices: Vec<String>,
    },

    /// Update (replace) the device ID filter for feedback events (whitelist of device_ids)
    #[serde(
        alias = "UpdateFeedbackDIDFilter",
        alias = "update_feedback_did_filter",
        alias = "update_feedback_didfilter"
    )]
    UpdateFeedbackDIDFilter {
        #[serde(default, alias = "devices", alias = "device_ids")]
        devices: Vec<String>,
    },

    /// Add (idempotently) one or more stream/topic IDs this client wants to receive.
    /// Streams are created if they do not already exist.
    #[serde(
        alias = "UpdateStreamRegistration",
        alias = "register_streams",
        alias = "RegisterStreams"
    )]
    UpdateStreamRegistration {
        #[serde(default, alias = "streams")]
        streams: Vec<String>,
    },

    /// Query current union of all known registered stream IDs. (No mutation.)
    #[serde(alias = "ListStreams", alias = "listStreams")] // additional safety
    ListStreams,

    #[serde(alias = "GetConfig", alias = "get_config", alias = "getConfig")]
    GetConfig,
}

/// Inbound message into the unified I/O processing pipeline
#[derive(Debug, Clone)]
pub enum IoInboundMessage {
    /// Input events produced by a client
    Input {
        client_id: String,
        packet: InputEventPacket,
    },
    /// Feedback events produced by a client (to be re-broadcast / routed)
    Feedback {
        client_id: String,
        packet: FeedbackEventPacket,
    },
    /// Control messages sent from clients to the server
    Control {
        client_id: String,
        msg: ClientControlMessage,
    },
}

/// Outbound messages emitted by the IoEventServer after processing
#[derive(Debug, Clone)]
pub enum IoOutboundMessage {
    /// Fully processed (atomic) input packet ready for routing / device filter
    Input(InputEventPacket),
    /// Feedback packet originating from any client (already broadcast internally)
    Feedback(FeedbackEventPacket),
}

/// Lightweight envelope used by transports to detect a control message before
/// deserializing into the strongly-typed `ClientControlMessage`.
/// JSON shape: {"type":"control", <command-specific fields> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub inner: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoErrorPacket {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Clone)]
/// Outbound messages to clients. NOTE: Serialization behavior:
/// - Feedback: serialized as the raw `FeedbackEventPacket` (backwards compatibility with
///   pre-unified format; no {"type":"Feedback","data":...} wrapper)
/// - Other variants (Input, Error, ControlAck): serialized as
///   {"type": <Variant>, "data": <payload>} matching the previous tagged format.
pub enum IoClientMessage {
    Feedback(FeedbackEventPacket),
    /// Stream/topic routed input packet
    Input(InputEventPacket),
    Error(IoErrorPacket),
    ControlAck(ControlAckPacket),
    /// Full configuration snapshot (response to GetConfig control)
    Config(Box<crate::config::AppConfig>),
}

impl serde::Serialize for IoClientMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        match self {
            // Raw packet (legacy shape expected by existing clients)
            IoClientMessage::Feedback(pkt) => pkt.serialize(serializer),
            IoClientMessage::Input(pkt) => {
                let mut st = serializer.serialize_struct("IoClientMessage", 2)?;
                st.serialize_field("type", "Input")?;
                st.serialize_field("data", pkt)?;
                st.end()
            }
            IoClientMessage::Error(err) => {
                let mut st = serializer.serialize_struct("IoClientMessage", 2)?;
                st.serialize_field("type", "Error")?;
                st.serialize_field("data", err)?;
                st.end()
            }
            IoClientMessage::ControlAck(ack) => {
                let mut st = serializer.serialize_struct("IoClientMessage", 2)?;
                st.serialize_field("type", "control_ack")?; // keep snake_case naming
                st.serialize_field("data", ack)?;
                st.end()
            }
            IoClientMessage::Config(cfg) => {
                let mut st = serializer.serialize_struct("IoClientMessage", 2)?;
                st.serialize_field("type", "config")?;
                st.serialize_field("data", cfg)?;
                st.end()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlAckPacket {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<String>>, // for *_did_filter updates
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_streams: Option<Vec<String>>, // for stream registration updates
}

#[derive(Clone)]
struct ClientEntry {
    settings: ClientSettings,
    outbound_tx: mpsc::UnboundedSender<IoClientMessage>,
}

pub struct IoEventServer {
    pub atomic_processor: Arc<AtomicInputProcessor>,
    // Inbound (unprocessed) messages from transports / plugins
    inbound_tx: mpsc::UnboundedSender<IoInboundMessage>,
    inbound_rx: Arc<Mutex<mpsc::UnboundedReceiver<IoInboundMessage>>>,
    // Unified outbound channel (processed input & feedback)
    outbound_tx: mpsc::UnboundedSender<IoOutboundMessage>,
    outbound_rx: Arc<Mutex<mpsc::UnboundedReceiver<IoOutboundMessage>>>,
    clients: Arc<RwLock<HashMap<String, ClientEntry>>>,
    app_config: Arc<crate::config::AppConfig>,
}

impl IoEventServer {
    /// Create a new unified I/O event server
    pub fn new() -> Self {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        Self {
            atomic_processor: Arc::new(AtomicInputProcessor::new(Default::default())),
            inbound_tx,
            inbound_rx: Arc::new(Mutex::new(inbound_rx)),
            outbound_tx,
            outbound_rx: Arc::new(Mutex::new(outbound_rx)),
            clients: Arc::new(RwLock::new(HashMap::new())),
            app_config: Arc::new(Default::default()),
        }
    }

    /// Create with explicit config (used by backend)
    pub fn with_config(cfg: Arc<crate::config::AppConfig>) -> Self {
        let mut s = Self::new();
        s.app_config = cfg;
        s
    }

    /// Get a clone of the inbound sender so transport backends can submit messages
    pub fn inbound_sender(&self) -> mpsc::UnboundedSender<IoInboundMessage> {
        self.inbound_tx.clone()
    }

    /// Register a client with optional settings, returning a receiver for outbound feedback
    pub async fn register_client(
        &self,
        client: ClientNode,
    ) -> mpsc::UnboundedReceiver<IoClientMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut guard = self.clients.write().await;
        guard.insert(
            client.id.clone(),
            ClientEntry {
                settings: client.client_settings,
                outbound_tx: tx,
            },
        );
        info!(client_id = %client.id, "Registered client to IoEventServer");
        rx
    }

    /// Internal: process a single inbound input packet (after atomic processing)
    async fn handle_input_packet(&self, client_id: &str, mut packet: InputEventPacket) {
        debug!(client_id, device_id = %packet.device_id, events = packet.events.len(), "Received inbound input packet");

        let original_device_id = packet.device_id.clone();
        packet.device_id = format!("{original_device_id}@{client_id}");

        match self.atomic_processor.process_packet(packet).await {
            Ok(Some(mut atomic_packet)) => {
                // Restore original device id (strip suffix)
                if let Some(at_pos) = atomic_packet.device_id.rfind('@') {
                    atomic_packet.device_id = atomic_packet.device_id[..at_pos].to_string();
                } else {
                    // fallback – should not happen
                    atomic_packet.device_id = original_device_id;
                }
                if let Err(e) = self
                    .outbound_tx
                    .send(IoOutboundMessage::Input(atomic_packet))
                {
                    error!(client_id, error = %e, "Failed to enqueue processed input packet");
                }
            }
            Ok(None) => {
                debug!(client_id, "Packet buffered for atomic processing");
            }
            Err(e) => {
                warn!(client_id, error = ?e, "Failed to process input packet");
            }
        }
    }

    /// Internal: forward feedback packet produced by a client into main feedback stream
    fn handle_feedback_packet(&self, client_id: &str, packet: FeedbackEventPacket) {
        debug!(client_id, device_id = %packet.device_id, events = packet.events.len(), "Received inbound feedback packet");
        // Push to unified outbound queue
        if let Err(e) = self
            .outbound_tx
            .send(IoOutboundMessage::Feedback(packet.clone()))
        {
            error!(client_id, error = %e, "Failed to enqueue feedback packet");
        }
        // Also dispatch to registered clients honoring filters
        // spawn task to avoid blocking caller (which may be in the main loop)
        let clients_arc = self.clients.clone();
        tokio::spawn(async move {
            let clients_snapshot = { clients_arc.read().await.clone() };
            for (cid, entry) in clients_snapshot.iter() {
                if entry.settings.write_only {
                    continue;
                }
                if !entry.settings.feedback_listener_device_ids.is_empty()
                    && !entry
                        .settings
                        .feedback_listener_device_ids
                        .contains(&packet.device_id)
                {
                    continue;
                }
                let _ = entry
                    .outbound_tx
                    .send(IoClientMessage::Feedback(packet.clone()));
                debug!(client_id = %cid, "Delivered feedback packet to client");
            }
        });
    }
    /// Handle a control message (mutates client subscription state)
    async fn handle_control_message(&self, client_id: &str, msg: ClientControlMessage) {
        use ClientControlMessage::*;
        let mut clients = self.clients.write().await;
        if let Some(entry) = clients.get_mut(client_id) {
            match &msg {
                StopFeedback => {
                    entry.settings.feedback_listener_device_ids.clear();
                    entry.settings.write_only = true;
                }
                StopInput => {
                    entry.settings.input_listener_device_ids.clear();
                }
                UpdateInputDIDFilter { devices } => {
                    entry.settings.input_listener_device_ids = devices.clone();
                }
                UpdateFeedbackDIDFilter { devices } => {
                    entry.settings.feedback_listener_device_ids = devices.clone();
                    entry.settings.write_only = false; // ensure enabled
                }
                UpdateStreamRegistration { streams } => {
                    // Declarative replacement with dedup
                    let set: std::collections::BTreeSet<String> = streams.iter().cloned().collect();
                    entry.settings.registered_stream_ids = set.into_iter().collect();
                }
                ListStreams => {
                    // No-op: handled at transport layer (returns current list)
                }
                ClientControlMessage::GetConfig => {
                    // Non-mutating: send config snapshot
                    let cfg = (*self.app_config).clone();
                    let _ = entry.outbound_tx.send(IoClientMessage::Config(Box::new(cfg)));
                }
            }
            // Emit centralized control_ack for mutating commands (excluding ListStreams)
            let ack_cmd = match &msg {
                StopFeedback => Some(("stop_feedback", None, None)),
                StopInput => Some(("stop_input", None, None)),
                UpdateInputDIDFilter { devices } => {
                    Some(("update_input_did_filter", Some(devices.clone()), None))
                }
                UpdateFeedbackDIDFilter { devices } => {
                    Some(("update_feedback_did_filter", Some(devices.clone()), None))
                }
                UpdateStreamRegistration { .. } => Some((
                    "update_stream_registration",
                    None,
                    Some(entry.settings.registered_stream_ids.clone()),
                )),
                ListStreams => None,
                ClientControlMessage::GetConfig => None,
            };
            if let Some((cmd, devices_opt, streams_opt)) = ack_cmd {
                let _ = entry
                    .outbound_tx
                    .send(IoClientMessage::ControlAck(ControlAckPacket {
                        cmd: cmd.to_string(),
                        devices: devices_opt,
                        registered_streams: streams_opt,
                    }));
            }
            debug!(client_id = %client_id, ?entry.settings.registered_stream_ids, "Applied control message");
        } else {
            warn!(client_id = %client_id, "Control message for unknown client");
        }
    }

    /// Broadcast input packet to clients subscribed to a stream id
    pub fn broadcast_stream_input(&self, stream_id: &str, packet: &InputEventPacket) {
        let stream_id = stream_id.to_string();
        let packet = packet.clone();
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let snapshot = { clients.read().await.clone() };
            for (cid, entry) in snapshot.iter() {
                if entry.settings.registered_stream_ids.contains(&stream_id) {
                    let _ = entry
                        .outbound_tx
                        .send(IoClientMessage::Input(packet.clone()));
                    debug!(client_id = %cid, stream=%stream_id, "Delivered stream input packet");
                }
            }
        });
    }
    /// Broadcast input packet to clients that have subscribed by device id (input_listener_device_ids)
    ///
    /// Precedence rule: if a client has registered any stream IDs, we ONLY deliver packets that
    /// match those streams via `broadcast_stream_input` (topic-based). If the client has not
    /// registered streams (empty `registered_stream_ids`), then device ID filters apply:
    ///  * Empty `input_listener_device_ids` => receive NO input (default lockdown)
    ///  * Non-empty => receive only listed device IDs (whitelist)
    pub fn broadcast_device_input(&self, packet: &InputEventPacket) {
        let packet = packet.clone();
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let snapshot = { clients.read().await.clone() };
            for (_cid, entry) in snapshot.iter() {
                // Skip clients that chose stream/topic registration path
                if !entry.settings.registered_stream_ids.is_empty() {
                    continue;
                }
                // Device filter evaluation (empty list -> no subscription)
                if entry.settings.input_listener_device_ids.is_empty() {
                    continue;
                }
                if !entry
                    .settings
                    .input_listener_device_ids
                    .contains(&packet.device_id)
                {
                    continue;
                }
                let _ = entry
                    .outbound_tx
                    .send(IoClientMessage::Input(packet.clone()));
                // debug!(client_id = %cid, device=%packet.device_id, "Delivered device input packet");
            }
        });
    }

    /// Send an AppError directly (automatically maps to client-facing code/message).
    pub fn send_app_error(&self, client_id: &str, error: AppError) {
        let cf: ClientFacingError = error.into();
        self.dispatch_client_error(client_id, cf);
    }

    fn dispatch_client_error(&self, client_id: &str, cf: ClientFacingError) {
        let client_id_str = client_id.to_string();
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let guard = clients.read().await;
            if let Some(entry) = guard.get(&client_id_str) {
                let _ = entry
                    .outbound_tx
                    .send(IoClientMessage::Error(IoErrorPacket {
                        code: cf.code,
                        message: cf.message,
                        raw: cf.raw,
                    }));
            } else {
                warn!(client_id = %client_id_str, "Attempted to send error to unknown client");
            }
        });
    }
    /// Take exclusive ownership of the unified outbound receiver.
    /// (Single-consumer model. Call once.)
    pub async fn take_outbound_receiver(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<IoOutboundMessage>> {
        let mut guard = self.outbound_rx.lock().await;
        // Replace with a fresh dummy receiver to avoid accidental reuse
        let (dummy_tx, dummy_rx) = mpsc::unbounded_channel::<IoOutboundMessage>();
        let original = std::mem::replace(&mut *guard, dummy_rx);
        drop(dummy_tx); // dummy_tx dropped immediately
        Some(original)
    }

    /// Returns current number of registered clients
    #[cfg(test)]
    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// List all currently registered stream/topic IDs across clients
    pub async fn list_stream_ids(&self) -> Vec<String> {
        let guard = self.clients.read().await;
        let mut set = std::collections::BTreeSet::new();
        for (_cid, entry) in guard.iter() {
            for s in &entry.settings.registered_stream_ids {
                set.insert(s.clone());
            }
        }
        set.into_iter().collect()
    }
}

#[async_trait::async_trait]
impl InputBackend for IoEventServer {
    async fn run(&mut self) -> eyre::Result<()> {
        info!("Starting IoEventServer unified processing loop");

        // Start atomic processor flush timer -> enqueue batches
        let mut flush_timer_rx = self.atomic_processor.start_flush_timer();
        let outbound_tx = self.outbound_tx.clone();
        tokio::spawn(async move {
            while let Some(mut atomic_packet) = flush_timer_rx.recv().await {
                if let Some(at_pos) = atomic_packet.device_id.rfind('@') {
                    atomic_packet.device_id = atomic_packet.device_id[..at_pos].to_string();
                }
                if let Err(e) = outbound_tx.send(IoOutboundMessage::Input(atomic_packet)) {
                    error!(error = %e, "Failed to enqueue atomic batch from flush timer");
                }
            }
            debug!("Atomic processor flush task stopped (IoEventServer)");
        });

        // Main inbound processing loop
        loop {
            let next_msg = {
                let mut rx = self.inbound_rx.lock().await;
                rx.recv().await
            };
            match next_msg {
                Some(IoInboundMessage::Input { client_id, packet }) => {
                    self.handle_input_packet(&client_id, packet).await;
                }
                Some(IoInboundMessage::Feedback { client_id, packet }) => {
                    self.handle_feedback_packet(&client_id, packet);
                }
                Some(IoInboundMessage::Control { client_id, msg }) => {
                    self.handle_control_message(&client_id, msg).await;
                }
                None => {
                    warn!("Inbound channel closed; IoEventServer stopping");
                    break;
                }
            }
        }

        Ok(())
    }
}

impl IoEventServer {
    /// Same as `run` but takes &self so it can be used behind Arc in transports.
    pub async fn run_forever(&self) -> eyre::Result<()> {
        info!("Starting IoEventServer unified processing loop (run_forever)");
        // Start atomic processor flush timer -> enqueue batches
        let mut flush_timer_rx = self.atomic_processor.start_flush_timer();
        let outbound_tx = self.outbound_tx.clone();
        tokio::spawn(async move {
            while let Some(mut atomic_packet) = flush_timer_rx.recv().await {
                if let Some(at_pos) = atomic_packet.device_id.rfind('@') {
                    atomic_packet.device_id = atomic_packet.device_id[..at_pos].to_string();
                }
                if let Err(e) = outbound_tx.send(IoOutboundMessage::Input(atomic_packet)) {
                    error!(error = %e, "Failed to enqueue atomic batch from flush timer");
                }
            }
            debug!("Atomic processor flush task stopped (IoEventServer run_forever)");
        });

        loop {
            let next_msg = {
                let mut rx = self.inbound_rx.lock().await;
                rx.recv().await
            };
            match next_msg {
                Some(IoInboundMessage::Input { client_id, packet }) => {
                    self.handle_input_packet(&client_id, packet).await;
                }
                Some(IoInboundMessage::Feedback { client_id, packet }) => {
                    self.handle_feedback_packet(&client_id, packet);
                }
                Some(IoInboundMessage::Control { client_id, msg }) => {
                    self.handle_control_message(&client_id, msg).await;
                }
                None => {
                    warn!("Inbound channel closed; IoEventServer stopping (run_forever)");
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{FeedbackEvent, FeedbackEventPacket, LedEvent, LedPattern};
    use crate::input::{InputEvent, InputEventPacket, KeyboardEvent};
    use tokio::time::{Duration, timeout};

    fn make_input_packet(device: &str, ts: u64) -> InputEventPacket {
        InputEventPacket {
            device_id: device.to_string(),
            timestamp: ts,
            events: vec![InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "KEY_A".into(),
            })],
        }
    }

    fn make_feedback_packet(device: &str, ts: u64) -> FeedbackEventPacket {
        FeedbackEventPacket {
            device_id: device.to_string(),
            timestamp: ts,
            events: vec![FeedbackEvent::Led(LedEvent::SetPattern {
                led_id: 1,
                pattern: LedPattern::Solid,
            })],
        }
    }

    #[tokio::test]
    async fn test_client_registration_and_error_delivery() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });

        let mut rx = server
            .register_client(ClientNode {
                id: "client1".into(),
                client_settings: ClientSettings::default(),
            })
            .await;
        assert_eq!(server.client_count().await, 1);

        // Send an error
        server.send_app_error("client1", AppError::Internal("Something went wrong".into()));
        let msg = timeout(Duration::from_millis(200), async { rx.recv().await })
            .await
            .expect("timed out waiting for error")
            .expect("channel closed");
        match msg {
            IoClientMessage::Error(err) => {
                // Accept mapped error code/message from AppError::Internal
                assert_eq!(err.code, "internal_error");
                assert!(
                    err.message == "internal error: Something went wrong"
                        || err.message.contains("Something went wrong"),
                    "unexpected error message: {}",
                    err.message
                );
                assert_eq!(err.raw, None);
            }
            _ => panic!("expected error message"),
        }
    }

    #[tokio::test]
    async fn test_input_packet_flow() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });
        // Register producer
        let _producer_rx = server
            .register_client(ClientNode {
                id: "producer".into(),
                client_settings: ClientSettings::default(),
            })
            .await;

        // Take outbound receiver (processed output)
        let mut outbound_rx = server.take_outbound_receiver().await.unwrap();
        let inbound = server.inbound_sender();
        inbound
            .send(IoInboundMessage::Input {
                client_id: "producer".into(),
                packet: make_input_packet("dev1", 10),
            })
            .unwrap();

        let msg = timeout(Duration::from_millis(100), async {
            outbound_rx.recv().await
        })
        .await
        .expect("timeout")
        .expect("channel closed");
        match msg {
            IoOutboundMessage::Input(pkt) => {
                assert_eq!(pkt.device_id, "dev1"); // original restored
                assert_eq!(pkt.events.len(), 1);
                assert_eq!(pkt.timestamp, 10);
            }
            _ => panic!("expected input outbound packet"),
        }
    }

    #[tokio::test]
    async fn test_out_of_order_dropped() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });
        let _rx = server
            .register_client(ClientNode {
                id: "producer".into(),
                client_settings: ClientSettings::default(),
            })
            .await;
        let mut outbound_rx = server.take_outbound_receiver().await.unwrap();
        let inbound = server.inbound_sender();
        // Send newer timestamp first
        inbound
            .send(IoInboundMessage::Input {
                client_id: "producer".into(),
                packet: make_input_packet("devX", 200),
            })
            .unwrap();
        // Then older
        inbound
            .send(IoInboundMessage::Input {
                client_id: "producer".into(),
                packet: make_input_packet("devX", 100),
            })
            .unwrap();

        // Expect only the first packet to emerge
        let first = timeout(Duration::from_millis(100), async {
            outbound_rx.recv().await
        })
        .await
        .expect("timeout")
        .expect("channel closed");
        match first {
            IoOutboundMessage::Input(pkt) => assert_eq!(pkt.timestamp, 200),
            _ => panic!("expected input packet"),
        }
        // Ensure no second packet within window
        let second = timeout(Duration::from_millis(80), async {
            outbound_rx.recv().await
        })
        .await;
        assert!(
            second.is_err(),
            "unexpected second packet emitted for out-of-order input"
        );
    }

    #[tokio::test]
    async fn test_feedback_broadcast_and_filters() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });
        // Producer client
        let _producer_rx = server
            .register_client(ClientNode {
                id: "producer".into(),
                client_settings: ClientSettings::default(),
            })
            .await;
        // Default listener (should receive)
        let mut default_rx = server
            .register_client(ClientNode {
                id: "listener_default".into(),
                client_settings: ClientSettings::default(),
            })
            .await;
        // Write-only client (should NOT receive)
        let mut write_only_rx = server
            .register_client(ClientNode {
                id: "listener_writeonly".into(),
                client_settings: ClientSettings {
                    write_only: true,
                    ..Default::default()
                },
            })
            .await;
        // Filter mismatch client (subscribes only to other_device)
        let mut mismatch_rx = server
            .register_client(ClientNode {
                id: "listener_mismatch".into(),
                client_settings: ClientSettings {
                    feedback_listener_device_ids: vec!["other_device".into()],
                    ..Default::default()
                },
            })
            .await;
        // Filter match client (subscribes to leddev)
        let mut match_rx = server
            .register_client(ClientNode {
                id: "listener_match".into(),
                client_settings: ClientSettings {
                    feedback_listener_device_ids: vec!["leddev".into()],
                    ..Default::default()
                },
            })
            .await;

        let inbound = server.inbound_sender();
        inbound
            .send(IoInboundMessage::Feedback {
                client_id: "producer".into(),
                packet: make_feedback_packet("leddev", 1),
            })
            .unwrap();

        // helper closure to try receive
        async fn expect_feedback(rx: &mut mpsc::UnboundedReceiver<IoClientMessage>, should: bool) {
            if should {
                let msg = timeout(Duration::from_millis(120), async { rx.recv().await })
                    .await
                    .expect("timeout waiting for feedback")
                    .expect("channel closed");
                match msg {
                    IoClientMessage::Feedback(pkt) => assert_eq!(pkt.device_id, "leddev"),
                    other => panic!("expected feedback, got {other:?}"),
                }
            } else {
                let res = timeout(Duration::from_millis(80), async { rx.recv().await }).await;
                assert!(res.is_err(), "received unexpected feedback packet");
            }
        }

        expect_feedback(&mut default_rx, true).await;
        expect_feedback(&mut write_only_rx, false).await;
        expect_feedback(&mut mismatch_rx, false).await;
        expect_feedback(&mut match_rx, true).await;
    }

    #[tokio::test]
    async fn test_device_input_broadcast_precedence() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });

        // Client A: device listener (no streams, listens to specific device devZ)
        let mut dev_listener_rx = server
            .register_client(ClientNode {
                id: "dev_listener".into(),
                client_settings: ClientSettings {
                    input_listener_device_ids: vec!["devZ".into()],
                    ..Default::default()
                },
            })
            .await;
        // Client B: empty list (should receive NONE now under new semantics)
        let mut empty_rx = server
            .register_client(ClientNode {
                id: "empty".into(),
                client_settings: ClientSettings::default(),
            })
            .await;
        // Client C: stream subscriber (registered_stream_ids non-empty, should NOT receive via device listener path)
        let mut stream_client_rx = server
            .register_client(ClientNode {
                id: "stream_sub".into(),
                client_settings: ClientSettings {
                    registered_stream_ids: vec!["uinput".into()],
                    ..Default::default()
                },
            })
            .await;

        // Simulate routing emission: directly call broadcast_device_input (mimic routing path)
        let pkt = InputEventPacket {
            device_id: "devZ".into(),
            timestamp: 1,
            events: vec![],
        };
        server.broadcast_device_input(&pkt);

        // Expect A to receive, B (empty) NOT to, C not to (since precedence excludes device path)
        let a = timeout(Duration::from_millis(120), async {
            dev_listener_rx.recv().await
        })
        .await
        .expect("timeout A")
        .expect("A closed");
        match a {
            IoClientMessage::Input(p) => assert_eq!(p.device_id, "devZ"),
            other => panic!("expected input for A got {other:?}"),
        }
        let b = timeout(Duration::from_millis(80), async { empty_rx.recv().await }).await;
        assert!(
            b.is_err(),
            "empty filter client unexpectedly received input"
        );
        let c = timeout(Duration::from_millis(80), async {
            stream_client_rx.recv().await
        })
        .await; // should timeout
        assert!(
            c.is_err(),
            "stream subscriber unexpectedly received device-broadcast input"
        );
    }

    #[tokio::test]
    async fn test_device_input_requires_opt_in() {
        let server = Arc::new(IoEventServer::new());
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });

        // Client with explicit opt-in
        let mut listener_rx = server
            .register_client(ClientNode {
                id: "listener".into(),
                client_settings: ClientSettings {
                    input_listener_device_ids: vec!["devA".into()],
                    ..Default::default()
                },
            })
            .await;
        // Client with empty list (default)
        let mut default_rx = server
            .register_client(ClientNode {
                id: "default".into(),
                client_settings: ClientSettings::default(),
            })
            .await;

        let pkt = InputEventPacket {
            device_id: "devA".into(),
            timestamp: 1,
            events: vec![],
        };
        server.broadcast_device_input(&pkt);

        // Opt-in client should receive
        let got = timeout(Duration::from_millis(120), async {
            listener_rx.recv().await
        })
        .await
        .expect("timeout listener")
        .expect("listener closed");
        assert!(
            matches!(got, IoClientMessage::Input(_)),
            "expected Input message"
        );

        // Default client should not
        let res = timeout(Duration::from_millis(80), async { default_rx.recv().await }).await;
        assert!(res.is_err(), "default client received input without opt-in");
    }

    #[tokio::test]
    async fn test_get_config_control_message() {
        let mut cfg = crate::config::AppConfig::default();
        cfg.output.uinput.enabled = false; // mutate something observable
        let server = Arc::new(IoEventServer::with_config(Arc::new(cfg.clone())));
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run_forever().await;
        });

        let mut rx = server
            .register_client(ClientNode {
                id: "client_cfg".into(),
                client_settings: ClientSettings::default(),
            })
            .await;

        // Send GetConfig control message
        server
            .inbound_sender()
            .send(IoInboundMessage::Control {
                client_id: "client_cfg".into(),
                msg: ClientControlMessage::GetConfig,
            })
            .unwrap();

        use tokio::time::{Duration, timeout};
        let msg = timeout(Duration::from_millis(200), async { rx.recv().await })
            .await
            .expect("timeout waiting for config")
            .expect("channel closed");
        match msg {
            IoClientMessage::Config(received) => {
                assert_eq!(received.output.uinput.enabled, false);
            }
            other => panic!("expected Config message, got {other:?}"),
        }
    }
}
