//! Device filtering and transformation module
//!
//! This module provides per-device filtering and key remapping functionality.
//! It acts as an intermediate layer between input and output backends,
//! allowing custom keycodes to be remapped to standard evdev codes
//! and routing events to specific output backends based on device configuration.

use crate::config::{AppConfig, DeviceConfig};
use crate::input::{AnalogEvent, InputEvent, InputEventPacket, KeyboardEvent};
use eyre::Result;
use std::collections::HashMap;
use std::fmt;
use tracing::{debug, trace, warn};

// Let's add an expression engine for our keys :3

/// Device filter that applies per-device transformations and routing
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
                // New: expand events if remapped to KeyExpr::Combo/Sequence
                let expanded = self.expand_event(event, config)?;
                filtered_events.extend(expanded);
            }
            packet.events = filtered_events;
        } else {
            // debug!(
            //     "No device config found for device '{}', using default transformation",
            //     packet.device_id
            // );
        }

        Ok(packet)
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
            // Analog and other events: unchanged for now
            _ => Ok(vec![event]),
        }
    }

    /// Transform and filter a single input event based on device configuration
    /// Returns true if the event should be kept, false if it should be filtered out
    fn transform_and_filter_event(
        &self,
        event: &mut InputEvent,
        config: &DeviceConfig,
    ) -> Result<bool> {
        match event {
            InputEvent::Keyboard(keyboard_event) => {
                self.transform_and_filter_keyboard_event(keyboard_event, config)
            }
            InputEvent::Analog(analog_event) => {
                self.transform_and_filter_analog_event(analog_event, config)
            }
            InputEvent::Pointer(_) => {
                // Pointer events don't typically need key remapping
                // but could be extended in the future for device-specific transformations
                // For now, always allow pointer events through
                Ok(true)
            }
            InputEvent::Joystick(_) => {
                // Joystick events could be remapped to keyboard events
                // or other transformations in the future
                // For now, always allow joystick events through
                Ok(true)
            }
        }
    }

    // /// Transform a single input event based on device configuration (legacy method for compatibility)
    // fn transform_event(&self, event: &mut InputEvent, config: &DeviceConfig) -> Result<()> {
    //     self.transform_and_filter_event(event, config)?;
    //     Ok(())
    // }

    /// Transform keyboard events based on device remapping rules and whitelist settings
    /// Returns true if the event should be kept, false if it should be filtered out
    fn transform_and_filter_keyboard_event(
        &self,
        event: &mut KeyboardEvent,
        config: &DeviceConfig,
    ) -> Result<bool> {
        let key = match event {
            KeyboardEvent::KeyPress { key } => key,
            KeyboardEvent::KeyRelease { key } => key,
        };

        // Check if this key needs to be remapped
        if let Some(remapped_expr) = config.remap.get(key) {
            // For the legacy single-event transform, only handle Single expressions
            match remapped_expr {
                KeyExpr::Single(remapped_key) => {
                    trace!("Remapping key '{}' to '{}' for device", key, remapped_key);
                    *key = remapped_key.clone();
                    return Ok(true); // Always keep remapped keys
                }
                _ => {
                    warn!(
                        "Complex KeyExpr found in legacy transform method - use expand_event instead"
                    );
                    return Ok(true); // Keep the original key for now
                }
            }
        }

        // Handle whitelist mode
        if config.remap_whitelist {
            // In whitelist mode, only allow keys that are in the remap table
            trace!("Filtering out key '{}' (not in whitelist) for device", key);
            return Ok(false); // Filter out keys not in remap table
        }

        // In non-whitelist mode, check if it's a custom key without remapping
        if !Self::is_standard_evdev_key(key) {
            // If it's not a standard evdev key and no remapping is defined, warn
            warn!(
                "Custom key '{}' has no remapping defined for device. Event may be ignored by output backend.",
                key
            );
        }

        Ok(true) // Keep the event in non-whitelist mode
    }

    fn transform_and_filter_analog_event(
        &self,
        event: &mut AnalogEvent,
        config: &DeviceConfig,
    ) -> Result<bool> {
        // Check if this key needs to be remapped
        if let Some(remapped_expr) = config.remap.get(&event.keycode) {
            // For the legacy single-event transform, only handle Single expressions
            match remapped_expr {
                KeyExpr::Single(remapped_key) => {
                    trace!(
                        "Remapping analog key '{}' to '{}' for device",
                        event.keycode, remapped_key
                    );
                    event.keycode = remapped_key.clone();
                    return Ok(true); // Always keep remapped keys
                }
                _ => {
                    warn!(
                        "Complex KeyExpr found in legacy transform method - use expand_event instead"
                    );
                    return Ok(true); // Keep the original key for now
                }
            }
        }

        // Handle whitelist mode
        if config.remap_whitelist {
            // In whitelist mode, only allow keys that are in the remap table
            debug!(
                "Filtering out analog key '{}' (not in whitelist) for device",
                event.keycode
            );
            return Ok(false); // Filter out keys not in remap table
        }

        Ok(true) // Keep the event in non-whitelist mode
    }

    // /// Transform keyboard events based on device remapping rules (legacy method for compatibility)
    // fn transform_keyboard_event(
    //     &self,
    //     event: &mut KeyboardEvent,
    //     config: &DeviceConfig,
    // ) -> Result<()> {
    //     self.transform_and_filter_keyboard_event(event, config)?;
    //     Ok(())
    // }

    /// Check if a key string appears to be a standard evdev key code
    pub fn is_standard_evdev_key(key: &str) -> bool {
        // Standard evdev keys typically start with KEY_, BTN_, etc.
        key.starts_with("KEY_")
            || key.starts_with("BTN_")
            || key.starts_with("ABS_")
            || key.starts_with("REL_")
    }

    /// Get the target backend for a specific device
    #[cfg(test)]
    pub fn get_device_backend(&self, device_id: &str) -> Option<&str> {
        self.device_configs
            .get(device_id)
            .map(|config| config.map_backend.as_str())
    }

    /// Get the device type for a specific device
    #[cfg(test)]
    pub fn get_device_type(&self, device_id: &str) -> Option<&str> {
        self.device_configs
            .get(device_id)
            .map(|config| config.device_type.as_str())
    }

    /// Check if a device has any configuration
    #[cfg(test)]
    pub fn has_device_config(&self, device_id: &str) -> bool {
        self.device_configs.contains_key(device_id)
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
