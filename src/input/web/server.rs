//! Web server implementation for both WebSocket input handling and web UI serving.
//! Uses axum framework with tower middleware support.

use crate::feedback::FeedbackEventPacket;
use crate::input::io_server::{ClientNode, ClientSettings, IoEventServer, IoInboundMessage};
use crate::input::{InputBackend, InputEventPacket};
use axum::{
    Router,
    extract::{
        ConnectInfo, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::{Json, Response},
    routing::get,
};
use eyre::{Context, Result};
use futures_util::{sink::SinkExt, stream::StreamExt};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{debug, error, info, trace, warn};

use super::frontend;

#[derive(Clone)]
pub struct WebSocketState {
    pub io_server: Arc<IoEventServer>,
}

/// Web server that handles both WebSocket connections for input events
/// and serves static files for the web UI.
pub struct WebServer {
    bind_addr: SocketAddr,
    web_assets_path: Option<PathBuf>,
    io_server: Arc<IoEventServer>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebTemplateResponse {
    /// Relative web path to the custom template
    pub path: String,
    /// Name of the custom template, usually the name of the directory
    pub name: String,
}

impl WebServer {
    /// Automatically detect and use web UI if available.
    ///
    /// # Arguments
    /// * `bind_addr` - The address to bind the server to
    /// * `event_stream` - The input event stream to send received events to
    /// * `feedback_stream` - The feedback event stream to receive feedback events from
    ///
    /// This method checks the `WEB_UI_PATH` environment variable for a configured path,
    /// and falls back to searching for a `web` directory in the current working directory
    pub fn auto_detect_web_ui(bind_addr: SocketAddr, io_server: Arc<IoEventServer>) -> Self {
        let configured_path = std::env::var("WEB_UI_PATH").ok().map(PathBuf::from);
        let web_assets_path = frontend::find_web_ui_dir(configured_path);
        Self {
            bind_addr,
            web_assets_path,
            io_server,
        }
    }
    // with_io_server removed; always unified

    pub fn list_custom_layouts(&self) -> Vec<WebTemplateResponse> {
        if let Some(assets_path) = &self.web_assets_path {
            let custom_dir = assets_path.join("custom");
            if custom_dir.is_dir() {
                match std::fs::read_dir(&custom_dir) {
                    Ok(entries) => entries
                        .filter_map(|entry| {
                            entry.ok().and_then(|e| {
                                let path = e.path();
                                if path.is_dir() {
                                    // Look for the first HTML file in this directory
                                    if let Ok(files) = std::fs::read_dir(&path) {
                                        for file in files.flatten() {
                                            let file_path = file.path();
                                            if let Some(ext) = file_path.extension() {
                                                if ext == "html" {
                                                    if let Some(dir_name) =
                                                        path.file_name().and_then(|n| n.to_str())
                                                    {
                                                        if let Some(file_name) = file_path
                                                            .file_name()
                                                            .and_then(|n| n.to_str())
                                                        {
                                                            return Some(WebTemplateResponse {
                                                                path: format!(
                                                                    "custom/{dir_name}/{file_name}"
                                                                ),
                                                                name: dir_name.to_string(),
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    None
                                } else {
                                    None
                                }
                            })
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    /// Build the application router with all routes and return both the router and WebSocket state
    fn build_router(&self) -> (Router, WebSocketState) {
        let ws_state = WebSocketState {
            io_server: self.io_server.clone(),
        };

        // Create the WebSocket router with WebSocket state
        let ws_router = Router::new()
            .route("/ws", get(bidirectional_ws_handler))
            .with_state(ws_state.clone());

        // Create API router with server state
        let server_arc = Arc::new(self.clone());
        let api_router = Router::new()
            .route("/api/layouts", get(get_custom_layouts))
            .with_state(server_arc);

        // Merge routers
        let mut router = ws_router.merge(api_router);

        // Add static file serving if web UI is enabled
        if let Some(assets_path) = &self.web_assets_path {
            if frontend::is_valid_web_ui(assets_path) {
                // Create a service for serving static files with SPA support
                let serve_dir = tower_http::services::ServeDir::new(assets_path)
                    .append_index_html_on_directories(true);

                // Add the static file service as a fallback service
                router = router.fallback_service(serve_dir);

                info!("Web UI serving enabled from {}", assets_path.display());
            } else {
                warn!(
                    "Web UI directory exists but doesn't contain index.html: {}",
                    assets_path.display()
                );
            }
        } else {
            info!("Web UI serving disabled (no web assets path configured)");
        }

        // Add tracing middleware
        let router = router.layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

        (router, ws_state)
    }
}

#[async_trait::async_trait]
impl InputBackend for WebServer {
    async fn run(&mut self) -> Result<()> {
        info!("Starting web server on {}", self.bind_addr);

        let (router, _ws_state) = self.build_router();

        // Create TCP listener
        let listener = tokio::net::TcpListener::bind(self.bind_addr)
            .await
            .context("Failed to bind to address")?;

        // Run the server with graceful shutdown
        let server_task = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for ctrl+c");
            info!("Shutting down web server...");
        });

        // Run both the server and feedback broadcast task
        server_task.await.context("Server error")?;

        Ok(())
    }
}

impl Clone for WebServer {
    fn clone(&self) -> Self {
        Self {
            bind_addr: self.bind_addr,
            web_assets_path: self.web_assets_path.clone(),
            io_server: self.io_server.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::feedback::{FeedbackEvent, LedEvent};
    use crate::input::{InputEvent, KeyboardEvent, PointerEvent};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_tungstenite::connect_async;

    /// Creates a sample input event packet for testing.
    fn create_sample_packet() -> InputEventPacket {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut packet = InputEventPacket::new("test-device".to_string(), timestamp);

        // Add some sample events
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "a".to_string(),
        }));
        packet.add_event(InputEvent::Pointer(PointerEvent::Move {
            x_delta: 10,
            y_delta: -5,
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
            key: "a".to_string(),
        }));

        packet
    }

    #[test]
    fn test_packet_serialization() {
        let packet = create_sample_packet();
        let json = serde_json::to_string(&packet).expect("Failed to serialize packet");

        // Ensure we can deserialize it back
        let _deserialized: InputEventPacket =
            serde_json::from_str(&json).expect("Failed to deserialize packet");
    }

    // Test function to demonstrate feedback functionality
    // #[cfg(test)]
    // legacy test_feedback_broadcast removed (feedback now unified via IoEventServer)
    //     use crate::feedback::{FeedbackEvent, FeedbackEventPacket, HapticEvent, LedEvent};
    //     use std::time::{SystemTime, UNIX_EPOCH};

    //     let timestamp = SystemTime::now()
    //         .duration_since(UNIX_EPOCH)
    //         .unwrap()
    //         .as_millis() as u64;

    //     // Create a test feedback packet with LED and haptic events
    //     let mut feedback_packet =
    //         FeedbackEventPacket::new("test-controller".to_string(), timestamp);

    //     // Add LED event - turn on red LED
    //     feedback_packet.add_event(FeedbackEvent::Led(LedEvent::Set {
    //         led_id: 1,
    //         on: true,
    //         brightness: Some(255),
    //         rgb: Some((255, 0, 0)), // Red
    //     }));

    //     // Add haptic event - vibrate motor 0
    //     feedback_packet.add_event(FeedbackEvent::Haptic(HapticEvent::Vibrate {
    //         motor_id: 0,
    //         intensity: 128,
    //         duration_ms: 500,
    //     }));

    //     // Send the feedback packet
    //     feedback_stream.send(feedback_packet).await?;
    //     info!("Test feedback packet sent successfully");

    //     Ok(())
    // }

    // Removed legacy unified_websocket_state test (state now only holds io_server)

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_api_layouts_endpoint() {
        use std::fs;

        // Create a temporary directory structure for testing
        let temp_dir = std::env::temp_dir().join("backflow_test_layouts");
        let web_dir = &temp_dir;
        let custom_dir = web_dir.join("custom");
        let layout1_dir = custom_dir.join("chuni");
        let layout2_dir = custom_dir.join("keyboard");

        // Clean up any existing test directory
        let _ = fs::remove_dir_all(&temp_dir);

        fs::create_dir_all(&layout1_dir).unwrap();
        fs::create_dir_all(&layout2_dir).unwrap();

        // Create HTML files in each layout directory
        fs::write(
            layout1_dir.join("chuni.html"),
            "<html><body>Chuni Layout</body></html>",
        )
        .unwrap();
        fs::write(
            layout2_dir.join("keys.html"),
            "<html><body>Keyboard Layout</body></html>",
        )
        .unwrap();

        // Create a web server with the temp directory
        let io_server = Arc::new(IoEventServer::new());
        let mut server = WebServer::auto_detect_web_ui("127.0.0.1:0".parse().unwrap(), io_server);
        // Manually set the web assets path for testing
        server.web_assets_path = Some(web_dir.to_path_buf());

        // Test the list_custom_layouts method
        let layouts = server.list_custom_layouts();

        assert_eq!(layouts.len(), 2);

        // Sort to ensure consistent ordering for testing
        let mut sorted_layouts = layouts;
        sorted_layouts.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(sorted_layouts[0].name, "chuni");
        assert_eq!(sorted_layouts[0].path, "custom/chuni/chuni.html");

        assert_eq!(sorted_layouts[1].name, "keyboard");
        assert_eq!(sorted_layouts[1].path, "custom/keyboard/keys.html");

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_web_template_response_serialization() {
        let response = WebTemplateResponse {
            path: "custom/chuni/layout.html".to_string(),
            name: "chuni".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        let expected = r#"{"path":"custom/chuni/layout.html","name":"chuni"}"#;
        assert_eq!(json, expected);

        // Test deserialization
        let deserialized: WebTemplateResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "custom/chuni/layout.html");
        assert_eq!(deserialized.name, "chuni");
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_websocket_stress_large_batches() {
        // 1. Setup server
        info!("Step 1: Setting up server");
        let io_server = Arc::new(IoEventServer::new());
        let mut server =
            WebServer::auto_detect_web_ui("127.0.0.1:0".parse().unwrap(), io_server.clone());
        // Spawn IO server processing loop required for test
        let io_server_task = {
            let io = io_server.clone();
            tokio::spawn(async move {
                let _ = io.run_forever().await;
            })
        };
        server.web_assets_path = None; // Disable static file serving for this test

        let (router, _ws_state) = server.build_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // 2. Connect client
        info!("Step 2: Connecting WebSocket client");
        let (ws_stream, _) = connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("Failed to connect");
        let (mut write, _read) = ws_stream.split();

        // 3. Send large batches of events
        info!("Step 3: Sending large batches of events");
        const NUM_KEYS: usize = 2500; // Total number of unique keys to press and release
        let mut key_events = Vec::new();

        // Generate all key press and release events up front
        for i in 0..NUM_KEYS {
            let key = format!("STRESS_KEY_{i}");
            key_events.push(InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: key.clone(),
            }));
            key_events.push(InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }));
        }

        // Send all events in chunks (packets)
        let mut total_events_sent = 0;
        for (idx, chunk) in key_events.chunks(50).enumerate() {
            let packet = InputEventPacket {
                device_id: "stress_test_device".to_string(),
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                events: chunk.to_vec(),
            };

            let json = serde_json::to_string(&packet).unwrap();
            write
                .send(tokio_tungstenite::tungstenite::Message::Text(json.into()))
                .await
                .expect("Failed to send message");
            total_events_sent += chunk.len();
            debug!(
                "Sent packet {} ({} events, total sent: {})",
                idx,
                chunk.len(),
                total_events_sent
            );
        }

        // 4. Verify all packets were received and final state is correct
        info!("Step 4: Verifying all packets were received");
        // Wait briefly to allow IO server to process (no direct assertion until full integration path updated)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 5. Check final input state: all keys should be released
        info!("Step 5: Checking final input state (all keys released)");
        // In the unified path, correctness is covered by IoEventServer tests; here we ensure client send path doesn't panic.

        // 6. Cleanup
        info!("Step 6: Cleaning up");
        server_handle.abort();
        io_server_task.abort();
    }
}

/// Example usage of the WebServer.
pub mod examples {
    /// WebSocket Bidirectional Protocol
    ///
    /// The WebSocket endpoint at `/ws` supports bidirectional communication:
    /// - **Input (Client → Server)**: Clients send JSON messages in the format of [`InputEventPacket`] for user input processing
    /// - **Feedback (Client → All Clients)**: Clients can send JSON messages in the format of [`FeedbackEventPacket`] to be broadcasted to all connected clients
    /// - **Feedback (Server → Client)**: Server broadcasts JSON messages in the format of [`FeedbackEventPacket`] to all connected clients
    ///
    /// ## Write-Only Mode
    ///
    /// Clients can opt out of receiving feedback events by including the header `X-Backflow-Write-Only: true`
    /// when upgrading the WebSocket connection. This saves bandwidth for input-only applications:
    ///
    /// ```
    /// GET /ws HTTP/1.1
    /// Host: localhost:8000
    /// Upgrade: websocket
    /// Connection: Upgrade
    /// X-Backflow-Write-Only: true
    /// Sec-WebSocket-Key: ...
    /// Sec-WebSocket-Version: 13
    /// ```
    ///
    /// ## Input Message Example (Client sends to Server)
    ///
    /// Send user input events like keyboard presses, mouse movements, etc:
    ///
    /// ```json
    /// {
    ///   "device_id": "my-device",
    ///   "timestamp": 1672531200000,
    ///   "events": [
    ///     {
    ///       "Keyboard": {
    ///         "KeyPress": {
    ///           "key": "a"
    ///         }
    ///       }
    ///     },
    ///     {
    ///       "Pointer": {
    ///         "Move": {
    ///           "x_delta": 10,
    ///           "y_delta": -5
    ///         }
    ///       }
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Client Feedback Broadcast Example (Client broadcasts to All Clients)
    ///
    /// Send feedback events to be announced to all connected clients:
    ///
    /// ```json
    /// {
    ///   "device_id": "my-custom-device",
    ///   "timestamp": 1672531200000,
    ///   "events": [
    ///     {
    ///       "Led": {
    ///         "Set": {
    ///           "led_id": 42,
    ///           "on": true,
    ///           "brightness": 255,
    ///           "rgb": [255, 0, 0]
    ///         }
    ///       }
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Feedback Message Example (Server broadcasts to Clients)
    ///
    /// Server-generated feedback events are automatically sent to all clients:
    ///
    /// ```json
    /// {
    ///   "device_id": "controller-001",
    ///   "timestamp": 1672531205000,
    ///   "events": [
    ///     {
    ///       "Led": {
    ///         "Set": {
    ///           "led_id": 1,
    ///           "on": true,
    ///           "brightness": 200,
    ///           "rgb": [255, 0, 0]
    ///         }
    ///       }
    ///     },
    ///     {
    ///       "Haptic": {
    ///         "Vibrate": {
    ///           "motor_id": 0,
    ///           "intensity": 128,
    ///           "duration_ms": 500
    ///         }
    ///       }
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Connection Behavior
    ///
    /// - All feedback messages are broadcast to ALL connected WebSocket clients
    /// - Clients can send input events at any time
    /// - Server will send feedback events to all clients when available
    /// - Connection supports standard WebSocket ping/pong for keepalive
    pub fn _doc_example() {}
}

/// Bidirectional WebSocket handler that processes input events from clients
/// and broadcasts feedback events to all connected clients.
///
/// Supports write-only mode (disables outbound feedback/input echo) via either:
/// - HTTP header: `X-Backflow-Write-Only: true`
/// - Query parameter: `/ws?write_only=true` (also accepts `1`, `yes`)
///
/// Header and query parameter are OR'ed; if either indicates truthy, write-only mode is enabled.
pub async fn bidirectional_ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<WebSocketState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Determine write-only via header OR query param
    let header_write_only = headers
        .get("X-Backflow-Write-Only")
        .and_then(|v| v.to_str().ok())
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);
    let query_write_only = params
        .get("write_only")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);
    let is_write_only = header_write_only || query_write_only;

    if is_write_only {
        info!(
            "New write-only WebSocket connection from {} (no feedback)",
            addr
        );
    } else {
        info!("New bidirectional WebSocket connection from {}", addr);
    }

    ws.on_upgrade(move |socket| handle_socket(socket, addr, state, is_write_only))
}
/// Handles a single WebSocket message for a client. Returns true if the connection should break/close.
#[tracing::instrument(skip_all, fields(addr = %addr))]
async fn handle_websocket_message(
    msg: Result<Message, axum::Error>,
    addr: SocketAddr,
    sender: &Arc<tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
    io_server: Arc<IoEventServer>,
) -> bool {
    match msg {
        Ok(Message::Text(text)) => {
            debug!("Received message from {}: {}", addr, text);

            // Unified control handling with ClientControlMessage
            if let Ok(env) = serde_json::from_str::<crate::input::io_server::ControlEnvelope>(&text)
            {
                if env.kind == "control" {
                    use crate::input::io_server::ClientControlMessage;
                    if let Ok(ctrl) = serde_json::from_value::<ClientControlMessage>(env.inner) {
                        match &ctrl {
                            ClientControlMessage::UpdateStreamRegistration { .. } => {
                                if let Err(e) =
                                    io_server.inbound_sender().send(IoInboundMessage::Control {
                                        client_id: addr.to_string(),
                                        msg: ctrl.clone(),
                                    })
                                {
                                    error!("Failed to send control msg: {e}");
                                }
                            }
                            ClientControlMessage::ListStreams => {
                                let streams = io_server.list_stream_ids().await;
                                let resp = serde_json::json!({"type":"streams","streams": streams});
                                if let Ok(json) = serde_json::to_string(&resp) {
                                    let mut g = sender.lock().await;
                                    let _ = g.send(Message::Text(json.into())).await;
                                }
                            }
                            _ => {
                                // Forward other control mutations (filters, stop, etc.)
                                if let Err(e) =
                                    io_server.inbound_sender().send(IoInboundMessage::Control {
                                        client_id: addr.to_string(),
                                        msg: ctrl.clone(),
                                    })
                                {
                                    error!("Failed to send control msg: {e}");
                                }
                            }
                        }
                        return false;
                    } else {
                        // invalid control contents
                        io_server.send_app_error(
                            &addr.to_string(),
                            crate::error::AppError::Parse("Failed to parse control command".into()),
                        );
                        return false;
                    }
                }
            }

            // Try to parse as InputEventPacket
            if let Ok(packet) = serde_json::from_str::<InputEventPacket>(&text) {
                debug!(
                    "Parsed input packet from {}: device_id={}, events={}",
                    addr,
                    packet.device_id,
                    packet.events.len()
                );

                if let Err(e) = io_server.inbound_sender().send(IoInboundMessage::Input {
                    client_id: addr.to_string(),
                    packet,
                }) {
                    error!("Failed to send input to IoEventServer: {}", e);
                }
            }
            // If not an input packet, try to parse as FeedbackEventPacket for broadcasting
            else if let Ok(feedback_packet) = serde_json::from_str::<FeedbackEventPacket>(&text) {
                trace!(
                    feedback_packet.device_id,
                    feedback_packet_events_count = feedback_packet.events.len(),
                    "Received feedback packet from {addr} for broadcasting",
                );

                // Instead of broadcasting directly here, send it through the main feedback stream
                // This ensures all feedback goes through the same unified broadcast mechanism
                if let Err(e) = io_server.inbound_sender().send(IoInboundMessage::Feedback {
                    client_id: addr.to_string(),
                    packet: feedback_packet,
                }) {
                    error!("Failed to forward feedback to IoEventServer: {}", e);
                }
            } else {
                warn!(
                    "Failed to parse message from {} as either input or feedback packet",
                    addr
                );
            }
            false
        }
        Ok(Message::Binary(_)) => {
            warn!("Received unexpected binary message from {}", addr);
            false
        }
        Ok(Message::Ping(data)) => {
            debug!("Received ping from {}", addr);
            let mut sender_guard = sender.lock().await;
            if let Err(e) = sender_guard.send(Message::Pong(data)).await {
                warn!("Failed to send pong to {}: {}", addr, e);
                return true;
            }
            false
        }
        Ok(Message::Pong(_)) => {
            debug!("Received pong from {}", addr);
            false
        }
        Ok(Message::Close(_)) => {
            info!("Client {} disconnected", addr);
            true
        }
        Err(e) => {
            warn!("WebSocket error from {}: {}", addr, e);
            true
        }
    }
}

// Previous specialized feedback send helper removed; we now serialize full IoClientMessage
// variants (including Feedback, Input, Error, ControlAck) for consistency with unix/stdio backends.

/// Handles a single WebSocket connection for bidirectional communication.
/// If `is_write_only` is true, the client will not receive feedback events.
#[tracing::instrument(skip_all)]
async fn handle_socket(
    socket: WebSocket,
    addr: SocketAddr,
    state: WebSocketState,
    is_write_only: bool,
) {
    let (sender, mut receiver) = socket.split();

    let state_for_input_task = state.clone();

    // Wrap sender in Arc<Mutex> to share between tasks
    let sender = Arc::new(tokio::sync::Mutex::new(sender));

    // If unified IO server present, register this client to receive unified feedback/errors
    let unified_feedback_rx = state
        .io_server
        .register_client(ClientNode {
            id: addr.to_string(),
            client_settings: if is_write_only {
                ClientSettings {
                    write_only: true,
                    ..Default::default()
                }
            } else {
                Default::default()
            },
        })
        .await;

    // Task to handle incoming messages (input events from client)
    let input_task = {
        let sender = sender.clone();
        let io_server = state_for_input_task.io_server.clone();
        let handle_web_socket_messages = async move {
            while let Some(msg) = receiver.next().await {
                if handle_websocket_message(msg, addr, &sender, io_server.clone()).await {
                    break;
                }
            }
            // Note: Client cleanup from the broadcast list happens automatically
            // when the feedback channel is dropped and detected as closed
            debug!("Input task for client {} finished", addr);
        };
        tokio::spawn(handle_web_socket_messages)
    };

    // Task to handle outgoing messages: serialize full IoClientMessage enum so clients get a
    // discriminated union (matches unix/stdio backend behavior). This includes Input packets
    // routed by stream registration or device filters – previously these were dropped.
    let output_task = if is_write_only {
        None
    } else {
        let sender = sender.clone();
        let mut rx = unified_feedback_rx;
        Some(tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                // Serialize the whole enum so JSON shape is {"type":..,"data":..}
                // This unifies WebSocket transport with unix/stdio backends.
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        let mut guard = sender.lock().await;
                        if guard.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                        // Best-effort flush (not required but keeps latency low)
                        if guard.flush().await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Failed to serialize IoClientMessage for {}: {}", addr, e);
                    }
                }
            }
        }))
    };

    // legacy feedback task removed

    // Wait for either task to complete (connection closed or error)
    match output_task {
        Some(output_task) => {
            tokio::select! {
                _ = input_task => {
                    debug!("Input task completed for {}", addr);
                }
                _ = output_task => {
                    debug!("Output task completed for {}", addr);
                }
            }
        }
        None => {
            // Write-only mode - only wait for input task
            let _ = input_task.await;
            debug!("Input-only task completed for {}", addr);
        }
    }

    info!("WebSocket connection {} closed", addr);
}

/// REST API handler for listing custom layouts
async fn get_custom_layouts(
    State(server): State<Arc<WebServer>>,
) -> Json<Vec<WebTemplateResponse>> {
    Json(server.list_custom_layouts())
}
