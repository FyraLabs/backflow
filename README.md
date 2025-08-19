# Backflow

> Accessible input for everyone, everywhere.
>
> Remap and route input from any device — adaptive controllers, arcade boards, or DIY hardware — with no kernel drivers, no lock-in.

**Backflow** is an input router and message broker that bridges unconventional input devices to standard HID events, powered by [InputPlumber](https://github.com/ShadowBlip/InputPlumber). Whether you're using analog controls, serial interfaces, web-based gamepads, MIDI devices, or other custom hardware, Backflow provides a modular way to route and remap any input into standard keyboard, mouse, or gamepad events.

Whether you're building a virtual gamepad in a browser, integrating RS232 or MIDI devices, or enabling hands-free control through custom hardware, Backflow helps unify everything into one virtual input stack.

This makes it ideal not just for gaming or custom hardware, but also for accessibility scenarios. Think of it as the [Xbox Adaptive Controller](https://www.xbox.com/en-US/accessories/controllers/xbox-adaptive-controller) — but entirely in software.

> No hands, no controller? No problem.
> Bring your own input, and play it your way.

## Relation to InputPlumber

Backflow is an input virtualization layer designed to complement tools like InputPlumber. It captures real-time input from unconventional, domain-specific, or remote sources — such as arcade IO boards, RS232 devices, MIDI controllers, browser-based PWAs, or assistive hardware — and exposes them to the Linux input stack as fully functional virtual devices.

Input events are routed through userspace and emitted via uinput, allowing them to be recognized by games, desktop environments, and remapping tools like InputPlumber — all without requiring vendor drivers or kernel modules.

This architecture cleanly separates responsibilities:

- Backflow captures arbitrary input sources and emits them as native virtual devices via uinput
- InputPlumber (optionally) handles remapping, transformation, and device composition within the HID domain

Together, they provide a flexible and modular input pipeline for gaming, accessibility tooling, or custom interactive systems.

> TL;DR: Backflow lets you turn anything into an input device — from arcade sliders to browser touchpads — no kernel drivers required.

## (Planned) Features

- **WebSocket server** for receiving input events
- **PWA virtual gamepad server** using the WebSocket routing backend
- **Per-device filtering and key remapping** with configurable transformations
  - Map custom keycodes to standard evdev codes
  - Device-specific routing to different output backends
  - Support for non-standard input devices with custom key definitions
- **Modular input backends** with support for:
  - Serial devices (e.g. RS232, UART, GPIO)
  - MIDI instruments and controllers
  - Analog input (e.g. joystick axes, potentiometers, rotary encoders, pedals)
- **RGB feedback output** to various devices and protocols:
  - [JVS](https://en.wikipedia.org/wiki/Japan_Amusement_Machine_and_Marketing_Association#Video)
  - Novation MIDI hardware
  - OpenRGB-compatible devices
- **Input remapping and transformation**, using InputPlumber's configuration and virtual device system

## Getting Started

> [!NOTE]
> Backflow is currently in early development and not yet ready for production use. Expect breaking changes and limited documentation.
> Backflow currently only supports the WebSocket remote routing backend, with no other input backends implemented yet.
> The web UI is also included for testing purposes, they are not yet fully functional and will be improved in future releases.

To get started with Backflow, build the project from source:

```bash
cargo build --release
```

Then run the daemon:

```bash
sudo ./target/release/backflow
```

This will start the Backflow daemon, listening for connections at `ws://localhost:8000/ws` by default.

The web UI can be accessed at `http://localhost:8000/`, which provides a basic [brokenithm-kb](https://github.com/4yn/brokenithm-kb)-like interface for testing the WebSocket router backend, as a demo for the UMIGURI-style layout. More default layouts will be added in the future.

Note that this layout only provides 16 slider zones, unlike a standard CHUNITHM controller which splits this vertically into 32 zones. The game however only uses 16 horizontal zones, so this is sufficient for testing purposes.

### WebSocket API

The WebSocket endpoint accepts JSON-formatted input events and optionally sends back feedback events. You can connect to `ws://localhost:8000/ws` to send input events.

#### Write-Only Mode

For applications that only need to send input events without receiving feedback (to save bandwidth), you can use write-only mode by adding the `X-Backflow-Write-Only: true` header when establishing the WebSocket connection:

```javascript
// JavaScript example
const ws = new WebSocket('ws://localhost:8000/ws', [], {
    headers: {
        'X-Backflow-Write-Only': 'true'
    }
});

// Python websockets example
import websockets

async def connect():
    extra_headers = {'X-Backflow-Write-Only': 'true'}
    async with websockets.connect('ws://localhost:8000/ws', extra_headers=extra_headers) as websocket:
        # Send input events without receiving feedback
        await websocket.send('{"device_id":"my_device","timestamp":1234567890,"events":[...]}')
```

#### Input Event Format

Send input events as JSON objects:

```json
{
    "device_id": "my_controller",
    "timestamp": 1234567890123,
    "events": [
        {
            "Keyboard": {
                "KeyPress": {
                    "key": "CHUNIIO_SLIDER_1"
                }
            }
        }
    ]
}
```

#### Feedback Events (if not in write-only mode)

Receive feedback events for LEDs, haptics, etc.:

```json
{
    "device_id": "my_controller", 
    "timestamp": 1234567890123,
    "events": [
        {
            "RgbLed": {
                "SetColor": {
                    "led": 0,
                    "color": [255, 0, 0]
                }
            }
        }
    ]
}
```

### Configuration

Backflow supports per-device filtering and key remapping through a TOML configuration file. Create a `backflow.toml` file in your working directory:

```toml
# Basic input/output configuration
[input.web]
enabled = true
port = 8000
host = "0.0.0.0"

[output.uinput]
enabled = true

# Per-device configuration with custom key remapping
[device."slider_controller"]
map_backend = "uinput"
device_type = "keyboard"

[device."slider_controller".remap]
"SLIDER_1" = "KEY_A"
"SLIDER_2" = "KEY_S" 
"SLIDER_3" = "KEY_D"
# ... more mappings

[device."custom_gamepad"]
map_backend = "uinput"
device_type = "keyboard"

[device."custom_gamepad".remap]
"GAME_1" = "KEY_SPACE"
"BUTTON_A" = "KEY_Z"
```

This allows you to:

- Map custom keycodes (like `SLIDER_1`, `GAME_1`) to standard evdev codes (`KEY_A`, `KEY_SPACE`)
- Route different devices to different output backends
- Configure device-specific transformations

See `backflow.device-example.toml` for a complete example configuration.

---

## Use Cases

- Virtual touch-based gamepads for PC gaming
- Accessibility solutions for users with limited mobility
- Repurposing old input devices (e.g. old joysticks, proprietary arcade hardware) for other applications
- Virtual gamepad emulation for proprietary control schemes

---

## The origin story

Backflow was actually not initially intended to be a universal input framework.
It started as a Linux-based implementation of [slidershim](https://github.com/4yn/slidershim), a project that allows users to remap their RS232-based CHUNITHM-style controllers to various input schemes.
However, as we started brainstorming the possibilities initially, it became clear that this could be expanded into a much more universal input framework, if marketed correctly and not scoped to just trying to hack $300 arcade pads or iPads to play VSRGs.

And as there seemed to simply be no such solution available already from:

- Steam Input (which only support well HID gamepads)
- Pinnacle Game Profiler (Windows only, proprietary, and dead project literally, the maintainer passed way RIP)
- slidershim (Windows only, scoped only to either RS232-based controllers, PWA virtual gamepads, and limited to just playing VSRGs or Project DIVA)
- Plain old InputPlumber (which is a great app, but only remaps HID events from one device to another, just like Steam Input)

So we decided to build Backflow, an input bridge that converts unconventional inputs into arbitrary D-Bus messages for InputPlumber to consume, remapping them to any HID events supported by InputPlumber.
