//! Device filtering and transformation module
//!
//! This module provides per-device filtering and key remapping functionality.
//! It focuses purely on transforming input events (key remapping, combos, sequences)
//! while leaving routing decisions to the EventRouter in the backend.

use crate::config::{AppConfig, DeviceConfig};
use crate::input::{InputEvent, InputEventPacket, KeyboardEvent};
use eyre::Result;
use std::collections::HashMap;
use std::fmt;
use tracing::debug;

/// Device filter that applies per-device transformations
#[derive(Clone)]
pub struct DeviceFilter {
    /// Device configurations mapped by device ID
    device_configs: HashMap<String, DeviceConfig>,
}

impl DeviceFilter {
    /// Create a new device filter from application configuration
    pub fn new(config: &AppConfig) -> Self {
        Self {
            device_configs: config.device.clone(),
        }
    }

    /// Transform an input event packet according to device-specific rules
    /// This focuses purely on transforming events (remapping keys, expanding combos)
    /// without making routing decisions
    pub fn transform_packet(&self, mut packet: InputEventPacket) -> Result<InputEventPacket> {
        // Get device configuration for this device ID
        let device_config = self.device_configs.get(&packet.device_id);

        if let Some(config) = device_config {
            debug!(
                "Applying device config for device '{}': backend={}, type={}, whitelist={}",
                packet.device_id, config.map_backend, config.device_type, config.remap_whitelist
            );

            // Transform and filter events based on device configuration
            let mut filtered_events = Vec::new();
            for event in packet.events.into_iter() {
                // Expand events if remapped to KeyExpr::Combo/Sequence
                let expanded = self.expand_event(event, config)?;
                filtered_events.extend(expanded);
            }
            packet.events = filtered_events;
        }

        Ok(packet)
    }

    /// Get the configured backend for a device (used by EventRouter)
    pub fn get_device_backend(&self, device_id: &str) -> Option<&str> {
        self.device_configs
            .get(device_id)
            .map(|config| config.map_backend.as_str())
    }

    /// Get the configured device type for a device
    pub fn get_device_type(&self, device_id: &str) -> Option<&str> {
        self.device_configs
            .get(device_id)
            .map(|config| config.device_type.as_str())
    }

    /// Check if a device has any configuration
    pub fn has_device_config(&self, device_id: &str) -> bool {
        self.device_configs.contains_key(device_id)
    }

    /// Expand an input event according to remapping rules (support KeyExpr)
    fn expand_event(&self, event: InputEvent, config: &DeviceConfig) -> Result<Vec<InputEvent>> {
        match event {
            InputEvent::Keyboard(mut keyboard_event) => {
                let key = match &mut keyboard_event {
                    KeyboardEvent::KeyPress { key } => key,
                    KeyboardEvent::KeyRelease { key } => key,
                };
                if let Some(expr) = config.remap.get(key) {
                    match expr {
                        KeyExpr::Single(remap) => {
                            *key = remap.clone();
                            Ok(vec![InputEvent::Keyboard(keyboard_event)])
                        }
                        KeyExpr::Combo(keys) => {
                            // For combos, emit multiple KeyPress/KeyRelease events with the same timestamp
                            let events = keys
                                .iter()
                                .map(|k| match &keyboard_event {
                                    KeyboardEvent::KeyPress { .. } => {
                                        InputEvent::Keyboard(KeyboardEvent::KeyPress {
                                            key: k.clone(),
                                        })
                                    }
                                    KeyboardEvent::KeyRelease { .. } => {
                                        InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                                            key: k.clone(),
                                        })
                                    }
                                })
                                .collect();
                            Ok(events)
                        }
                        KeyExpr::Sequence(keys) => {
                            // For sequences, emit events in order
                            let events = keys
                                .iter()
                                .map(|k| match &keyboard_event {
                                    KeyboardEvent::KeyPress { .. } => {
                                        InputEvent::Keyboard(KeyboardEvent::KeyPress {
                                            key: k.clone(),
                                        })
                                    }
                                    KeyboardEvent::KeyRelease { .. } => {
                                        InputEvent::Keyboard(KeyboardEvent::KeyRelease {
                                            key: k.clone(),
                                        })
                                    }
                                })
                                .collect();
                            Ok(events)
                        }
                    }
                } else if config.remap_whitelist {
                    // Not in whitelist, filter out
                    Ok(vec![])
                } else {
                    Ok(vec![InputEvent::Keyboard(keyboard_event)])
                }
            }
            // Analog and other events: pass through unchanged for now
            _ => Ok(vec![event]),
        }
    }

    /// Check if a key string appears to be a standard evdev key code
    pub fn is_standard_evdev_key(key: &str) -> bool {
        // Standard evdev keys typically start with KEY_, BTN_, etc.
        key.starts_with("KEY_")
            || key.starts_with("BTN_")
            || key.starts_with("ABS_")
            || key.starts_with("REL_")
    }
}

