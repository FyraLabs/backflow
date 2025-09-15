//! StdIO based transport.
//!
//! Mirrors the behaviour of the UNIX socket backend, but instead of a per-connection
//! stream it uses the parent process' stdin (for inbound messages) and
//! stdout (for outbound messages). Errors are also emitted as normal outbound
//! `IoClientMessage::Error` variants over stdout so a supervising process can
//! parse everything from a single stream. This allows simple composition with
//! tools that can only speak over standard I/O (plugins, child processes, etc.).
//!
//! For now this backend is unconditional and not configurable; it's intended as
//! a building block for future plugin execution models. One logical StdIO client
//! is registered whose id is `stdio`.

use crate::feedback::FeedbackEventPacket;
use crate::input::io_server::{ClientNode, ClientSettings, IoEventServer, IoInboundMessage};
use crate::input::{InputBackend, InputEventPacket};
use eyre::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

enum ReaderMode {
    Inherit, // Use process stdin
    Provided(BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>),
}
enum WriterMode {
    Inherit, // Use process stdout
    Provided(Box<dyn tokio::io::AsyncWrite + Send + Unpin>),
}

pub struct StdioBackend {
    pub io_server: Arc<IoEventServer>,
    client_id: String,
    reader: ReaderMode,
    writer: WriterMode,
}

impl StdioBackend {
    /*
    /// Standard constructor using the current process stdin/stdout with fixed id "stdio"
    pub fn new(io_server: Arc<IoEventServer>) -> Self {
        Self {
            io_server,
            client_id: "stdio".into(),
            reader: ReaderMode::Inherit,
            writer: WriterMode::Inherit,
        }
    }
    */
    /// Create a backend with custom reader & writer (e.g. plugin child process)
    pub fn with_streams<R, W>(
        client_id: impl Into<String>,
        reader: R,
        writer: W,
        io_server: Arc<IoEventServer>,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            io_server,
            client_id: client_id.into(),
            reader: ReaderMode::Provided(BufReader::new(Box::new(reader))),
            writer: WriterMode::Provided(Box::new(writer)),
        }
    }
}

