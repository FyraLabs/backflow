//! Config modules for the application.

use crate::device_filter::KeyExpr;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    /// Per-device configurations for filtering and remapping
    #[serde(default)]
    pub device: HashMap<String, DeviceConfig>,
    /// External helper processes ("plugins") that Backflow will spawn and integrate as
    /// additional input/feedback peers.
    ///
    /// Each entry spawns one child process whose stdin/stdout speak the standard Backflow
    /// JSON line protocol (identical to the WebSocket + UDS transports). This lets you extend
    /// Backflow without modifying the core codebase – ideal for rapid prototyping of custom
    /// hardware bridges, converters, or experimental mappers.
    ///
    /// Basic example:
    ///
    /// ```toml
    /// [[plugins]]
    /// command = "/usr/local/bin/my-midi-adapter"
    /// args = ["--device", "hw:1,0,0"]
    ///
    /// [[plugins]]
    /// command = "./target/release/custom_source"
    /// # args = []   # (empty by default)
    /// ```
    ///
    /// Runtime behavior:
    /// - Backflow spawns the process with piped stdin/stdout (stderr is inherited for logs).
    /// - Each newline-delimited UTF‑8 JSON message coming from the plugin stdout is parsed as an
    ///   inbound event or control frame.
    /// - Backflow sends outbound messages (feedback, acknowledgements, etc.) as single-line JSON
    ///   objects to the plugin stdin.
    /// - If a plugin exits, Backflow currently logs the termination (no auto‑restart yet).
    ///
    /// Protocol expectations:
    /// - The same message envelopes used by existing transports (e.g. `{ "type": "input", ... }`).
    /// - Control messages like `UpdateInputDIDFilter` / `UpdateStreamRegistration` are supported.
    ///
    /// Notes & future extensions (not yet implemented – subject to change):
    /// - Optional per‑plugin working directory / environment overrides
    /// - Auto‑restart / backoff policies
    /// - Explicit plugin ID or stream namespace prefixes
    /// - Graceful shutdown signals
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
}

/// Configuration for a single plugin process.
///
/// A plugin is an external executable that speaks Backflow's line‑delimited JSON protocol over
/// its stdin/stdout. Backflow launches it, wires a `StdioBackend` to its pipes, and treats it as
/// another transport peer (just like a WebSocket or Unix domain socket client).
///
/// Minimal example (TOML):
/// ```toml
/// [[plugins]]
/// command = "./target/release/my-plugin"
/// args = ["--verbose"]
/// ```
///
/// Message format: identical to other transports. For instance an input batch:
/// ```json
/// {"type":"input","packet":{"device_id":"my_dev","timestamp":12345,"events":[{"key":"KEY_A","pressed":true}]}}
/// ```
///
/// Or a control frame to register for streams:
/// ```json
/// {"type":"control","control":{"command":"UpdateStreamRegistration","register_streams":["uinput"],"unregister_streams":[]}}
/// ```
///
/// Field notes:
/// - `command`: Executable path (absolute or relative to current working directory when Backflow starts).
/// - `args`: Optional CLI arguments (defaults to empty). Omit the field or use an empty array for none.
///
/// Lifecycle:
/// - Spawned once during backend initialization.
/// - On exit, Backflow logs the status; restart logic is not implemented.
///
/// Guidelines for plugin authors:
/// - Write one JSON object per line. Avoid buffering without flushing newlines.
/// - Treat unknown outbound message types as opaque (forward compatible).
/// - Keep stdout strictly protocol; send human logs to stderr.
/// - Exit cleanly on EOF from stdin (Backflow shutdown).
///
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PluginConfig {
    /// Path to the plugin executable
    pub command: String,
    /// Optional CLI arguments to pass to the plugin, if need be.
    #[serde(default)]
    pub args: Vec<String>,
}