/// Key expression for remapping: can be a single key, a combination (chord), or a sequence
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KeyExpr {
    Single(String),
    Combo(Vec<String>),    // e.g., multiple keys pressed together
    Sequence(Vec<String>), // e.g., keys pressed in order
}

impl KeyExpr {
    /// Parse a key expression from a string (simple format: 'KEY_A', 'KEY_A+KEY_B', 'KEY_A,KEY_B')
    pub fn parse(expr: &str) -> Self {
        if expr.contains(",") {
            KeyExpr::Sequence(expr.split(',').map(|s| s.trim().to_string()).collect())
        } else if expr.contains("+") {
            KeyExpr::Combo(expr.split('+').map(|s| s.trim().to_string()).collect())
        } else {
            KeyExpr::Single(expr.trim().to_string())
        }
    }
}

impl fmt::Display for KeyExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyExpr::Single(k) => write!(f, "{k}"),
            KeyExpr::Combo(keys) => write!(f, "{}", keys.join("+")),
            KeyExpr::Sequence(keys) => write!(f, "{}", keys.join(",")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DeviceConfig};
    use crate::input::{InputEvent, InputEventPacket, KeyboardEvent};
    use std::collections::HashMap;

    fn create_test_config() -> AppConfig {
        let mut device_configs = HashMap::new();

        // Create a test device config with remapping
        let mut remap = HashMap::new();
        remap.insert("SLIDER_1".to_string(), KeyExpr::Single("KEY_A".to_string()));
        remap.insert("SLIDER_2".to_string(), KeyExpr::Single("KEY_B".to_string()));
        remap.insert(
            "GAME_1".to_string(),
            KeyExpr::Single("KEY_SPACE".to_string()),
        );

        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap,
            remap_whitelist: false,
        };

        device_configs.insert("test_device".to_string(), device_config);

        AppConfig {
            device: device_configs,
            ..Default::default()
        }
    }

    #[test]
    fn test_device_filter_creation() {
        let config = create_test_config();
        let filter = DeviceFilter::new(&config);

        assert!(filter.has_device_config("test_device"));
        assert!(!filter.has_device_config("nonexistent_device"));
        assert_eq!(filter.get_device_backend("test_device"), Some("uinput"));
        assert_eq!(filter.get_device_type("test_device"), Some("keyboard"));
    }

    #[test]
    fn test_key_remapping() {
        let config = create_test_config();
        let filter = DeviceFilter::new(&config);

        // Create a test packet with custom keys
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(),
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
            key: "GAME_1".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Check that keys were remapped
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_A");
            }
            _ => panic!("Expected KeyPress event"),
        }

        match &transformed_packet.events[1] {
            InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }) => {
                assert_eq!(key, "KEY_SPACE");
            }
            _ => panic!("Expected KeyRelease event"),
        }
    }

    #[test]
    fn test_standard_keys_passthrough() {
        let config = create_test_config();
        let filter = DeviceFilter::new(&config);

        // Create a packet with standard evdev keys
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "KEY_A".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Standard keys should pass through unchanged
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_A");
            }
            _ => panic!("Expected KeyPress event"),
        }
    }

    #[test]
    fn test_unconfigured_device() {
        let config = create_test_config();
        let filter = DeviceFilter::new(&config);

        // Create a packet for a device without configuration
        let mut packet = InputEventPacket::new("unknown_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Keys should remain unchanged for unconfigured devices
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "SLIDER_1");
            }
            _ => panic!("Expected KeyPress event"),
        }
    }

    #[test]
    fn test_is_standard_evdev_key() {
        assert!(DeviceFilter::is_standard_evdev_key("KEY_A"));
        assert!(DeviceFilter::is_standard_evdev_key("BTN_LEFT"));
        assert!(DeviceFilter::is_standard_evdev_key("ABS_X"));
        assert!(DeviceFilter::is_standard_evdev_key("REL_X"));

        assert!(!DeviceFilter::is_standard_evdev_key("SLIDER_1"));
        assert!(!DeviceFilter::is_standard_evdev_key("GAME_1"));
        assert!(!DeviceFilter::is_standard_evdev_key("CUSTOM_KEY"));
    }

    #[test]
    fn test_remap_whitelist_enabled_with_mappings() {
        let mut device_configs = HashMap::new();

        // Create a test device config with whitelist enabled
        let mut remap = HashMap::new();
        remap.insert("SLIDER_1".to_string(), KeyExpr::Single("KEY_A".to_string()));
        remap.insert(
            "GAME_1".to_string(),
            KeyExpr::Single("KEY_SPACE".to_string()),
        );

        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap,
            remap_whitelist: true,
        };

        device_configs.insert("test_device".to_string(), device_config);

        let config = AppConfig {
            device: device_configs,
            ..Default::default()
        };

        let filter = DeviceFilter::new(&config);

        // Create a packet with mixed keys - some in remap table, some not
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(), // Should be remapped and kept
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_2".to_string(), // Should be filtered out (not in remap table)
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyRelease {
            key: "GAME_1".to_string(), // Should be remapped and kept
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "KEY_B".to_string(), // Standard key but should be filtered out (not in remap table)
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Should only have 2 events (the ones in the remap table)
        assert_eq!(transformed_packet.events.len(), 2);

        // Check that the kept events were remapped correctly
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_A"); // SLIDER_1 -> KEY_A
            }
            _ => panic!("Expected KeyPress event"),
        }

        match &transformed_packet.events[1] {
            InputEvent::Keyboard(KeyboardEvent::KeyRelease { key }) => {
                assert_eq!(key, "KEY_SPACE"); // GAME_1 -> KEY_SPACE
            }
            _ => panic!("Expected KeyRelease event"),
        }
    }

    #[test]
    fn test_remap_whitelist_enabled_no_mappings() {
        let mut device_configs = HashMap::new();

        // Create a test device config with whitelist enabled but no remap table
        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap: HashMap::new(), // Empty remap table
            remap_whitelist: true,
        };

        device_configs.insert("test_device".to_string(), device_config);

        let config = AppConfig {
            device: device_configs,
            ..Default::default()
        };

        let filter = DeviceFilter::new(&config);

        // Create a packet with various keys
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(),
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "KEY_A".to_string(),
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "BTN_LEFT".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // All events should be filtered out since whitelist is enabled but remap table is empty
        assert_eq!(transformed_packet.events.len(), 0);
    }

    #[test]
    fn test_remap_whitelist_disabled_default_behavior() {
        let mut device_configs = HashMap::new();

        // Create a test device config with whitelist disabled (default behavior)
        let mut remap = HashMap::new();
        remap.insert("SLIDER_1".to_string(), KeyExpr::Single("KEY_A".to_string()));

        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap,
            remap_whitelist: false, // Explicitly disabled
        };

        device_configs.insert("test_device".to_string(), device_config);

        let config = AppConfig {
            device: device_configs,
            ..Default::default()
        };

        let filter = DeviceFilter::new(&config);

        // Create a packet with mixed keys
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(), // Should be remapped
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_2".to_string(), // Should pass through unchanged
        }));
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "KEY_B".to_string(), // Should pass through unchanged
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // All events should be kept in non-whitelist mode
        assert_eq!(transformed_packet.events.len(), 3);

        // Check transformations
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_A"); // SLIDER_1 -> KEY_A
            }
            _ => panic!("Expected KeyPress event"),
        }

        match &transformed_packet.events[1] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "SLIDER_2"); // unchanged
            }
            _ => panic!("Expected KeyPress event"),
        }

        match &transformed_packet.events[2] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_B"); // unchanged
            }
            _ => panic!("Expected KeyPress event"),
        }
    }

    #[test]
    fn test_remap_whitelist_default_value() {
        // Test that remap_whitelist defaults to false
        let default_config = DeviceConfig::default();
        assert!(!default_config.remap_whitelist);
    }

    #[test]
    fn test_keyexpr_combo_expansion() {
        let mut device_configs = HashMap::new();

        // Create a test device config with combo mapping
        let mut remap = HashMap::new();
        remap.insert(
            "SLIDER_1".to_string(),
            KeyExpr::Combo(vec!["KEY_A".to_string(), "KEY_B".to_string()]),
        );

        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap,
            remap_whitelist: false,
        };

        device_configs.insert("test_device".to_string(), device_config);

        let config = AppConfig {
            device: device_configs,
            ..Default::default()
        };

        let filter = DeviceFilter::new(&config);

        // Create a test packet with a key that maps to a combo
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Should have 2 events (one for each key in the combo)
        assert_eq!(transformed_packet.events.len(), 2);

        // Check that both keys in the combo are present
        match &transformed_packet.events[0] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_A");
            }
            _ => panic!("Expected KeyPress event"),
        }

        match &transformed_packet.events[1] {
            InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                assert_eq!(key, "KEY_B");
            }
            _ => panic!("Expected KeyPress event"),
        }
    }

    #[test]
    fn test_keyexpr_sequence_expansion() {
        let mut device_configs = HashMap::new();

        // Create a test device config with sequence mapping
        let mut remap = HashMap::new();
        remap.insert(
            "SLIDER_1".to_string(),
            KeyExpr::Sequence(vec![
                "KEY_A".to_string(),
                "KEY_B".to_string(),
                "KEY_C".to_string(),
            ]),
        );

        let device_config = DeviceConfig {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap,
            remap_whitelist: false,
        };

        device_configs.insert("test_device".to_string(), device_config);

        let config = AppConfig {
            device: device_configs,
            ..Default::default()
        };

        let filter = DeviceFilter::new(&config);

        // Create a test packet with a key that maps to a sequence
        let mut packet = InputEventPacket::new("test_device".to_string(), 12345);
        packet.add_event(InputEvent::Keyboard(KeyboardEvent::KeyPress {
            key: "SLIDER_1".to_string(),
        }));

        let transformed_packet = filter.transform_packet(packet).unwrap();

        // Should have 3 events (one for each key in the sequence)
        assert_eq!(transformed_packet.events.len(), 3);

        // Check that all keys in the sequence are present in order
        let expected_keys = ["KEY_A", "KEY_B", "KEY_C"];
        for (i, expected_key) in expected_keys.iter().enumerate() {
            match &transformed_packet.events[i] {
                InputEvent::Keyboard(KeyboardEvent::KeyPress { key }) => {
                    assert_eq!(key, expected_key);
                }
                _ => panic!("Expected KeyPress event"),
            }
        }
    }

    #[test]
    fn test_keyexpr_parsing() {
        // Test single key
        let single = KeyExpr::parse("KEY_A");
        assert_eq!(single, KeyExpr::Single("KEY_A".to_string()));

        // Test combo
        let combo = KeyExpr::parse("KEY_A+KEY_B");
        assert_eq!(
            combo,
            KeyExpr::Combo(vec!["KEY_A".to_string(), "KEY_B".to_string()])
        );

        // Test sequence
        let sequence = KeyExpr::parse("KEY_A,KEY_B,KEY_C");
        assert_eq!(
            sequence,
            KeyExpr::Sequence(vec![
                "KEY_A".to_string(),
                "KEY_B".to_string(),
                "KEY_C".to_string()
            ])
        );

        // Test combo with spaces
        let combo_spaces = KeyExpr::parse(" KEY_A + KEY_B ");
        assert_eq!(
            combo_spaces,
            KeyExpr::Combo(vec!["KEY_A".to_string(), "KEY_B".to_string()])
        );

        // Test sequence with spaces
        let sequence_spaces = KeyExpr::parse(" KEY_A , KEY_B , KEY_C ");
        assert_eq!(
            sequence_spaces,
            KeyExpr::Sequence(vec![
                "KEY_A".to_string(),
                "KEY_B".to_string(),
                "KEY_C".to_string()
            ])
        );
    }

    #[test]
    fn test_keyexpr_to_string() {
        // Test single key
        let single = KeyExpr::Single("KEY_A".to_string());
        assert_eq!(format!("{single}"), "KEY_A");

        // Test combo
        let combo = KeyExpr::Combo(vec!["KEY_A".to_string(), "KEY_B".to_string()]);
        assert_eq!(format!("{combo}"), "KEY_A+KEY_B");

        // Test sequence
        let sequence = KeyExpr::Sequence(vec![
            "KEY_A".to_string(),
            "KEY_B".to_string(),
            "KEY_C".to_string(),
        ]);
        assert_eq!(format!("{sequence}"), "KEY_A,KEY_B,KEY_C");
    }
}
