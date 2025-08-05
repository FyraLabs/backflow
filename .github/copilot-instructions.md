# Backflow AI Coding Instructions

Backflow is a userspace input bridge daemon that converts unconventional input devices (arcade controllers, web gamepads, RS232 devices) into standard Linux input events via uinput. Think of it as a software equivalent to the Xbox Adaptive Controller - bringing accessibility and custom hardware integration to Linux gaming.

## Architecture Overview

### Core Components & Data Flow

1. **Input Backends** (`src/input/`) - Capture events from various sources (WebSocket, Unix sockets, Brokenithm iOS proxy)
2. **Device Filter** (`src/device_filter.rs`) - Transforms custom keycodes (e.g., `SLIDER_1`) to evdev codes (e.g., `KEY_A`) with per-device configuration
3. **Output Backends** (`src/output/`) - Route processed events to uinput, InputPlumber D-Bus, or ChuniIO proxy
4. **Feedback System** (`src/feedback/`) - Handles RGB LED data from games back to hardware

### Key Data Structures

- `InputEventPacket` - Container for batched events from a specific device with timestamp
- `DeviceFilter` - Handles per-device key remapping and routing using `DeviceConfig` from TOML
- `MessageStreams` - Tokio broadcast channels connecting input→filter→output pipelines

## Essential Development Patterns

### Configuration-Driven Device Routing

Every device can have custom mapping rules in `backflow.toml`:

```toml
[device."my_controller"]
map_backend = "uinput"           # Route to specific output backend
device_type = "keyboard"         # Device type emulation
remap_whitelist = false          # Filter unmapped keys if true

[device."my_controller".remap]
"SLIDER_1" = "KEY_A"             # Simple 1:1 mapping
"COMBO_KEY" = ["KEY_A", "KEY_B"] # Multi-key combo (not yet implemented)
```

### Service Management Pattern

All async services use the `ServiceManager` pattern in `backend.rs`:

- Services are spawned as independent tokio tasks
- Use `service_manager.spawn()` for lifecycle management
- Services communicate via broadcast channels (not direct references)
- Graceful shutdown via `shutdown()` and `wait_for_any_completion()`

### Input Event Processing

- **Atomic Batching** (`src/input/atomic.rs`) - Groups related events by device/timestamp to prevent race conditions
- **Cross-Device Events** - Web frontend uses `data-name` attribute to unify events from multiple UI sections
- **Event Routing** - Device filter splits events into `uinput_events` (standard keys) vs `chuniio_events` (CHUNIIO\_ prefixed)

## Critical Development Commands

### Build & Test

```bash
# Build for development
cargo build

# Run with debug logging
RUST_LOG=trace cargo run

# Run specific tests (device filtering is heavily tested)
cargo test device_filter
cargo test test_atomic_batching
```

### Configuration Testing

```bash
# Test with example configs
cargo run -- --config backflow.device-example.toml

# Web UI testing at http://localhost:8000/templates/chuni.html
# WebSocket endpoint: ws://localhost:8000/ws
```

## Project-Specific Conventions

### Error Handling

- Use `eyre::Result` for all fallible operations
- Structured logging via `tracing` with target filtering (zbus/tungstenite are silenced by default)
- Services should log errors but continue operating when possible

### Input Key Conventions

- Custom keys: `SLIDER_1`, `GAME_1`, `CHUNIIO_SLIDER_1` (device-specific prefixes)
- Standard keys: `KEY_A`, `BTN_LEFT` (Linux evdev codes)
- `DeviceFilter::is_standard_evdev_key()` distinguishes between them

### Web Frontend Integration

- Touch events are throttled (10ms) and batched by device ID
- Uses `data-key` attributes for key mapping and `data-cell-section` for device grouping
- WebSocket write-only mode available via `X-Backflow-Write-Only: true` header

### Testing Patterns

- Mock `InputEventPacket`s with `InputEventPacket::new(device_id, timestamp)`
- Device filter tests use `create_test_config()` helper
- Async tests use `tokio::test` and `timeout()` for channel operations

## Integration Points

### InputPlumber D-Bus

- Optional integration via `zbus-inputplumber` crate
- Creates virtual devices: keyboard, mouse, gamepad based on `device_type`
- Handles device lifecycle and capability advertisement

### ChuniIO Ecosystem

- Proxy server mode for rhythm game integration (`chuniio_proxy`)
- RGB feedback via Unix socket from Windows games (via Outflow bridge)
- LED data flows: segatools → Outflow → Unix socket → Backflow → hardware

### Web PWA Frontend

- Static files in `web/` directory served by Axum
- Templates for different controller layouts (`web/templates/`)
- Service worker for offline capability

## Common Pitfalls

- **Channel Lifecycle**: Ensure streams exist before spawning services that use them
- **Device Naming**: Inconsistent device IDs between web frontend and config will break routing
- **Event Atomicity**: Related touch events must use same device ID to prevent stuck keys
- **TOML Parsing**: Device section names must be quoted strings in config files

When implementing new backends or device types, follow the existing patterns in `input/web/` and `output/udev.rs` for consistent error handling and service lifecycle management.