impl AppConfig {
    pub fn from_toml_str(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Load configuration from a TOML file
    pub fn from_file<P: AsRef<std::path::Path>>(
        path: P,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: AppConfig = Self::from_toml_str(&contents)
            .map_err(|e| format!("Failed to parse config file: {e}"))?;
        Ok(config)
    }

    /// Load configuration with fallback to defaults
    pub fn load_or_default() -> Self {
        // Try to load from standard locations in order: CWD > .config > /etc
        let config_paths = [
            std::path::PathBuf::from("backflow.toml"),
            dirs::config_dir()
                .map(|config_dir| config_dir.join("backflow.toml"))
                .unwrap_or_else(|| PathBuf::from("backflow.toml")),
            std::path::PathBuf::from("/etc/backflow/backflow.toml"),
        ];

        for path in &config_paths {
            if path.exists() {
                match Self::from_file(path) {
                    Ok(mut config) => {
                        tracing::info!("Loaded configuration from: {}", path.display());
                        config.validate_and_fix();
                        return config;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to load config from {}: {}. Trying next location.",
                            path.display(),
                            e
                        );
                    }
                }
                // Only try the first existing config file
                break;
            }
        }

        tracing::info!("No configuration file found, using defaults");
        let mut config = Self::default();
        config.validate_and_fix();
        config
    }

    /// Validate and fix configuration inconsistencies
    pub fn validate_and_fix(&mut self) {
        // Check if chuniio_proxy is enabled
        let chuniio_proxy_enabled = self
            .output
            .chuniio_proxy
            .as_ref()
            .map(|config| config.enabled)
            .unwrap_or(false);

        if chuniio_proxy_enabled {
            // If chuniio_proxy is enabled, we should not read from a socket for feedback
            // Clear the socket_path if it's configured
            if let Some(ref mut chuniio_feedback) = self.feedback.chuniio {
                if chuniio_feedback.socket_path.is_some() {
                    tracing::warn!(
                        "ChuniIO feedback socket_path ({:?}) will be ignored because chuniio_proxy is enabled. Feedback will be fed from chuniio_proxy instead.",
                        chuniio_feedback.socket_path
                    );
                    chuniio_feedback.socket_path = None;
                }
            } else {
                // Auto-enable chuniio feedback with default settings when chuniio_proxy is enabled
                tracing::info!("Auto-enabling ChuniIO feedback because chuniio_proxy is enabled");
                self.feedback.chuniio = Some(ChuniIoRgbConfig::default());
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InputConfig {
    #[serde(default = "default_web_enabled")]
    pub web: Option<WebBackend>,
    #[serde(default)]
    pub unix: Option<UnixDomainSocketConfig>,
    #[serde(default)]
    pub brokenithm: Option<BrokenithmConfig>,
    // #[serde(default)]
    // pub chuniio: Option<ChuniIoSerialConfig>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            web: default_web_enabled(),
            unix: None,
            brokenithm: None,
        }
    }
}

fn default_web_enabled() -> Option<WebBackend> {
    Some(WebBackend::default())
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct OutputConfig {
    #[serde(default)]
    pub uinput: UInputConfig,
    #[serde(default)]
    pub chuniio_proxy: Option<ChuniioProxyConfig>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct UnixDomainSocketConfig {
    #[serde(default = "default_unix_socket_path")]
    pub path: PathBuf,
}
fn default_unix_socket_path() -> PathBuf {
    use std::env;
    // Check environment variable first
    if let Ok(env_path) = env::var("BACKFLOW_UNIX_SOCKET") {
        return PathBuf::from(env_path);
    }
    let uid = nix::unistd::Uid::effective().as_raw();
    if uid == 0 {
        // If running as root, use /run/backflow directly
        PathBuf::from("/run/backflow")
    } else {
        PathBuf::from(format!("/run/user/{uid}/backflow"))
    }
}

// set web.enabled = false in [input.web] to explicitly disable the web backend
#[derive(Debug, Deserialize, Serialize)]
pub struct WebBackend {
    #[serde(default = "default_web_enabled_bool")]
    pub enabled: bool,
    #[serde(default = "default_web_port")]
    pub port: u16,
    #[serde(default = "default_web_host")]
    pub host: String,
}

impl Default for WebBackend {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8000,
            host: "0.0.0.0".to_string(),
        }
    }
}

fn default_web_enabled_bool() -> bool {
    true
}

fn default_web_port() -> u16 {
    8000
}

fn default_web_host() -> String {
    "0.0.0.0".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UInputConfig {
    pub enabled: bool,
}

impl Default for UInputConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct FeedbackConfig {
    // CHUNIIO RGB feedback socket
    pub chuniio: Option<ChuniIoRgbConfig>,
    // pub rgb:
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChuniIoRgbConfig {
    /// Path to the Unix domain socket for ChuniIo RGB feedback, usually from Outflow bridge from inside Wine
    /// Optional - if not specified, will use chuniio_proxy data directly
    pub socket_path: Option<PathBuf>,
    /// Number of RGB outputs to clamp to
    /// Default will be at 32
    #[serde(default = "default_slider_lights")]
    pub slider_clamp_lights: u32,

    /// The offset of the light ID, defaults to 0 (no offset, emit light events from 0-31)
    /// Useful if you want to route to specific lights
    #[serde(default)]
    pub slider_id_offset: u32,
}

impl Default for ChuniIoRgbConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            slider_clamp_lights: default_slider_lights(),
            slider_id_offset: 0,
        }
    }
}

fn default_slider_lights() -> u32 {
    32
}

/// Configuration for a specific device, including backend routing and key remapping
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceConfig {
    /// Which output backend this device should route to (e.g., "uinput", "inputplumber")
    pub map_backend: String,
    /// The type of device this represents (e.g., "keyboard", "mouse", "gamepad")
    pub device_type: String,
    /// Key remapping from custom keys to evdev codes or expressions
    #[serde(default, with = "keyexpr_remap_serde")]
    pub remap: HashMap<String, KeyExpr>,
    /// When true, only keys defined in the remap table are allowed through (whitelist mode)
    /// When false (default), undefined keys pass through unchanged
    #[serde(default)]
    pub remap_whitelist: bool,
}

mod keyexpr_remap_serde {
    use super::KeyExpr;
    use serde::{Deserializer, Serializer};
    use std::collections::HashMap;

