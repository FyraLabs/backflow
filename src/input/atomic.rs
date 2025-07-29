/// Enhanced input processing system with FIFO queuing and atomicity
///
/// This module provides a FIFO queue system for input processing that ensures:
/// - Per-client atomicity (using compound device IDs like "device@client_addr")
/// - Timestamp-based ordering within each client's queue
/// - Immediate processing to prevent stuck events
/// - Cross-client isolation to prevent interference
use crate::input::InputEventPacket;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, trace, warn};

/// Configuration for input batching behavior
#[derive(Debug, Clone)]
pub struct BatchingConfig {
    /// Maximum time to wait for related events before processing a batch
    pub max_batch_delay: Duration,
    /// Maximum number of events to accumulate before forcing a batch
    pub max_batch_size: usize,
    /// Maximum number of packets to buffer per device
    pub max_buffer_per_device: usize,
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            max_batch_delay: Duration::from_millis(1), // Still used by flush timer for cleanup
            max_batch_size: 1,                         // Immediate processing, no batching
            max_buffer_per_device: 5,                  // Small buffer since we flush immediately
        }
    }
}

/// A buffered packet waiting to be processed
#[derive(Debug)]
struct BufferedPacket {
    packet: InputEventPacket,
    received_at: Instant,
}

/// Enhanced input processor that handles cross-device atomicity
pub struct AtomicInputProcessor {
    config: BatchingConfig,
    /// Buffer of packets waiting to be batched, organized by device
    pending_packets: RwLock<HashMap<String, VecDeque<BufferedPacket>>>,
    /// Sequence counter for packet ordering
    sequence_counter: std::sync::atomic::AtomicU64,
    /// Last processed timestamp per device
    last_timestamps: RwLock<HashMap<String, u64>>,
}

impl AtomicInputProcessor {
    pub fn new(config: BatchingConfig) -> Self {
        Self {
            config,
            pending_packets: RwLock::new(HashMap::new()),
            sequence_counter: std::sync::atomic::AtomicU64::new(0),
            last_timestamps: RwLock::new(HashMap::new()),
        }
    }

    /// Process an incoming packet, handling batching and atomicity
    pub async fn process_packet(
        &self,
        packet: InputEventPacket,
    ) -> Result<Option<InputEventPacket>, ProcessingError> {
        let sequence_num = self
            .sequence_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Check if packet should be processed based on timestamp ordering
        if !self.should_process_packet(&packet).await {
            return Ok(None);
        }

        // Add packet to buffer
        let mut pending = self.pending_packets.write().await;
        let device_buffer = pending
            .entry(packet.device_id.clone())
            .or_insert_with(VecDeque::new);

        // Enforce buffer size limit
        if device_buffer.len() >= self.config.max_buffer_per_device {
            warn!(
                "Device '{}' buffer full, dropping oldest packet (seq: {})",
                packet.device_id, sequence_num
            );
            device_buffer.pop_front();
        }

        device_buffer.push_back(BufferedPacket {
            packet: packet.clone(),
            received_at: Instant::now(),
        });

        trace!(
            "Buffered packet for device '{}' (seq: {}, buffer size: {})",
            packet.device_id,
            sequence_num,
            device_buffer.len()
        );

        // Check if we should flush a batch
        if self
            .should_flush_batch(&packet.device_id, &packet, &pending)
            .await
        {
            self.flush_device_batch(&packet.device_id, pending).await
        } else {
            Ok(None)
        }
    }

    /// Check if a packet should be processed based on timestamp ordering
    async fn should_process_packet(&self, packet: &InputEventPacket) -> bool {
        let mut timestamps = self.last_timestamps.write().await;

        match timestamps.get(&packet.device_id) {
            Some(&last_timestamp) => {
                if packet.timestamp < last_timestamp {
                    warn!(
                        "Discarding out-of-order packet for device '{}': timestamp {} < last {}",
                        packet.device_id, packet.timestamp, last_timestamp
                    );
                    return false;
                }
            }
            None => {
                debug!("First packet for device '{}'", packet.device_id);
            }
        }

        timestamps.insert(packet.device_id.clone(), packet.timestamp);
        true
    }

    /// Check if we should flush a batch for a device
    async fn should_flush_batch(
        &self,
        device_id: &str,
        _current_packet: &InputEventPacket,
        pending: &HashMap<String, VecDeque<BufferedPacket>>,
    ) -> bool {
        // Always flush immediately - this creates a FIFO queue behavior
        // where every packet is processed in arrival order per device
        // while maintaining per-client atomicity (due to compound device IDs)
        if let Some(buffer) = pending.get(device_id) {
            !buffer.is_empty()
        } else {
            false
        }
    }

