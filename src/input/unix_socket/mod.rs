use crate::feedback::FeedbackEventPacket;
use crate::input::io_server::{
    ClientNode, ClientSettings, IoClientMessage, IoEventServer, IoInboundMessage,
};
use crate::input::{InputBackend, InputEventPacket};
use eyre::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

pub struct UnixSocketServer {
    pub socket_path: PathBuf,
    pub io_server: Arc<IoEventServer>,
}

impl UnixSocketServer {
    pub fn new(socket_path: PathBuf, io_server: Arc<IoEventServer>) -> Self {
        Self {
            socket_path,
            io_server,
        }
    }

    async fn handle_client(&self, stream: UnixStream) {
        let peer_addr = format!("unix_socket_{}", std::process::id()); // Use process ID as identifier for UNIX socket
        let (reader, writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut writer = BufWriter::new(writer);
        info!("UNIX socket client connected");

        let io_server = self.io_server.clone();
        // Register a client node (one per socket connection)
        let client_id = peer_addr.clone();
        let feedback_rx_for_client = io_server
            .register_client(ClientNode {
                id: client_id.clone(),
                client_settings: ClientSettings::default(),
            })
            .await;
        let mut outbound_rx = feedback_rx_for_client; // IoClientMessage receiver
        let _phantom_use: Option<IoClientMessage> = None; // keep type imported (avoid unused warning)
        let inbound_tx = io_server.inbound_sender();

        // Task to handle incoming messages (input/feedback/control from client)
        let input_task = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let line_trimmed = line.trim();
                if line_trimmed.is_empty() {
                    continue;
                }
                // Unified control handling: attempt to parse {"type":"control", ...}
                if let Ok(env) =
                    serde_json::from_str::<crate::input::io_server::ControlEnvelope>(line_trimmed)
                {
                    if env.kind == "control" {
                        use crate::input::io_server::ClientControlMessage;
                        if let Ok(ctrl) = serde_json::from_value::<ClientControlMessage>(env.inner)
                        {
                            match ctrl {
                                ClientControlMessage::ListStreams => {
                                    // Transport-level response
                                    let streams = io_server.list_stream_ids().await;
                                    let _resp =
                                        serde_json::json!({"type":"streams","streams":streams});
                                    // We don't echo on unix socket currently (no writer handle here) -> could add ack path
                                    // For now just ignore; clients using unix socket can rely on future ack implementation.
                                }
                                other => {
                                    let _ = inbound_tx.send(IoInboundMessage::Control {
                                        client_id: client_id.clone(),
                                        msg: other,
                                    });
                                }
                            }
                            continue;
                        } else {
                            io_server.send_app_error(
                                &client_id,
                                crate::error::AppError::Parse(
                                    "Failed to parse control command".into(),
                                ),
                            );
                            continue;
                        }
                    }
                }

                if let Ok(packet) = serde_json::from_str::<InputEventPacket>(&line) {
                    debug!(
                        "(unix) inbound input: client={}, device={}, events={}",
                        peer_addr,
                        packet.device_id,
                        packet.events.len()
                    );
                    if let Err(e) = inbound_tx.send(IoInboundMessage::Input {
                        client_id: client_id.clone(),
                        packet,
                    }) {
                        error!("Failed to forward input to IoEventServer: {}", e);
                    }
                } else if let Ok(packet) = serde_json::from_str::<FeedbackEventPacket>(&line) {
                    debug!(
                        "(unix) inbound feedback: client={}, device={}, events={}",
                        peer_addr,
                        packet.device_id,
                        packet.events.len()
                    );
                    if let Err(e) = inbound_tx.send(IoInboundMessage::Feedback {
                        client_id: client_id.clone(),
                        packet,
                    }) {
                        error!("Failed to forward feedback to IoEventServer: {}", e);
                    }
                } else {
                    warn!(
                        "Failed to parse message from unix socket (neither input, feedback, nor control)"
                    );
                    io_server.send_app_error(&client_id, crate::error::AppError::Parse("Message was not a valid InputEventPacket, FeedbackEventPacket, or control envelope".into()));
                }
            }
            debug!("UNIX socket client input task finished");
        });

        // Task to handle outbound (feedback + errors) events to client
        let output_task = tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        if let Err(e) = writer.write_all(json.as_bytes()).await {
                            warn!("Failed to send message to unix socket client: {}", e);
                            break;
                        }
                        if let Err(e) = writer.write_all(b"\n").await {
                            warn!("Failed to send newline to unix socket client: {}", e);
                            break;
                        }
                        if let Err(e) = writer.flush().await {
                            warn!("Failed to flush unix socket client: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to serialize outbound message for unix socket client: {}",
                            e
                        );
                    }
                }
            }
            debug!("UNIX socket client output task finished");
        });

        // Wait for either task to complete
        let _ = tokio::join!(input_task, output_task);
        info!("UNIX socket client disconnected");
    }
}

#[async_trait::async_trait]
impl InputBackend for UnixSocketServer {
    async fn run(&mut self) -> Result<()> {
        // Remove any existing socket file
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = UnixListener::bind(&self.socket_path).with_context(|| {
            format!("Failed to bind unix socket: {}", self.socket_path.display())
        })?;
        info!(
            "UNIX socket input backend listening at {}",
            self.socket_path.display()
        );

        // NOTE: atomic batching & feedback broadcast now handled by IoEventServer

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let handler = self.clone();
                    tokio::spawn(async move {
                        handler.handle_client(stream).await;
                    });
                }
                Err(e) => {
                    error!("Failed to accept unix socket connection: {}", e);
                }
            }
        }
    }
}

impl Clone for UnixSocketServer {
    fn clone(&self) -> Self {
        Self {
            socket_path: self.socket_path.clone(),
            io_server: self.io_server.clone(),
        }
    }
}