    use serde::de::{self, MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use std::fmt;

    pub fn serialize<S>(map: &HashMap<String, KeyExpr>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut ser_map = serializer.serialize_map(Some(map.len()))?;
        for (k, v) in map {
            match v {
                KeyExpr::Combo(keys) => {
                    ser_map.serialize_entry(k, keys)?;
                }
                _ => {
                    ser_map.serialize_entry(k, &format!("{v}"))?;
                }
            }
        }
        ser_map.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<String, KeyExpr>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeyExprMapVisitor;
        impl<'de> Visitor<'de> for KeyExprMapVisitor {
            type Value = HashMap<String, KeyExpr>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map of key remappings")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut map = HashMap::new();
                while let Some((k, v)) = access.next_entry::<String, serde_json::Value>()? {
                    let expr = match v {
                        serde_json::Value::String(s) => KeyExpr::parse(&s),
                        serde_json::Value::Array(arr) => {
                            let keys: Vec<String> = arr
                                .into_iter()
                                .map(|v| v.as_str().unwrap_or("").to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if keys.len() == 1 {
                                KeyExpr::Single(keys[0].clone())
                            } else {
                                KeyExpr::Combo(keys)
                            }
                        }
                        _ => return Err(de::Error::custom("Invalid value for KeyExpr")),
                    };
                    map.insert(k, expr);
                }
                Ok(map)
            }
        }
        deserializer.deserialize_map(KeyExprMapVisitor)
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            map_backend: "uinput".to_string(),
            device_type: "keyboard".to_string(),
            remap: HashMap::new(),
            remap_whitelist: false,
        }
    }
}
#[derive(Debug, Deserialize, Serialize)]
pub struct ChuniioProxyConfig {
    #[serde(default = "default_chuniio_proxy_enabled")]
    pub enabled: bool,
    #[serde(default = "default_chuniio_proxy_socket_path")]
    pub socket_path: PathBuf,
}

impl Default for ChuniioProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: default_chuniio_proxy_socket_path(),
        }
    }
}

fn default_chuniio_proxy_enabled() -> bool {
    false
}

