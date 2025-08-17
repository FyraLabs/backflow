const throttle = (func, wait) => {
  var ready = true;
  var args = null;
  return function throttled() {
    var context = this;
    if (ready) {
      ready = false;
      setTimeout(function () {
        ready = true;
        if (args) {
          throttled.apply(context);
        }
      }, wait);
      if (args) {
        func.apply(this, args);
        args = null;
      } else {
        func.apply(this, arguments);
      }
    } else {
      args = arguments;
    }
  };
};

class WebSocketHandler {
  constructor(url = `ws://${window.location.host}/ws`) {
    this.url = url;
    this.ws = null;
    this.isConnected = false;
    this.reconnectAttempts = 0;
    this.maxReconnectAttempts = 5;
    this.reconnectDelay = 1000;
    this.connect();
  }

  connect() {
    try {
      this.ws = new WebSocket(this.url);

      this.ws.onopen = () => {
        console.log("connected to backflow websocket");
        this.isConnected = true;
        this.reconnectAttempts = 0;
      };

      this.ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data);
          if (data.events && Array.isArray(data.events)) {
            this.handleFeedbackPacket(data);
          }
        } catch (e) {
          console.warn("Failed to parse WebSocket message:", e);
        }
      };

      this.ws.onclose = () => {
        console.log("websocket connection closed");
        this.isConnected = false;
        this.attemptReconnect();
      };

      this.ws.onerror = (error) => {
        console.error("websocket error:", error);
        this.isConnected = false;
      };
    } catch (error) {
      console.error("failed to create websocket connection:", error);
      this.attemptReconnect();
    }
  }

  attemptReconnect() {
    if (this.reconnectAttempts < this.maxReconnectAttempts) {
      this.reconnectAttempts++;
      console.log(
        `attempting to reconnect... (${this.reconnectAttempts}/${this.maxReconnectAttempts})`,
      );
      setTimeout(() => {
        this.connect();
      }, this.reconnectDelay * this.reconnectAttempts);
    } else {
      console.error("max reconnection attempts reached");
    }
  }

  sendPacket(packet) {
    if (!this.isConnected || !this.ws) {
      return;
    }
    try {
      this.ws.send(JSON.stringify(packet));
    } catch (error) {
      console.error("failed to send ws message:", error);
    }
  }

  handleFeedbackPacket(packet) {
    for (const event of packet.events) {
      if (event.Led && event.Led.Set) {
        this.handleLedEvent(event.Led.Set);
      }
    }
  }

  handleLedEvent(ledEvent) {
    const { led_id, on, brightness, rgb } = ledEvent;
    const cell = this.getCellByLedId(led_id);
    if (cell) {
      this.applyLedToCell(cell, on, brightness, rgb);
    }
  }

  getCellByLedId(ledId) {
    const elementWithLedId = document.querySelector(`[data-led-id="${ledId}"]`);
    console.log(`Getting cell for LED ID ${ledId}:`, elementWithLedId);

    // For separator LED IDs (odd numbers 1-29), return the adjacent keys for border glow
    if (ledId >= 1 && ledId <= 29 && ledId % 2 === 1) {
      const leftKeyId = ledId - 1;
      const rightKeyId = ledId + 1;
      const leftKey = document.querySelector(`[data-led-id="${leftKeyId}"]`);
      const rightKey = document.querySelector(`[data-led-id="${rightKeyId}"]`);

      // Return a special object that represents the separator between keys
      return {
        isSeparator: true,
        ledId: ledId,
        leftKey: leftKey,
        rightKey: rightKey
      };
    }

    return elementWithLedId || null;
  }

  applyLedToCell(cell, on, brightness, rgb) {
    // Check if this is a separator (border glow) request
    const isSeparator = cell && cell.isSeparator;

    console.log(
      `LED Apply - Cell:`,
      cell,
      `isSeparator: ${isSeparator}`,
      `on: ${on}`,
      `rgb:`,
      rgb,
    );

    if (!on) {
      if (isSeparator) {
        console.log("Turning OFF separator border line");
        // Remove border glow from adjacent keys
        if (cell.leftKey) {
          cell.leftKey.classList.remove("border-glow-right");
          cell.leftKey.style.removeProperty("--glow-color");
          this.updateCellShadows(cell.leftKey);
        }
        if (cell.rightKey) {
          cell.rightKey.classList.remove("border-glow-left");
          cell.rightKey.style.removeProperty("--glow-color");
          this.updateCellShadows(cell.rightKey);
        }
      } else if (cell && cell.classList) {
        cell.style.removeProperty("background-color");
        cell.style.removeProperty("box-shadow");
        cell.style.removeProperty("--key-glow-color");
        cell.classList.remove("rgb-active");
        cell.classList.remove("border-glow-left");
        cell.classList.remove("border-glow-right");
        cell.removeAttribute("data-rgb-color");
      }
      return;
    }

    let color = "white";
    let r = 255,
      g = 255,
      b = 255;

    if (rgb && Array.isArray(rgb) && rgb.length >= 3) {
      [r, g, b] = rgb;
      color = `rgb(${r}, ${g}, ${b})`;
    }

    if (brightness !== null && brightness !== undefined) {
      const alpha = brightness / 255;
      color = `rgba(${r}, ${g}, ${b}, ${alpha})`;
    }

    if (isSeparator) {
      console.log(`Applying separator border line with color: ${color}`);

      const glowColor = `rgba(${r}, ${g}, ${b}, 0.9)`;

      // Apply border glow to adjacent keys
      if (cell.leftKey) {
        cell.leftKey.style.setProperty("--glow-color", glowColor);
        cell.leftKey.classList.add("border-glow-right");
        this.updateCellShadows(cell.leftKey);
      }
      if (cell.rightKey) {
        cell.rightKey.style.setProperty("--glow-color", glowColor);
        cell.rightKey.classList.add("border-glow-left");
        this.updateCellShadows(cell.rightKey);
      }

      console.log(`Applied border line to separator ${cell.ledId} between keys`);
    } else if (cell && cell.classList) {
      cell.setAttribute("data-rgb-color", color);
      cell.style.backgroundColor = color;

      const glowColor = `rgba(${r}, ${g}, ${b}, 0.6)`;
      cell.style.setProperty("--key-glow-color", glowColor);

      // Build box-shadow combining key LED and border glow effects
      let shadows = [`inset 0 0 20px ${glowColor}`];

      // Add border lines if they exist
      const hasLeftGlow = cell.classList.contains("border-glow-left");
      const hasRightGlow = cell.classList.contains("border-glow-right");

      if (hasLeftGlow && hasRightGlow) {
        shadows.push(
          `inset 3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`,
          `inset -3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
        );
      } else if (hasLeftGlow) {
        shadows.push(
          `inset 3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
        );
      } else if (hasRightGlow) {
        shadows.push(
          `inset -3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
        );
      }

      cell.style.boxShadow = shadows.join(', ');
      cell.classList.add("rgb-active");
    }
  }

  updateCellShadows(cell) {
    if (!cell || !cell.classList) return;

    const isActive = cell.classList.contains("rgb-active");
    const hasLeftGlow = cell.classList.contains("border-glow-left");
    const hasRightGlow = cell.classList.contains("border-glow-right");

    if (!isActive && !hasLeftGlow && !hasRightGlow) {
      cell.style.removeProperty("box-shadow");
      return;
    }

    let shadows = [];

    // Add key LED shadow if active
    if (isActive) {
      const keyGlowColor = cell.style.getPropertyValue("--key-glow-color") || "rgba(255, 255, 255, 0.6)";
      shadows.push(`inset 0 0 20px ${keyGlowColor}`);
    }

    // Add border lines
    if (hasLeftGlow && hasRightGlow) {
      shadows.push(
        `inset 3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`,
        `inset -3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
      );
    } else if (hasLeftGlow) {
      shadows.push(
        `inset 3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
      );
    } else if (hasRightGlow) {
      shadows.push(
        `inset -3px 0 0 var(--glow-color, rgba(255, 255, 255, 0.9))`
      );
    }

    cell.style.boxShadow = shadows.join(', ');
  }

  getDeviceNameForCell(cell) {
    const gridContainer = cell.closest("[data-name]");
    if (gridContainer) {
      const containerName = gridContainer.getAttribute("data-name");
      return containerName;
    }

    const container = cell.closest("[data-cell-section]");
    if (container) {
      const section = container.getAttribute("data-cell-section");
      return `chunitroller-${section}`;
    }
    return "chunitroller-webapp";
  }

  sendKeyboardEvent(key, pressed, deviceId = "service-menu") {
    if (!this.isConnected || !this.ws) {
      console.warn("Cannot send keyboard event: WebSocket not connected");
      return;
    }

    const event = {
      Keyboard: {
        [pressed ? "KeyPress" : "KeyRelease"]: {
          key: key,
        },
      },
    };

    const packet = {
      device_id: deviceId,
      timestamp: Date.now(),
      events: [event],
    };

    this.sendPacket(packet);
  }

  close() {
    if (this.ws) {
      this.ws.close();
      this.isConnected = false;
    }
  }
}

class GridController {
  constructor() {
    this.webSocketHandler = new WebSocketHandler();

    this.cells = [];
    this.compiledCells = [];
    this.keyStates = [];
    this.lastKeyStates = [];
    this.pendingKeyStates = [];
    this.container = document.getElementById("grid-container");
    this.overlapThreshold = 0;
    this.touchCounter = null;
    this.activeTouches = new Map();

    this.init();
  }

  init() {
    this.cells = document.querySelectorAll(".grid-cell");
    this.touchCounter = document.getElementById("touchCounter");

    const overlapAttr = this.container.getAttribute("data-overlap");
    if (overlapAttr !== null) {
      const parsed = parseInt(overlapAttr, 10);
      if (!isNaN(parsed)) {
        this.overlapThreshold = parsed;
      }
    }

    this.cells.forEach((cell, index) => {
      cell.setAttribute("data-cell-index", index);
    });

    this.compileKeys();
    this.setupEventHandlers();
    this.updateTouchCounter();

    console.log(`Grid controller initialized with ${this.cells.length} cells`);
  }

  compileKeys() {
    this.compiledCells = [];
    this.keyStates = [];
    this.lastKeyStates = [];
    this.pendingKeyStates = [];

    for (let i = 0; i < this.cells.length; i++) {
      const cell = this.cells[i];
      const prev = cell.previousElementSibling;
      const next = cell.nextElementSibling;
      const rect = cell.getBoundingClientRect();
      // Find the closest container with data-slider
      const sectionContainer = cell.closest("[data-cell-section]");
      let sliderAttr = sectionContainer
        ? sectionContainer.getAttribute("data-slider")
        : null;
      let sliderEnabled = true;
      if (sliderAttr !== null) {
        sliderEnabled = sliderAttr === "true";
      }

      // Check for feedback setting - defaults to true
      let feedbackEnabled = true;
      const feedbackAttr = sectionContainer
        ? sectionContainer.getAttribute("data-feedback")
        : null;
      if (feedbackAttr !== null) {
        feedbackEnabled = feedbackAttr === "true";
      }

      const compiled = {
        index: i,
        top: rect.top,
        bottom: rect.bottom,
        left: rect.left,
        right: rect.right,
        almostLeft: prev ? rect.left + this.overlapThreshold : -99999,
        almostRight: next ? rect.right - this.overlapThreshold : 99999,
        prevIndex: prev ? parseInt(prev.getAttribute("data-cell-index")) : null,
        nextIndex: next ? parseInt(next.getAttribute("data-cell-index")) : null,
        ref: cell,
        section: sectionContainer?.getAttribute("data-cell-section"),
        isAirSensor:
          sectionContainer?.getAttribute("data-cell-section") === "air-sensor",
        sliderEnabled,
        feedbackEnabled,
      };

      this.compiledCells.push(compiled);
      this.keyStates.push(0);
      this.lastKeyStates.push(0);
      this.pendingKeyStates.push(0);
    }

    this.webSocketHandler.lastKeyStates = [...this.lastKeyStates];
  }

  isInside(x, y, compiledCell) {
    return (
      compiledCell.left <= x &&
      x < compiledCell.right &&
      compiledCell.top <= y &&
      y < compiledCell.bottom
    );
  }

  getCell(x, y) {
    for (let i = 0; i < this.compiledCells.length; i++) {
      if (this.isInside(x, y, this.compiledCells[i])) {
        return this.compiledCells[i];
      }
    }
    return null;
  }

  setKey(keyStates, index, value = 1) {
    if (index >= 0 && index < keyStates.length) {
      keyStates[index] = value;
    }
  }

  updateTouches(e) {
    try {
      e.preventDefault();

      const newKeyStates = new Array(this.keyStates.length).fill(0);
      const currentTouchIds = new Set();
      // Track previous cell per touchId
      if (!this.touchToCell) this.touchToCell = new Map();

      // First pass: handle cell changes and releases
      for (let i = 0; i < e.touches.length; i++) {
        const touch = e.touches[i];
        const touchId = touch.identifier;
        const x = touch.clientX;
        const y = touch.clientY;
        currentTouchIds.add(touchId);

        let targetCell = this.getCell(x, y);
        let prevCell = this.touchToCell.get(touchId) || null;

        // If this touch is already locked to a cell and slider is disabled, keep using the locked cell
        if (this.activeTouches.has(touchId)) {
          const lockedCell = this.activeTouches.get(touchId);
          if (!lockedCell.sliderEnabled) {
            targetCell = lockedCell;
          } else {
            // Update the locked cell if sliding is enabled
            if (targetCell) {
              this.activeTouches.set(touchId, targetCell);
            }
          }
        } else if (targetCell) {
          // New touch - lock it to the first cell it hits
          this.activeTouches.set(touchId, targetCell);
        }

        // Update tracking for this touch
        if (targetCell) {
          this.touchToCell.set(touchId, targetCell);
        } else if (prevCell) {
          // Touch moved outside all cells - remove tracking
          this.touchToCell.delete(touchId);
        }
      }

      // Clean up touches that are no longer active
      for (const touchId of this.activeTouches.keys()) {
        if (!currentTouchIds.has(touchId)) {
          this.touchToCell.delete(touchId);
          this.activeTouches.delete(touchId);
          console.log(`Touch ${touchId}: ended`);
        }
      }

      // Second pass: set active cells based on all remaining touches
      // Count how many touches are on each cell
      const cellTouchCounts = new Array(this.keyStates.length).fill(0);

      for (let i = 0; i < e.touches.length; i++) {
        const touch = e.touches[i];
        const touchId = touch.identifier;
        const targetCell = this.touchToCell.get(touchId);

        if (targetCell) {
          cellTouchCounts[targetCell.index]++;

          // Only allow sliding if sliderEnabled is true for this cell
          if (targetCell.sliderEnabled && !targetCell.isAirSensor) {
            const x = touch.clientX;
            if (x < targetCell.almostLeft && targetCell.prevIndex !== null) {
              cellTouchCounts[targetCell.prevIndex]++;
            }

            if (x > targetCell.almostRight && targetCell.nextIndex !== null) {
              cellTouchCounts[targetCell.nextIndex]++;
            }
          }
        }
      }

      // Set key states based on touch counts (any cell with > 0 touches is active)
      for (let i = 0; i < cellTouchCounts.length; i++) {
        newKeyStates[i] = cellTouchCounts[i] > 0 ? 1 : 0;
      }

      for (let i = 0; i < this.compiledCells.length; i++) {
        const cell = this.compiledCells[i];
        if (newKeyStates[i] !== this.lastKeyStates[i]) {
          // Only apply visual feedback if feedbackEnabled is true
          if (cell.feedbackEnabled) {
            if (newKeyStates[i]) {
              cell.ref.classList.add("active");
            } else {
              cell.ref.classList.remove("active");
            }
          }
        }
      }

      if (
        JSON.stringify(newKeyStates) !== JSON.stringify(this.pendingKeyStates)
      ) {
        this.throttledSendKeys(newKeyStates);
        this.pendingKeyStates = [...newKeyStates];
      }

      this.keyStates = newKeyStates;
      this.updateTouchCounter();
    } catch (err) {
      console.error("error in updateTouches:", err);
    }
  }

  sendKeys(keyStates) {
    const eventsByDevice = {};

    for (let i = 0; i < keyStates.length; i++) {
      if (keyStates[i] !== this.lastKeyStates[i]) {
        const cell = this.compiledCells[i].ref;
        if (cell) {
          const key = cell.getAttribute("data-key");
          const deviceId = this.webSocketHandler.getDeviceNameForCell(cell);
          const pressed = keyStates[i] === 1;

          const event = {
            Keyboard: {
              [pressed ? "KeyPress" : "KeyRelease"]: {
                key: key,
              },
            },
          };

          if (!eventsByDevice[deviceId]) {
            eventsByDevice[deviceId] = [];
          }
          eventsByDevice[deviceId].push(event);
        }
      }
    }

    for (const deviceId in eventsByDevice) {
      if (eventsByDevice[deviceId].length > 0) {
        const packet = {
          device_id: deviceId,
          timestamp: Date.now(),
          events: eventsByDevice[deviceId],
        };
        this.webSocketHandler.sendPacket(packet);
      }
    }

    this.lastKeyStates = [...keyStates];
  }

  setupEventHandlers() {
    const throttledUpdateTouches = throttle((e) => this.updateTouches(e), 10);
    this.throttledSendKeys = throttle(
      (keyStates) => this.sendKeys(keyStates),
      10,
    );

    const container = document.getElementById("main") || document.body;

    container.addEventListener("touchstart", throttledUpdateTouches, {
      passive: false,
    });
    container.addEventListener("touchmove", throttledUpdateTouches, {
      passive: false,
    });
    container.addEventListener("touchend", throttledUpdateTouches, {
      passive: false,
    });
    container.addEventListener("touchcancel", throttledUpdateTouches, {
      passive: false,
    });

    container.addEventListener("mousedown", (e) => {
      if (e.target.classList.contains("grid-cell")) {
        e.preventDefault();
        this.updateTouches({
          preventDefault: () => { },
          touches: [{ clientX: e.clientX, clientY: e.clientY }],
        });
      }
    });

    container.addEventListener("mouseup", (e) => {
      this.updateTouches({
        preventDefault: () => { },
        touches: [],
      });
    });

    container.addEventListener("mouseleave", (e) => {
      this.updateTouches({
        preventDefault: () => { },
        touches: [],
      });
    });

    window.addEventListener("resize", () => {
      this.compileKeys();
    });
  }

  updateTouchCounter() {
    if (this.touchCounter) {
      const activeCount = this.keyStates.filter((state) => state).length;
      this.touchCounter.textContent = `active keys: ${activeCount}`;
    }
  }

  resetAll() {
    this.keyStates.fill(0);
    this.lastKeyStates.fill(0);
    this.pendingKeyStates.fill(0);
    this.webSocketHandler.lastKeyStates = [];

    // Only remove active class from cells that have feedback enabled
    this.cells.forEach((cell, index) => {
      const compiledCell = this.compiledCells[index];
      if (compiledCell && compiledCell.feedbackEnabled) {
        cell.classList.remove("active");
      }
    });

    this.updateTouchCounter();
    console.log("all states reset");
  }

  destroy() {
    this.resetAll();
    this.webSocketHandler.close();
    console.log("GridController destroyed");
  }
}

window.addEventListener("DOMContentLoaded", () => {
  window.gridController = new GridController();

  window.resetGrid = () => {
    window.gridController.resetAll();
  };

  console.log("grid controller loaded");
});

document.addEventListener("contextmenu", (e) => {
  if (e.target.classList.contains("grid-cell")) {
    e.preventDefault();
  }
});