#[async_trait::async_trait]
impl InputBackend for StdioBackend {
    async fn run(&mut self) -> Result<()> {
        info!(client_id=%self.client_id, "Starting stdio transport backend");

        let io_server = self.io_server.clone();
        let client_id = self.client_id.clone();

        // Register logical client
        let mut outbound_rx = io_server
            .register_client(ClientNode {
                id: client_id.clone(),
                client_settings: ClientSettings::default(),
            })
            .await;
        let inbound_tx = io_server.inbound_sender();

        // Writer task (move ownership of provided writer out)
        let client_id_reader = client_id.clone();
        match std::mem::replace(&mut self.writer, WriterMode::Inherit) {
            WriterMode::Provided(mut w) => {
                let client_id_out = client_id.clone();
                tokio::spawn(async move {
                    while let Some(msg) = outbound_rx.recv().await {
                        match serde_json::to_string(&msg) {
                            Ok(json) => {
                                if let Err(e) = w.write_all(json.as_bytes()).await {
                                    warn!(%client_id_out, error=%e, "Failed writing json to stdio writer");
                                    break;
                                }
                                if let Err(e) = w.write_all(b"\n").await {
                                    warn!(%client_id_out, error=%e, "Failed writing newline to stdio writer");
                                    break;
                                }
                                if let Err(e) = w.flush().await {
                                    warn!(%client_id_out, error=%e, "Failed flushing stdio writer");
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(%client_id_out, error=%e, "Failed serializing IoClientMessage")
                            }
                        }
                    }
                    debug!(%client_id_out, "StdIO provided writer outbound task ended");
                });
            }
            WriterMode::Inherit => {
                let client_id_out = client_id.clone();
                tokio::spawn(async move {
                    let mut stdout = tokio::io::stdout();
                    while let Some(msg) = outbound_rx.recv().await {
                        match serde_json::to_string(&msg) {
                            Ok(json) => {
                                if let Err(e) = stdout.write_all(json.as_bytes()).await {
                                    warn!(%client_id_out, error=%e, "Failed writing stdout json");
                                    break;
                                }
                                if let Err(e) = stdout.write_all(b"\n").await {
                                    warn!(%client_id_out, error=%e, "Failed writing stdout newline");
                                    break;
                                }
                                if let Err(e) = stdout.flush().await {
                                    warn!(%client_id_out, error=%e, "Failed flushing stdout");
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(%client_id_out, error=%e, "Failed serializing IoClientMessage for stdout")
                            }
                        }
                    }
                    debug!(%client_id_out, "StdIO outbound task ended");
                });
            }
        }

        // Reader loop using unified enum wrapper
        enum ReaderLines {
            Inherit(tokio::io::Lines<BufReader<tokio::io::Stdin>>),
            Provided(tokio::io::Lines<BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>>),
        }
        impl ReaderLines {
            async fn next_line(&mut self) -> std::io::Result<Option<String>> {
                match self {
                    ReaderLines::Inherit(lines) => lines.next_line().await,
                    ReaderLines::Provided(lines) => lines.next_line().await,
                }
            }
        }
        let mut reader_lines = match std::mem::replace(&mut self.reader, ReaderMode::Inherit) {
            ReaderMode::Inherit => {
                let stdin = tokio::io::stdin();
                ReaderLines::Inherit(BufReader::new(stdin).lines())
            }
            ReaderMode::Provided(buf) => ReaderLines::Provided(buf.lines()),
        };
        while let Ok(Some(line)) = reader_lines.next_line().await {
            let line = line.trim_end().to_string();
            if line.trim().is_empty() {
                continue;
            }
            // Unified control parsing (same structure as unix + websocket)
            if let Ok(env) = serde_json::from_str::<crate::input::io_server::ControlEnvelope>(&line)
            {
                if env.kind == "control" {
                    use crate::input::io_server::ClientControlMessage;
                    if let Ok(ctrl) = serde_json::from_value::<ClientControlMessage>(env.inner) {
                        match ctrl {
                            ClientControlMessage::ListStreams => {
                                let streams = io_server.list_stream_ids().await;
                                // We currently do not emit an ack over stdio (could print JSON line)
                                let _ = streams; // silence unused for now
                            }
                            other => {
                                let _ = inbound_tx.send(IoInboundMessage::Control {
                                    client_id: client_id_reader.clone(),
                                    msg: other,
                                });
                            }
                        }
                        continue;
                    } else {
                        io_server.send_app_error(
                            &client_id_reader,
                            crate::error::AppError::Parse("Failed to parse control command".into()),
                        );
                        continue;
                    }
                }
            }
            if let Ok(packet) = serde_json::from_str::<InputEventPacket>(&line) {
                debug!(%client_id_reader, device=%packet.device_id, events=%packet.events.len(), "(stdio) inbound input");
                if let Err(e) = inbound_tx.send(IoInboundMessage::Input {
                    client_id: client_id_reader.clone(),
                    packet,
                }) {
                    error!(%client_id_reader, error=%e, "Failed to forward stdio input");
                }
            } else if let Ok(packet) = serde_json::from_str::<FeedbackEventPacket>(&line) {
                debug!(%client_id_reader, device=%packet.device_id, events=%packet.events.len(), "(stdio) inbound feedback");
                if let Err(e) = inbound_tx.send(IoInboundMessage::Feedback {
                    client_id: client_id_reader.clone(),
                    packet,
                }) {
                    error!(%client_id_reader, error=%e, "Failed to forward stdio feedback");
                }
            } else {
                warn!(%client_id_reader, "Failed to parse stdio line (neither input nor feedback nor control)");
                io_server.send_app_error(
                    &client_id_reader,
                    crate::error::AppError::Parse(
                        "Message was not a valid InputEventPacket or FeedbackEventPacket".into(),
                    ),
                );
            }
        }
        info!(%client_id_reader, "StdIO backend exiting (reader closed)");
        Ok(())
    }
}

// Provide a no-op Clone similar to other backends if needed later
impl Clone for StdioBackend {
    fn clone(&self) -> Self {
        Self {
            io_server: self.io_server.clone(),
            client_id: self.client_id.clone(),
            // Cloning reader/writer modes: Inherit is copyable, Provided cannot be cloned safely.
            // For simplicity, clones fall back to inherit mode (suitable for typical usage where clone isn't needed heavily).
            reader: ReaderMode::Inherit,
            writer: WriterMode::Inherit,
        }
    }
}