fn default_chuniio_proxy_socket_path() -> PathBuf {
    use std::env;
    // Check environment variable first
    if let Ok(env_path) = env::var("CHUNIIO_PROXY_SOCKET") {
        return PathBuf::from(env_path);
    }
    // Try to use user runtime directory, fallback to /tmp
    let uid = nix::unistd::Uid::effective().as_raw();
    let runtime_dir = format!("/run/user/{uid}");
    let runtime_path = format!("{runtime_dir}/backflow_chuniio");
    if std::path::Path::new(&runtime_dir).exists() {
        PathBuf::from(runtime_path)
    } else {
        PathBuf::from("/tmp/backflow_chuniio")
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct BrokenithmUdpConfig {
    #[serde(default = "default_brokenithm_enabled")]
    pub enabled: bool,
    #[serde(default = "default_brokenithm_port")]
    pub port: u16,
    #[serde(default = "default_brokenithm_host")]
    pub host: String,
}

fn default_brokenithm_enabled() -> bool {
    true
}
fn default_brokenithm_port() -> u16 {
    24864
}
fn default_brokenithm_host() -> String {
    "0.0.0.0".to_string()
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct BrokenithmIdeviceConfig {
    #[serde(default = "default_brokenithm_enabled")]
    pub enabled: bool,
    #[serde(default = "default_brokenithm_port")]
    pub device_port: u16,
    #[serde(default = "default_brokenithm_port")]
    pub local_port: u16,
    #[serde(default)]
    pub udid: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct BrokenithmConfig {
    #[serde(default = "default_brokenithm_enabled")]
    pub enabled: bool,
    #[serde(default = "default_brokenithm_port")]
    pub port: u16,
    #[serde(default = "default_brokenithm_host")]
    pub host: String,
    #[serde(default)]
    pub idevice: Option<BrokenithmIdeviceConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_app_config() {
        let config = AppConfig::default();
        // InputConfig.web should be Some(WebBackend::default())
        let web = config.input.web.as_ref().unwrap();
        assert!(web.enabled);
        assert_eq!(web.port, 8000);
        assert_eq!(web.host, "0.0.0.0");
        // OutputConfig.uinput.enabled should be true
        assert!(config.output.uinput.enabled);
        // FeedbackConfig.chuniio should be None
        assert!(config.feedback.chuniio.is_none());
    }

    #[test]
    fn test_disable_web_backend() {
        let toml_str = r#"
            [input.web]
            enabled = false
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let web = config.input.web.unwrap();
        assert!(!web.enabled);
    }

    #[test]
    fn test_custom_web_backend() {
        let toml_str = r#"
            [input.web]
            port = 1234
            host = "127.0.0.1"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let web = config.input.web.unwrap();
        assert!(web.enabled); // Should default to true
        assert_eq!(web.port, 1234);
        assert_eq!(web.host, "127.0.0.1");
    }

    #[test]
    fn test_default_chuniio_rgb_config() {
        let toml_str = r#"
            [feedback.chuniio]
            socket_path = "/tmp/chuniio.sock"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let chuniio = config.feedback.chuniio.unwrap();
        assert_eq!(
            chuniio.socket_path,
            Some(PathBuf::from("/tmp/chuniio.sock"))
        );
        assert_eq!(chuniio.slider_clamp_lights, 32);
        assert_eq!(chuniio.slider_id_offset, 0);
    }

    #[test]
    fn test_custom_chuniio_rgb_config() {
        let toml_str = r#"
            [feedback.chuniio]
            socket_path = "/tmp/chuniio.sock"
            slider_clamp_lights = 16
            slider_id_offset = 2
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let chuniio = config.feedback.chuniio.unwrap();
        assert_eq!(
            chuniio.socket_path,
            Some(PathBuf::from("/tmp/chuniio.sock"))
        );
        assert_eq!(chuniio.slider_clamp_lights, 16);
        assert_eq!(chuniio.slider_id_offset, 2);
    }

    #[test]
    fn test_chuniio_rgb_config_no_socket() {
        let toml_str = r#"
            [feedback.chuniio]
            slider_clamp_lights = 16
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let chuniio = config.feedback.chuniio.unwrap();
        assert_eq!(chuniio.socket_path, None);
        assert_eq!(chuniio.slider_clamp_lights, 16);
        assert_eq!(chuniio.slider_id_offset, 0);
    }

    #[test]
    fn test_default_uinput_config() {
        let config = OutputConfig::default();
        assert!(config.uinput.enabled);
    }

    #[test]
    fn test_disable_uinput() {
        let toml_str = r#"
            [output.uinput]
            enabled = false
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.output.uinput.enabled);
    }

    #[test]
    fn test_device_config_basic() {
        let toml_str = r#"
            [device."test_device"]
            map_backend = "uinput"
            device_type = "keyboard"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("test_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");
        assert!(device_config.remap.is_empty());
    }

    #[test]
    fn test_device_config_with_remapping() {
        let toml_str = r#"
            [device."slider_device"]
            map_backend = "uinput"
            device_type = "keyboard"

            [device."slider_device".remap]
            "SLIDER_1" = "KEY_A"
            "SLIDER_2" = "KEY_B"
            "GAME_1" = "KEY_SPACE"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("slider_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");
        assert_eq!(
            device_config.remap.get("SLIDER_1"),
            Some(&KeyExpr::Single("KEY_A".to_string()))
        );
        assert_eq!(
            device_config.remap.get("SLIDER_2"),
            Some(&KeyExpr::Single("KEY_B".to_string()))
        );
        assert_eq!(
            device_config.remap.get("GAME_1"),
            Some(&KeyExpr::Single("KEY_SPACE".to_string()))
        );
    }

    #[test]
    fn test_multiple_devices() {
        let toml_str = r#"
            [device."keyboard_device"]
            map_backend = "uinput"
            device_type = "keyboard"

            [device."gamepad_device"]
            map_backend = "inputplumber"
            device_type = "gamepad"

            [device."gamepad_device".remap]
            "BUTTON_A" = "BTN_A"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();

        let keyboard_config = config.device.get("keyboard_device").unwrap();
        assert_eq!(keyboard_config.map_backend, "uinput");
        assert_eq!(keyboard_config.device_type, "keyboard");

        let gamepad_config = config.device.get("gamepad_device").unwrap();
        assert_eq!(gamepad_config.map_backend, "inputplumber");
        assert_eq!(gamepad_config.device_type, "gamepad");
        assert_eq!(
            gamepad_config.remap.get("BUTTON_A"),
            Some(&KeyExpr::Single("BTN_A".to_string()))
        );
    }

    #[test]
    fn test_device_example_config_format() {
        let toml_str = r#"
            [input.web]
            enabled = true
            port = 8000
            host = "0.0.0.0"

            [output.uinput]
            enabled = true

            [device."slider_controller"]
            map_backend = "uinput"
            device_type = "keyboard"

            [device."slider_controller".remap]
            "SLIDER_1" = "KEY_A"
            "SLIDER_2" = "KEY_S"
            "SLIDER_3" = "KEY_D"

            [device."custom_gamepad"]
            map_backend = "uinput"
            device_type = "keyboard"

            [device."custom_gamepad".remap]
            "GAME_1" = "KEY_SPACE"
            "BUTTON_A" = "KEY_Z"
        "#;

        let config: AppConfig = toml::from_str(toml_str).unwrap();

        // Test slider controller
        let slider_config = config.device.get("slider_controller").unwrap();
        assert_eq!(slider_config.map_backend, "uinput");
        assert_eq!(slider_config.device_type, "keyboard");
        assert_eq!(
            slider_config.remap.get("SLIDER_1"),
            Some(&KeyExpr::Single("KEY_A".to_string()))
        );
        assert_eq!(
            slider_config.remap.get("SLIDER_2"),
            Some(&KeyExpr::Single("KEY_S".to_string()))
        );
        assert_eq!(
            slider_config.remap.get("SLIDER_3"),
            Some(&KeyExpr::Single("KEY_D".to_string()))
        );
        assert!(!slider_config.remap_whitelist); // Should default to false

        // Test custom gamepad
        let gamepad_config = config.device.get("custom_gamepad").unwrap();
        assert_eq!(gamepad_config.map_backend, "uinput");
        assert_eq!(gamepad_config.device_type, "keyboard");
        assert_eq!(
            gamepad_config.remap.get("GAME_1"),
            Some(&KeyExpr::Single("KEY_SPACE".to_string()))
        );
        assert_eq!(
            gamepad_config.remap.get("BUTTON_A"),
            Some(&KeyExpr::Single("KEY_Z".to_string()))
        );
        assert!(!gamepad_config.remap_whitelist); // Should default to false

        // Test other configuration sections remain working
        assert!(config.input.web.is_some());
        assert!(config.output.uinput.enabled);
    }

    #[test]
    fn test_device_config_with_whitelist_enabled() {
        let toml_str = r#"
            [device."whitelist_device"]
            map_backend = "uinput"
            device_type = "keyboard"
            remap_whitelist = true

            [device."whitelist_device".remap]
            "SLIDER_1" = "KEY_A"
            "GAME_1" = "KEY_SPACE"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("whitelist_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");
        assert!(device_config.remap_whitelist);
        assert_eq!(
            device_config.remap.get("SLIDER_1"),
            Some(&KeyExpr::Single("KEY_A".to_string()))
        );
        assert_eq!(
            device_config.remap.get("GAME_1"),
            Some(&KeyExpr::Single("KEY_SPACE".to_string()))
        );
    }

    #[test]
    fn test_device_config_with_whitelist_enabled_no_remap() {
        let toml_str = r#"
            [device."ignore_all_device"]
            map_backend = "uinput"
            device_type = "keyboard"
            remap_whitelist = true
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("ignore_all_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");
        assert!(device_config.remap_whitelist);
        assert!(device_config.remap.is_empty());
    }

    #[test]
    fn test_device_config_with_whitelist_explicitly_disabled() {
        let toml_str = r#"
            [device."passthrough_device"]
            map_backend = "uinput"
            device_type = "keyboard"
            remap_whitelist = false

            [device."passthrough_device".remap]
            "SLIDER_1" = "KEY_A"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("passthrough_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");
        assert!(!device_config.remap_whitelist);
        assert_eq!(
            device_config.remap.get("SLIDER_1"),
            Some(&KeyExpr::Single("KEY_A".to_string()))
        );
    }

    #[test]
    fn test_device_config_with_keyexpr_combo_and_sequence() {
        // Test both string and array TOML syntax for combos
        let toml_str = r#"
            [device."advanced_device"]
            map_backend = "uinput"
            device_type = "keyboard"

            [device."advanced_device".remap]
            "SLIDER_1" = "KEY_A+KEY_B"
            "SLIDER_2" = "KEY_C,KEY_D,KEY_E"
            "SLIDER_3" = ["KEY_X", "KEY_Y"]
            "GAME_1" = "KEY_SPACE"
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let device_config = config.device.get("advanced_device").unwrap();
        assert_eq!(device_config.map_backend, "uinput");
        assert_eq!(device_config.device_type, "keyboard");

        // Test combo expression (string syntax)
        assert_eq!(
            device_config.remap.get("SLIDER_1"),
            Some(&KeyExpr::Combo(vec![
                "KEY_A".to_string(),
                "KEY_B".to_string()
            ]))
        );

        // Test sequence expression (string syntax)
        assert_eq!(
            device_config.remap.get("SLIDER_2"),
            Some(&KeyExpr::Sequence(vec![
                "KEY_C".to_string(),
                "KEY_D".to_string(),
                "KEY_E".to_string()
            ]))
        );

        // Test combo expression (array syntax)
        assert_eq!(
            device_config.remap.get("SLIDER_3"),
            Some(&KeyExpr::Combo(vec![
                "KEY_X".to_string(),
                "KEY_Y".to_string()
            ]))
        );

        // Test single expression
        assert_eq!(
            device_config.remap.get("GAME_1"),
            Some(&KeyExpr::Single("KEY_SPACE".to_string()))
        );
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