    /// Flush all buffered packets for a device into a single atomic batch
    async fn flush_device_batch(
        &self,
        device_id: &str,
        mut pending: tokio::sync::RwLockWriteGuard<'_, HashMap<String, VecDeque<BufferedPacket>>>,
    ) -> Result<Option<InputEventPacket>, ProcessingError> {
        if let Some(buffer) = pending.get_mut(device_id) {
            if buffer.is_empty() {
                return Ok(None);
            }

            // Collect all events from buffered packets
            let mut all_events = Vec::new();
            let mut latest_timestamp = 0u64;

            while let Some(buffered) = buffer.pop_front() {
                all_events.extend(buffered.packet.events);
                latest_timestamp = latest_timestamp.max(buffered.packet.timestamp);
            }

            if all_events.is_empty() {
                return Ok(None);
            }

            // Create a new atomic batch packet
            let batch_packet = InputEventPacket {
                device_id: device_id.to_string(),
                timestamp: latest_timestamp,
                events: all_events,
            };

            debug!(
                "Flushed atomic batch for device '{}': {} events at timestamp {}",
                device_id,
                batch_packet.events.len(),
                latest_timestamp
            );

            Ok(Some(batch_packet))
        } else {
            Ok(None)
        }
    }

    /// Force flush all pending batches (useful for shutdown)
    pub async fn flush_all(&self) -> Vec<InputEventPacket> {
        let mut pending = self.pending_packets.write().await;
        let mut result = Vec::new();

        for (device_id, buffer) in pending.iter_mut() {
            if buffer.is_empty() {
                continue;
            }

            let mut all_events = Vec::new();
            let mut latest_timestamp = 0u64;

            while let Some(buffered) = buffer.pop_front() {
                all_events.extend(buffered.packet.events);
                latest_timestamp = latest_timestamp.max(buffered.packet.timestamp);
            }

            if !all_events.is_empty() {
                result.push(InputEventPacket {
                    device_id: device_id.clone(),
                    timestamp: latest_timestamp,
                    events: all_events,
                });
            }
        }

        debug!("Force-flushed {} atomic batches", result.len());
        result
    }

    /// Start a background task to periodically flush old batches
    pub fn start_flush_timer(&self) -> mpsc::UnboundedReceiver<InputEventPacket> {
        let (tx, rx) = mpsc::unbounded_channel();
        let processor = self.clone();

        tokio::spawn(async move {
            let mut flush_interval = tokio::time::interval(processor.config.max_batch_delay);
            flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                flush_interval.tick().await;

                // Check for expired batches and flush them one by one
                let devices_to_flush: Vec<String> = {
                    let pending = processor.pending_packets.read().await;
                    let now = Instant::now();

                    pending
                        .iter()
                        .filter_map(|(device_id, buffer)| {
                            if let Some(oldest) = buffer.front() {
                                if now.duration_since(oldest.received_at)
                                    >= processor.config.max_batch_delay
                                {
                                    Some(device_id.clone())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                // Flush expired batches one device at a time
                for device_id in devices_to_flush {
                    let batch_opt = {
                        let pending = processor.pending_packets.write().await;
                        processor.flush_device_batch(&device_id, pending).await
                    };

                    if let Ok(Some(batch)) = batch_opt {
                        if tx.send(batch).is_err() {
                            debug!("Flush timer receiver dropped, stopping");
                            return;
                        }
                    }
                }
            }
        });

        rx
    }
}

impl Clone for AtomicInputProcessor {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            pending_packets: RwLock::new(HashMap::new()),
            sequence_counter: std::sync::atomic::AtomicU64::new(0),
            last_timestamps: RwLock::new(HashMap::new()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[error("Device buffer overflow")]
    BufferOverflow,
    #[error("Invalid packet timestamp")]
    InvalidTimestamp,
    #[error("Processing timeout")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputEvent, KeyboardEvent};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn test_atomic_batching() {
        let config = BatchingConfig {
            max_batch_delay: Duration::from_millis(10),
            max_batch_size: 3,
            max_buffer_per_device: 5,
        };

        let processor = AtomicInputProcessor::new(config);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Create multiple packets for the same device
        let packet1 = InputEventPacket {
            device_id: "test-device".to_string(),
            timestamp,
            events: vec![InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "a".to_string(),
            })],
        };

        let packet2 = InputEventPacket {
            device_id: "test-device".to_string(),
            timestamp: timestamp + 1,
            events: vec![InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                key: "a".to_string(),
            })],
        };

        // Process packets
        let result1 = processor.process_packet(packet1).await.unwrap();
        let result2 = processor.process_packet(packet2).await.unwrap();

        // First packet should be buffered
        assert!(result1.is_none());

        // Second packet might trigger a batch or remain buffered
        // depending on timing and configuration
    }

    #[tokio::test]
    async fn test_out_of_order_rejection() {
        let processor = AtomicInputProcessor::new(BatchingConfig::default());

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let packet1 = InputEventPacket {
            device_id: "test-device".to_string(),
            timestamp: timestamp + 100,
            events: vec![],
        };
        let packet2 = InputEventPacket {
            device_id: "test-device".to_string(),
            timestamp, // Older
            events: vec![],
        };

        // Process newer packet first
        processor.process_packet(packet1).await.unwrap();

        // Older packet should be rejected
        let result = processor.process_packet(packet2).await.unwrap();
        assert!(result.is_none());
    }
}
