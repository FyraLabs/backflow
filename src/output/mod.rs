pub mod chuniio_proxy;
pub mod dummy;
pub mod plumber_dbus;
pub mod udev;

// Re-export the concrete types for external use
pub use chuniio_proxy::ChuniioProxyServer;
pub use dummy::DummyOutput;
pub use plumber_dbus::DbusPlumberOutput;
pub use udev::UdevOutput;

// even though our MSRV is Rust 1.75 and Rust 2024,
// we use async_trait because of trait object safety
pub trait OutputBackend: Send {
    /// Starts the output backend, processing input events and sending them to the appropriate destination.
    async fn run(&mut self) -> eyre::Result<()>;
    async fn stop(&mut self) -> eyre::Result<()> {
        // Default implementation does nothing
        Ok(())
    }
}

pub enum OutputBackendType {
    Dummy(DummyOutput),
    Dbus(DbusPlumberOutput),
    Udev(UdevOutput),
    ChuniioProxy(ChuniioProxyServer),
}

impl OutputBackend for OutputBackendType {
    async fn run(&mut self) -> eyre::Result<()> {
        match self {
            OutputBackendType::Dummy(backend) => backend.run().await,
            OutputBackendType::Dbus(backend) => backend.run().await,
            OutputBackendType::Udev(backend) => backend.run().await,
            OutputBackendType::ChuniioProxy(backend) => backend.run().await,
        }
    }

    async fn stop(&mut self) -> eyre::Result<()> {
        match self {
            OutputBackendType::Dummy(backend) => backend.stop().await,
            OutputBackendType::Dbus(backend) => backend.stop().await,
            OutputBackendType::Udev(backend) => backend.stop().await,
            OutputBackendType::ChuniioProxy(backend) => backend.stop().await,
        }
    }
}

pub fn rgb_to_brg(rgb: &[u8; 3]) -> [u8; 3] {
    // Convert RGB to BRG (Blue, Red, Green)
    [rgb[2], rgb[0], rgb[1]]
}

#[cfg(test)]
mod tests {
    use crate::feedback::FeedbackEventStream;
    use crate::feedback::generators::chuni_jvs::ChuniLedDataPacket;
    use crate::input::{InputEvent, InputEventPacket, InputEventStream, KeyboardEvent};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Mutex, mpsc};
    use tokio::time::timeout;

    /// Test to reproduce the issue where KeyRelease events for CHUNIIO events
    /// are not being parsed properly when both uinput and chuniio_proxy backends
    /// are consuming from the same stream
    #[tokio::test]
    async fn test_chuniio_keyrelease_events_with_multiple_backends() {
        // Create streams
        let input_stream = InputEventStream::new();
        let feedback_stream = FeedbackEventStream::new();
        let (led_packet_tx, _led_packet_rx) = mpsc::unbounded_channel::<ChuniLedDataPacket>(); // Create a shared counter to track processed events
        let processed_events = Arc::new(Mutex::new(HashMap::<String, u32>::new()));

        // Create receivers before starting the backends to ensure they exist when we send events
        let uinput_receiver = input_stream.subscribe();
        let chuniio_receiver = input_stream.subscribe();

        // Create uinput backend (this will filter out CHUNIIO events)
        let uinput_counter = processed_events.clone();
        let uinput_handle = tokio::spawn(async move {
            let mut receiver = uinput_receiver;
            // Process a limited number of packets for testing
            for _ in 0..10 {
                if let Ok(Some(packet)) =
                    timeout(Duration::from_millis(100), receiver.receive()).await
                {
                    for event in &packet.events {
                        if let InputEvent::Keyboard(keyboard_event) = event {
                            let key = match keyboard_event {
                                KeyboardEvent::KeyPress { key } => key,
                                KeyboardEvent::KeyRelease { key } => key,
                            };

                            // Simulate the filtering logic from UdevOutput
                            if !crate::device_filter::DeviceFilter::is_standard_evdev_key(key) {
                                // This backend should skip CHUNIIO events
                                continue;
                            }

                            let event_key = format!("uinput_{key}");
                            let mut counter = uinput_counter.lock().await;
                            *counter.entry(event_key).or_insert(0) += 1;
                        }
                    }
                } else {
                    break;
                }
            }
        });

        // Create chuniio proxy backend (this will process CHUNIIO events)
        let chuniio_counter = processed_events.clone();
        let chuniio_handle = tokio::spawn(async move {
            let mut receiver = chuniio_receiver;
            // Process a limited number of packets for testing
            for _ in 0..10 {
                if let Ok(Some(packet)) =
                    timeout(Duration::from_millis(100), receiver.receive()).await
                {
                    for event in &packet.events {
                        if let InputEvent::Keyboard(keyboard_event) = event {
                            let key = match keyboard_event {
                                KeyboardEvent::KeyPress { key } => key,
                                KeyboardEvent::KeyRelease { key } => key,
                            };

                            // Simulate the filtering logic from ChuniioProxyServer
                            if !key.starts_with("CHUNIIO_") {
                                // This backend should skip non-CHUNIIO events
                                continue;
                            }

                            let event_key = format!("chuniio_{key}");
                            let mut counter = chuniio_counter.lock().await;
                            *counter.entry(event_key).or_insert(0) += 1;
                        }
                    }
                } else {
                    break;
                }
            }
        });

        // Send test events
        let test_events = vec![
            InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "CHUNIIO_SLIDER_1".to_string(),
            }),
            InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                key: "CHUNIIO_SLIDER_1".to_string(),
            }),
            InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "KEY_A".to_string(),
            }),
            InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                key: "KEY_A".to_string(),
            }),
            InputEvent::Keyboard(KeyboardEvent::KeyPress {
                key: "CHUNIIO_TEST".to_string(),
            }),
            InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                key: "CHUNIIO_TEST".to_string(),
            }),
        ];

        for event in test_events {
            let packet = InputEventPacket {
                device_id: "test_device".to_string(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                events: vec![event],
            };

            input_stream
                .send(packet)
                .await
                .expect("Failed to send test packet");
        }

        // Wait for both backends to process events
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Stop the backends
        uinput_handle.abort();
        chuniio_handle.abort();

        // Check results
        let final_counts = processed_events.lock().await;
        println!("Processed events: {:?}", *final_counts);

        // Verify that CHUNIIO events went to chuniio backend
        let chuniio_slider_count = final_counts.get("chuniio_CHUNIIO_SLIDER_1").unwrap_or(&0);
        let chuniio_test_count = final_counts.get("chuniio_CHUNIIO_TEST").unwrap_or(&0);

        // Verify that standard events went to uinput backend
        let uinput_key_a_count = final_counts.get("uinput_KEY_A").unwrap_or(&0);

        println!("CHUNIIO_SLIDER_1 events processed by chuniio backend: {chuniio_slider_count}");
        println!("CHUNIIO_TEST events processed by chuniio backend: {chuniio_test_count}");
        println!("KEY_A events processed by uinput backend: {uinput_key_a_count}");

        // With the broadcast fix: Each backend should receive all events and filter appropriately
        // CHUNIIO events should now reach the chuniio backend, while standard events go to uinput
        assert_eq!(
            *chuniio_slider_count, 2,
            "CHUNIIO_SLIDER_1 events (KeyPress + KeyRelease) should now be received by chuniio backend"
        );
        assert_eq!(
            *chuniio_test_count, 2,
            "CHUNIIO_TEST events (KeyPress + KeyRelease) should now be received by chuniio backend"
        );
        assert_eq!(
            *uinput_key_a_count, 2,
            "KEY_A events (KeyPress + KeyRelease) should be processed correctly by uinput backend"
        );
    }
}
