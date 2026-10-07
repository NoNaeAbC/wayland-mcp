import vm from "node:vm";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

// Permission categories are validated on every runtime start and restart.
if (Number(process.versions.node.split(".")[0]) < 26 || !process.permission) {
  throw new Error("gui_console requires Node 26 or newer with --permission");
}
for (const permission of ["fs.read", "fs.write", "net", "child", "worker", "addons", "ffi", "wasi", "inspector"]) {
  if (process.permission.has(permission)) throw new Error(`unexpected Node permission: ${permission}`);
}

const pendingNative = new Map();
let nextNativeId = 1;
const evalContext = new AsyncLocalStorage();
const inputContext = new AsyncLocalStorage();
const MAX_LOG_ENTRIES = 500;
const MAX_LOG_CHARS = 4096;
const MAX_RESULT_CHARS = 1_000_000;
const SYNC_EVAL_TIMEOUT_MS = 1000;

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function printable(value) {
  let rendered;
  if (typeof value === "string") rendered = value;
  else {
    try { rendered = JSON.stringify(value); } catch { rendered = String(value); }
  }
  if (rendered === undefined) rendered = String(value);
  if (rendered.length <= MAX_LOG_CHARS) return rendered;
  return `${rendered.slice(0, MAX_LOG_CHARS)}… [truncated]`;
}

function appendLog(args) {
  const logs = evalContext.getStore()?.logs;
  if (!logs) return;
  if (logs.length < MAX_LOG_ENTRIES) {
    logs.push(args.map(printable).join(" "));
  } else if (logs.length === MAX_LOG_ENTRIES) {
    logs.push(`[additional console output omitted after ${MAX_LOG_ENTRIES} entries]`);
  }
}

function boundedResult(value) {
  let encoded;
  try { encoded = JSON.stringify(value ?? null); }
  catch (error) { throw new Error(`evaluation result is not JSON-serializable: ${error}`); }
  if (encoded.length > MAX_RESULT_CHARS) {
    throw new Error(`evaluation result exceeds the ${MAX_RESULT_CHARS}-character limit`);
  }
  return JSON.parse(encoded);
}

function native(method, args = {}) {
  if (pendingNative.size >= 64) return Promise.reject(new Error("native request limit reached"));
  const id = nextNativeId++;
  const evalId = evalContext.getStore()?.id ?? null;
  const subscriptionId = inputContext.getStore()?.id ?? null;
  write({type:"native_call", id, evalId, subscriptionId, method, args});
  const promise = new Promise((resolve, reject) => pendingNative.set(id, {resolve, reject, evalId, subscriptionId}));
  // Detached calls still reject when their evaluation ends; avoid turning that
  // lifecycle cleanup into an unrelated unhandled-rejection process exit.
  promise.catch(() => {});
  return promise;
}

const pointerTarget = { windowId: null };

// Linux evdev key codes. Named keys make common automation readable while the
// raw numeric form remains available for unusual layouts and hardware keys.
const namedKeys = Object.freeze({
  ESC: 1, ESCAPE: 1,
  BACKSPACE: 14, TAB: 15, ENTER: 28, RETURN: 28,
  CTRL: 29, CONTROL: 29, LEFTCTRL: 29,
  SHIFT: 42, LEFTSHIFT: 42,
  RIGHTSHIFT: 54, ALT: 56, LEFTALT: 56, SPACE: 57, RIGHTCTRL: 97, RIGHTALT: 100,
  F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64,
  F7: 65, F8: 66, F9: 67, F10: 68, F11: 87, F12: 88,
  HOME: 102, UP: 103, PAGEUP: 104, LEFT: 105, RIGHT: 106,
  END: 107, DOWN: 108, PAGEDOWN: 109, INSERT: 110, DELETE: 111,
  ARROWUP: 103, ARROWLEFT: 105, ARROWRIGHT: 106, ARROWDOWN: 108,
  META: 125, SUPER: 125, LOGO: 125, LEFTMETA: 125,
});
const namedKeyNames = Object.freeze(Object.keys(namedKeys).sort());

function resolveKey(key) {
  if (Number.isInteger(key) && key >= 0 && key <= 0xffffffff) return key;
  if (typeof key === "string") {
    const resolved = namedKeys[key.replaceAll(/[-_ ]/g, "").toUpperCase()];
    if (resolved !== undefined) return resolved;
  }
  throw new TypeError(`key must be a non-negative 32-bit evdev code, one character, or one of: ${namedKeyNames.join(", ")}`);
}

function finiteNumber(value, name) {
  if (!Number.isFinite(value)) throw new TypeError(`${name} must be a finite number`);
  return value;
}

function fullPoint({
  x,
  y,
  coordinateSpace = "full",
  previewToFullScale,
  preview_to_full_scale: previewToFullScaleSnakeCase,
}) {
  x = finiteNumber(x, "x");
  y = finiteNumber(y, "y");
  if (coordinateSpace === "full") return { x: Math.round(x), y: Math.round(y) };
  if (coordinateSpace !== "preview") {
    throw new TypeError('coordinateSpace must be "full" or "preview"');
  }
  const scale = previewToFullScale ?? previewToFullScaleSnakeCase;
  const scaleX = finiteNumber(scale?.x, "previewToFullScale.x");
  const scaleY = finiteNumber(scale?.y, "previewToFullScale.y");
  if (scaleX <= 0 || scaleY <= 0) throw new RangeError("preview scale must be positive");
  return { x: Math.round(x * scaleX), y: Math.round(y * scaleY) };
}

async function enterPointerTarget(windowId, x, y, events) {
  if (pointerTarget.windowId !== windowId) {
    if (pointerTarget.windowId !== null) {
      const previousWindowId = pointerTarget.windowId;
      pointerTarget.windowId = null;
      try {
        events.push(await native("pointer_event", {
          windowId: previousWindowId,
          event: { type: "leave" },
        }));
        events.push(await native("pointer_event", {
          windowId: previousWindowId,
          event: { type: "frame" },
        }));
      } catch (error) {
        if (!/no mapped target window|disappeared|unknown windowId/.test(String(error))) throw error;
      }
    }
  }
  // The native scene selects a surface for these coordinates. Even within one
  // window that surface can change (for example, a popup or subsurface).
  // Reestablish the explicit target rather than trusting a cached window ID.
  events.push(await native("pointer_event", { windowId, event: { type: "enter", x, y } }));
  pointerTarget.windowId = windowId;
}

async function move({ windowId, ...coordinates }) {
  const { x, y } = fullPoint(coordinates);
  const events = [];
  await enterPointerTarget(windowId, x, y, events);
  events.push(await native("pointer_event", { windowId, event: { type: "motion", x, y } }));
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  return { delivered: true, point: { x, y }, events };
}

async function click({ windowId, button = 0x110, ...coordinates }) {
  const moved = await move({ windowId, ...coordinates });
  const events = [...moved.events];
  let buttonDown = false;
  try {
    buttonDown = true;
    events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 1 } }));
    events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
    events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 0 } }));
    buttonDown = false;
    events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
    return { delivered: true, point: moved.point, button, events };
  } finally {
    if (buttonDown) {
      try {
        await native("pointer_event", { windowId, event: { type: "button", button, state: 0 } });
      } finally {
        await native("pointer_event", { windowId, event: { type: "frame" } });
      }
    }
  }
}

async function doubleClick(args) {
  const intervalMs = args.intervalMs ?? 100;
  finiteNumber(intervalMs, "intervalMs");
  if (intervalMs < 0 || intervalMs > 2000) {
    throw new RangeError("intervalMs must be from 0 through 2000");
  }
  const first = await click(args);
  await new Promise((resolve) => setTimeout(resolve, intervalMs));
  const second = await click(args);
  return { delivered: true, clicks: [first, second] };
}

async function drag({ windowId, from, to, button = 0x110, durationMs = 400, steps = 12 }) {
  if (!Number.isInteger(steps) || steps < 1 || steps > 1000) {
    throw new RangeError("steps must be an integer from 1 through 1000");
  }
  finiteNumber(durationMs, "durationMs");
  if (durationMs < 0 || durationMs > 60_000) {
    throw new RangeError("durationMs must be from 0 through 60000");
  }
  const start = fullPoint(from);
  const end = fullPoint(to);
  const events = [];
  await enterPointerTarget(windowId, start.x, start.y, events);
  events.push(await native("pointer_event", { windowId, event: { type: "motion", ...start } }));
  let buttonDown = false;
  try {
    buttonDown = true;
    events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 1 } }));
    events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
    for (let step = 1; step <= steps; step += 1) {
      if (durationMs > 0) await new Promise((resolve) => setTimeout(resolve, durationMs / steps));
      const point = {
        x: Math.round(start.x + ((end.x - start.x) * step) / steps),
        y: Math.round(start.y + ((end.y - start.y) * step) / steps),
      };
      events.push(await native("pointer_event", { windowId, event: { type: "motion", ...point } }));
      events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
    }
    events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 0 } }));
    buttonDown = false;
    events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
    return { delivered: true, from: start, to: end, button, durationMs, steps, events };
  } finally {
    if (buttonDown) {
      try {
        await native("pointer_event", { windowId, event: { type: "button", button, state: 0 } });
      } finally {
        await native("pointer_event", { windowId, event: { type: "frame" } });
      }
    }
  }
}

async function scroll({ windowId, deltaY, ...coordinates }) {
  const { x, y } = fullPoint(coordinates);
  deltaY = finiteNumber(deltaY, "deltaY");
  const value = Math.round(deltaY * 256);
  if (value < -2147483648 || value > 2147483647) {
    throw new RangeError("deltaY does not fit a signed wl_fixed value");
  }
  const events = [];
  await enterPointerTarget(windowId, x, y, events);
  events.push(await native("pointer_event", { windowId, event: { type: "motion", x, y } }));
  events.push(await native("pointer_event", { windowId, event: { type: "axis_source", axis_source: 0 } }));
  events.push(await native("pointer_event", { windowId, event: { type: "axis", axis: 0, value } }));
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  return { delivered: true, point: { x, y }, deltaY, rawFixedValue: value, events };
}

async function enterKeyboardTarget(windowId, events) {
  // A wl_keyboard.key has no surface argument; establish the requested target
  // for every operation, independently of desktop input and previous calls.
  events.push(await native("keyboard_event", { windowId, event: { type: "enter", keys: [] } }));
}

async function pressKey({ windowId, key, holdMs = 40 }) {
  const isCharacter = typeof key === "string" && [...key].length === 1;
  const character = isCharacter ? key.toLocaleLowerCase("en-US") : null;
  const plan = character === null ? null : await native("keyboard_text_plan", { windowId, text: character });
  const stroke = plan?.strokes[0];
  if (character !== null && stroke === undefined) {
    throw new Error(`character key ${JSON.stringify(key)} is absent from the target keymap`);
  }
  const keyCode = stroke?.key ?? resolveKey(key);
  finiteNumber(holdMs, "holdMs");
  if (holdMs < 0 || holdMs > 60_000) throw new RangeError("holdMs must be from 0 through 60000");
  const events = [];
  await enterKeyboardTarget(windowId, events);
  let keyDown = false;
  let modifiersChanged = false;
  try {
    if (plan !== null) {
      modifiersChanged = true;
      events.push(await native("keyboard_event", { windowId, event: {
        type: "modifiers", mods_depressed: stroke.modifiers, mods_latched: 0, mods_locked: 0,
        group: plan.layout_group,
      } }));
    }
    keyDown = true;
    events.push(await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 1 } }));
    if (holdMs > 0) await new Promise((resolve) => setTimeout(resolve, holdMs));
    events.push(await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 0 } }));
    keyDown = false;
  } finally {
    try {
      if (keyDown) {
        await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 0 } });
      }
    } finally {
      if (modifiersChanged) {
        events.push(await native("keyboard_event", { windowId, event: {
          type: "modifiers",
          mods_depressed: plan.restore_mods_depressed,
          mods_latched: plan.restore_mods_latched,
          mods_locked: plan.restore_mods_locked,
          group: plan.layout_group,
        } }));
      }
    }
  }
  return {
    delivered: true, key, keyCode, holdMs, keymapDriven: plan !== null,
    modifierMask: stroke?.modifiers ?? null, events,
  };
}

async function pressShortcut({ windowId, keys, holdMs = 40 }) {
  if (!Array.isArray(keys) || keys.length < 2 || keys.length > 8) {
    throw new TypeError('pressShortcut requires keys with 2..8 entries, for example {windowId,keys:["CTRL","+"]}; named modifiers first, primary key last');
  }
  finiteNumber(holdMs, "holdMs");
  if (holdMs < 0 || holdMs > 60_000) throw new RangeError("holdMs must be from 0 through 60000");
  const primary = keys.at(-1);
  const primaryText = typeof primary === "string" && [...primary].length === 1
    ? primary.toLocaleLowerCase("en-US") : "";
  const plan = await native("keyboard_text_plan", { windowId, text: primaryText });
  const primaryStroke = primaryText === "" ? null : plan.strokes[0];
  if (primaryText !== "" && primaryStroke === undefined) {
    throw new Error(`shortcut key ${JSON.stringify(primary)} is absent from the target keymap`);
  }
  const keyCode = primaryStroke?.key ?? resolveKey(primary);
  let depressed = primaryStroke?.modifiers ?? 0;
  for (const modifier of keys.slice(0, -1)) {
    if (typeof modifier !== "string") {
      throw new TypeError("shortcut modifiers must be named CTRL, SHIFT, ALT, or META");
    }
    const name = modifier.replaceAll(/[-_ ]/g, "").toUpperCase();
    const mask = name === "CTRL" || name === "CONTROL" ? plan.control_modifier
      : name === "SHIFT" ? plan.shift_modifier
      : name === "ALT" ? plan.alt_modifier
      : name === "META" || name === "SUPER" || name === "LOGO" ? plan.logo_modifier
      : undefined;
    if (mask === undefined || mask === null) {
      throw new Error(`shortcut modifier ${JSON.stringify(modifier)} is absent from the target keymap`);
    }
    depressed |= mask;
  }
  const events = [];
  await enterKeyboardTarget(windowId, events);
  let keyDown = false;
  let modifiersChanged = false;
  try {
    modifiersChanged = true;
    events.push(await native("keyboard_event", { windowId, event: {
      type: "modifiers", mods_depressed: depressed, mods_latched: 0, mods_locked: 0,
      group: plan.layout_group,
    } }));
    keyDown = true;
    events.push(await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 1 } }));
    if (holdMs > 0) await new Promise((resolve) => setTimeout(resolve, holdMs));
    events.push(await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 0 } }));
    keyDown = false;
  } finally {
    try {
      if (keyDown) {
        await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 0 } });
      }
    } finally {
      if (modifiersChanged) {
        events.push(await native("keyboard_event", { windowId, event: {
          type: "modifiers",
          mods_depressed: plan.restore_mods_depressed,
          mods_latched: plan.restore_mods_latched,
          mods_locked: plan.restore_mods_locked,
          group: plan.layout_group,
        } }));
      }
    }
  }
  return { delivered: true, keys, keyCode, modifierMask: depressed, holdMs, events };
}

async function typeText({ windowId, text, intervalMs = 0, inputMethod = "keymap" }) {
  if (typeof text !== "string") throw new TypeError("text must be a string");
  if ([...text].length > 512) throw new RangeError("text is limited to 512 characters per call");
  finiteNumber(intervalMs, "intervalMs");
  if (intervalMs < 0 || intervalMs > 60_000) throw new RangeError("intervalMs must be from 0 through 60000");
  if (!["keymap", "unicode-hex"].includes(inputMethod)) {
    throw new RangeError('inputMethod must be "keymap" or "unicode-hex"');
  }
  if (inputMethod === "unicode-hex") {
    // Opt-in: this is an application input convention, not a Wayland feature.
    // Preflight the complete alphabet before changing the application's text.
    await native("keyboard_text_plan", { windowId, text: "u0123456789abcdef" });
    for (const character of text) {
      await pressShortcut({ windowId, keys: ["CTRL", "SHIFT", "u"] });
      await typeText({ windowId, text: character.codePointAt(0).toString(16) });
      await pressKey({ windowId, key: "ENTER" });
      if (intervalMs > 0) await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }
    return { delivered: true, text, inputMethod };
  }
  const plan = await native("keyboard_text_plan", { windowId, text });
  const events = [];
  await enterKeyboardTarget(windowId, events);
  let depressed = null;
  let keyDown = null;
  try {
    for (const stroke of plan.strokes) {
      if (stroke.modifiers !== depressed) {
        depressed = stroke.modifiers;
        events.push(await native("keyboard_event", { windowId, event: {
          type: "modifiers", mods_depressed: depressed, mods_latched: 0, mods_locked: 0,
          group: plan.layout_group,
        } }));
      }
      keyDown = stroke.key;
      events.push(await native("keyboard_event", { windowId, event: { type: "key", key: stroke.key, state: 1 } }));
      events.push(await native("keyboard_event", { windowId, event: { type: "key", key: stroke.key, state: 0 } }));
      keyDown = null;
      if (intervalMs > 0) await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }
  } finally {
    try {
      if (keyDown !== null) {
        await native("keyboard_event", { windowId, event: { type: "key", key: keyDown, state: 0 } });
      }
    } finally {
      if (depressed !== null) {
        events.push(await native("keyboard_event", { windowId, event: {
          type: "modifiers",
          mods_depressed: plan.restore_mods_depressed,
          mods_latched: plan.restore_mods_latched,
          mods_locked: plan.restore_mods_locked,
          group: plan.layout_group,
        } }));
      }
    }
  }
  return {
    delivered: true, text, keymapDriven: true, layoutGroup: plan.layout_group,
    intervalMs, strokes: plan.strokes, events,
  };
}

async function waitForWindow({ windowId, title, appId, timeoutMs = 5000, pollMs = 50 } = {}) {
  finiteNumber(timeoutMs, "timeoutMs");
  finiteNumber(pollMs, "pollMs");
  if (timeoutMs < 1 || timeoutMs > 900_000) {
    throw new RangeError("timeoutMs must be from 1 through 900000");
  }
  if (pollMs < 10 || pollMs > 10_000) {
    throw new RangeError("pollMs must be from 10 through 10000");
  }
  const deadline = Date.now() + timeoutMs;
  let matches = [];
  do {
    const windows = await native("windows");
    matches = windows.filter((window) => window.mapped !== false)
      .filter((window) => windowId === undefined || window.window_id === windowId)
      .filter((window) => title === undefined || window.title === title)
      .filter((window) => appId === undefined || window.app_id === appId);
    if (matches.length === 1) return matches[0];
    if (matches.length > 1) {
      throw new Error(`window selector is ambiguous; matched ${matches.map((window) => window.window_id).join(", ")}`);
    }
    if (Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, pollMs));
  } while (Date.now() < deadline);
  throw new Error(`no mapped window matched before the ${timeoutMs} ms timeout`);
}

async function waitForWindowGone({ windowId, title, appId, timeoutMs = 5000, pollMs = 50 } = {}) {
  finiteNumber(timeoutMs, "timeoutMs");
  finiteNumber(pollMs, "pollMs");
  if (timeoutMs < 1 || timeoutMs > 900_000) throw new RangeError("timeoutMs must be from 1 through 900000");
  if (pollMs < 10 || pollMs > 10_000) throw new RangeError("pollMs must be from 10 through 10000");
  const deadline = Date.now() + timeoutMs;
  do {
    const windows = await native("windows");
    const matches = windows.filter((window) => window.mapped !== false)
      .filter((window) => windowId === undefined || window.window_id === windowId)
      .filter((window) => title === undefined || window.title === title)
      .filter((window) => appId === undefined || window.app_id === appId);
    if (matches.length === 0) return { gone: true, selector: { windowId, title, appId } };
    if (Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, pollMs));
  } while (Date.now() < deadline);
  throw new Error(`a mapped window still matched after the ${timeoutMs} ms timeout`);
}

async function waitForCommit({ windowId, afterCommitSerial, timeoutMs = 5000 }) {
  validateFrameTimeout(timeoutMs);
  const before = (await native("windows")).find(w => w.window_id === windowId && w.mapped !== false);
  if (!before) return { surfaceDisappeared: true };
  const baseline = afterCommitSerial ?? before.commit_serial;
  const lease = await native("begin_observation", { windowId, durationMs: timeoutMs });
  const deadline = Date.now() + timeoutMs;
  try {
    do {
      const window = (await native("windows")).find(w => w.window_id === windowId && w.mapped !== false);
      if (!window) return { surfaceDisappeared: true };
      if (window.commit_serial > baseline) return { frameObserved: true, commitSerial: window.commit_serial };
      await new Promise(resolve => setTimeout(resolve, 10));
    } while (Date.now() < deadline);
    throw new Error("waitForCommit timed out");
  } finally {
    await native("end_observation", { id: lease.id });
  }
}

function validateFrameTimeout(timeoutMs) {
  if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 10000) {
    throw new RangeError("timeoutMs must be an integer from 1 through 10000");
  }
}

async function actAndCapture({ windowId, action, afterCommitSerial, timeoutMs = 5000 }) {
  if (typeof action !== "function") throw new TypeError("action must be a function returning an input-operation promise");
  validateFrameTimeout(timeoutMs);
  const beforeWindows = await native("windows");
  const before = beforeWindows.find((window) => window.window_id === windowId && window.mapped !== false);
  if (!before) throw new Error(`window ${JSON.stringify(windowId)} is not mapped`);
  const baselineCommitSerial = afterCommitSerial ?? before.commit_serial;
  const startedAt = Date.now();
  // Start capture before input: a quick producer commit can be released before
  // captureNextFrame is called, and released buffers cannot safely be read.
  const lease = await native("begin_observation", { windowId, durationMs: 120000 });
  try {
    const actionResult = await action();
    let capture = null;
    let captureError = null;
    try {
      capture = await native("capture_next_frame", { windowId, afterCommitSerial: baselineCommitSerial, timeoutMs });
    } catch (error) {
      captureError = error instanceof Error ? error.message : String(error);
    }
    const afterWindows = await native("windows");
    const after = afterWindows.find((window) => window.window_id === windowId && window.mapped !== false);
    return {
      actionResult,
      baselineCommitSerial,
      resultingCommitSerial: after?.commit_serial ?? null,
      elapsedMs: Date.now() - startedAt,
      frameObserved: capture !== null,
      surfaceDisappeared: after === undefined,
      capture,
      captureError,
    };
  } finally {
    await native("end_observation", { id: lease.id });
  }
}

function resetInputState() {
  pointerTarget.windowId = null;
  return { pointerTargetReset: true };
}

const apiHelp = `Persistent JavaScript console. State survives calls.
Input helpers explicitly target windowId on every operation, independently of desktop focus.
Call environment() for the native Wayland launch environment and socket/access requirements;
Pass its WAYLAND_DISPLAY and XDG_RUNTIME_DIR to the application, unset DISPLAY, and
use its native Wayland mode (Chromium: --ozone-platform=wayland). Launch as a live
foreground process session if the caller reaps detached jobs. socket_path and
launch_preflight are diagnostics, not environment variables to pass to applications.
Use the caller's permission mechanism if socket/GPU access is blocked. Then use
waitForWindow({appId,title,windowId,timeoutMs:5000,pollMs:50}); selectors are exact matches.
windows() returns window_id, mapped, dimensions, commit_serial and capture diagnostics;
subsurface_count is the exact number of live wl_subsurface descendants (nested included);
subsurfaces lists each role/surface/parent ID, position, effective synchronization,
committed buffer type/dimensions and capture diagnostics, including unbuffered children.
buffer_kind/capture_details at window level describe render_surface_id only; they do
not classify the entire window. Protocol IDs are local to each client connection.
it has no desktop focus flag. diagnostics() reports proxy/client errors.
Explicit screenshot({windowId}) and captureNextFrame({windowId}) return memory-only imageHandle,
width/height/color metadata and attach the image to the foreground MCP response.
No image files are written. presentImage(imageHandle) presents a retained image;
disposeImage(imageHandle) releases it early. The cache holds up to 16 images/32 MiB;
new captures automatically evict the oldest handles and report evictedImageHandles.
Evicted handles expire; manual disposal is optional. JS never receives PNG bytes or paths.
One evaluation can attach at most 16 images/32 MiB; disposal does not remove images
already attached to that evaluation. Split larger capture batches across console calls.
screenshot({windowId,coordinateSpace:"buffer"}) presents the raw render buffer so
sample coordinates match GLSL, without viewport/window/subsurface composition.
screenshot({windowId,surfaceId:30,coordinateSpace:"buffer"}) explicitly captures a
live surface in that window's subsurface tree. surfaceId is a positive protocol ID
from windows(). It requires DMA-BUF and waits up to 1 second for a fresh producer
commit under an observation lease. It excludes the parent and other subsurfaces.
Window-coordinate composition is not accepted together with surfaceId.
screenshot waits up to 250 ms for an update, then returns the readable current frame.
captureNextFrame({windowId,afterCommitSerial,timeoutMs:5000}) requires a newer commit;
timeoutMs is 1..10000. Omitting afterCommitSerial uses the current commit as baseline.
Prefer actAndCapture({windowId,action:()=>wayland.click({windowId,x,y}),timeoutMs:5000});
it starts observation BEFORE action, captures relative to the prior commit, and returns
actionResult, frameObserved, surfaceDisappeared, capture, captureError and commit serials.
Delivered input or a newer commit does not prove the application accepted the action.
beginObservation({windowId,durationMs:5000}) returns {id,durationMs}; duration is 1..120000.
endObservation({id}) ends that lease. Leases expire automatically (at most 32 active).
Capture/input leases and active visual subscriptions drive committed frame callbacks
without raising host windows. Input keeps its target rendering/capturable for 500 ms.
An already-released producer buffer is never reread; a missing/stale owned GPU copy
is a capture error. Enable observation before an action to retain its fresh pixels.
Continuous onVisual/onVisualProgram analysis keeps frames/history/scratch/state on GPU;
only declared bounded result records reach JS. No shader-file/SPIR-V uploads.
GLSL source strings compile in memory; fixed trusted kernels are embedded at build time.
Observe mapped proxied windows with environment(), diagnostics(), windows(), waitForWindow().
visualInfo({windowId}) reports buffer dimensions, scale/viewport, format and DRM affinity;
these are protocol metadata, without reading pixels or certifying a successful GPU import.
Programmable GPU observer: await wayland.onVisualProgram({windowId,
 passes:[{source:glslString,dispatch:[groupsX,groupsY,groupsZ]}],resultBytes:32,
 stateBytes:32,scratchBytes:4096,parameters:new Uint8Array(64),previousFrame:true,
 feedback:"previousResult",sourceColor:{transfer:"srgb",primaries:"bt709",alpha:"opaque"},
 maxFps:60,durationMs:120000},async(resultArrayBuffer,metadata)=>{}).
GLSL compiles in memory with statically linked Shaderc. No shader files/includes/plugins.
Set0 bindings: 0 current readonly rgba16f image2D linear BT.2020; 1 optional previous;
2 readonly previous state; 3 next state; 4 scratch; 5 result; 6 readonly parameters.
Buffers use std430. Push constants: uint width,height,sequence,historyValid.
Result <=256 bytes; state <=64KiB; scratch <=1MiB; parameters <=4096 bytes;
result/state/scratch lengths are >=4 and multiples of 4; parameters also align to 4.
Program durationMs is 1..600000; default maxFps:60, durationMs:120000,
stateBytes:32, scratchBytes:4096, feedback:"separateState", previousFrame:false.
Tracked Wayland color metadata controls normalization. sourceColor supplies an explicit
fallback for untagged sources: transfer:"srgb"/"linear"/"pq"/"hlg"/"bt1886"/"gamma22"/"gamma28"
or Wayland transfer ID 1..14; primaries:"bt709"/"bt2020"/"display-p3"/"adobe-rgb"
or Wayland primaries ID 1..10; optional luminances:[minimum,maximum,referenceWhite] in cd/m2.
alpha:"opaque"/"straight"/"premultiplied" (default premultiplied). RGBX always has alpha 1.
Untagged sources require sourceColor; tagged sources never use its fallback transfer/primaries.
Both observers decode into linear BT.2020; programmable frame storage is RGBA16F.
No tone mapping, 8-bit quantization or SDR clamping occurs in observation.
Results report sourceColor (assumed/description/alphaMode), epoch and historyValid.
Color interpretation changes reset GPU state/history before the next result.
For linear BT.709 RGB use rows (1.660491,-0.587641,-0.072850),
(-0.124550,1.132900,-0.008349),(-0.018151,-0.100579,1.118730) times BT.2020 RGB.
1..8 passes, each source <=256KiB. Use uint atomics for boolean flags.
Scratch/result zero each frame; state carried forward or previousResult copied GPU-only.
Geometry/format/device changes terminate; resubscribe resets history and state.
Callbacks can conditionally emit input between console calls. handle.metrics() reports cost/skips.
Subscribe: await wayland.onVisual({windowId,rules:[{id:"signal",rect:[x,y,width,height],
kind:"luminance",threshold:0.5,minPixels:8,polarity:"below",debounceFrames:1,cooldownFrames:1}],
maxFps:60,durationMs:120000}, async event => { /* programmed reaction */ });
kind:"luminance" uses Y=0.2627002*R+0.6779981*G+0.0593017*B on linear BT.2020 RGB. "below" includes Y<=threshold;
"above" includes Y>=threshold. active means at least minPixels matched in the rectangle.
It emits initial/changed boolean occupancy. kind:"change" compares maximum linear BT.2020 RGB
difference with GPU history (>=threshold) and
emits true pulses after a baseline. threshold is finite and nonnegative (positive for change; HDR values may exceed 1). minPixels is
an agent-provided threshold; counts and pixel values never return to the CPU.
GPU debounce/cooldown are measured in processed frames. maxFps 1..120; durationMs
1..3600000. Up to16 rules per window,8 windows,one subscription per window.
Rect coordinates are raw committed-buffer pixels; width/height>=4,area>=64,total<=1048576.
Supports all advertised 8-bit/10-bit/FP16 RGB DMA-BUF formats, known DRM feedback affinity, no transform or viewport crop.
sourceColor has the same fallback/alpha semantics as onVisualProgram.
Buffer scaling and destination-only viewport scaling are supported in raw buffer coordinates.
Unsupported geometry/buffers terminate with an error; no CPU/software fallback.
Events: {windowId,ruleId,active,frame,commitSerial,timestampMs,deliveryTimestampMs,sequence}.
Only events are transferred to CPU. Rule order is stable within each observed commit.
The callback runs between evaluations and may perform asynchronous input operations.
Handle: {id,initialState,status(),unsubscribe({drain:true})}; use drain:false inside callback.
callbackTimeoutMs defaults to 5000 (1..120000); status reports active/queued/error/end.
Visual handles also have metrics(): CPU operational counters for commits/admission/timing/lease;
these counters report no pixel-derived values and do not read any GPU pixel state.
Bounded queues fail with a gap on overflow; callback errors terminate the subscription.
Expiry, buffer geometry/format/device change, window destruction, and runtime exit stop it.
Input helpers: wayland.click,doubleClick,move,drag,scroll,pressKey,pressShortcut,typeText,
resizeWindow,resetInputState; raw pointerEvent,keyboardEvent,touchEvent,inputCapabilities.
click({windowId,x,y,button:272}); button is a Linux button code (272 left,273 right,274 middle).
doubleClick accepts the same fields plus intervalMs:100 (0..2000).
move({windowId,x,y}); drag({windowId,from:{x,y},to:{x,y},button:272,durationMs:400,steps:12});
durationMs is 0..60000, steps 1..1000. Points use rounded full screenshot pixels.
For preview points add coordinateSpace:"preview",previewToFullScale:{x,y} from the
image's preview_to_full_scale metadata; use each point's options for drag from/to.
scroll({windowId,x,y,deltaY}) sends a vertical wheel axis; positive down, negative up.
deltaY is a Wayland axis distance, not a guaranteed content-pixel displacement.
Applications decide scroll speed. It is converted to signed 24.8 as round(deltaY*256).
pressKey({windowId,key:"Space",holdMs:20}) accepts named keys, evdev codes, or characters.
pressShortcut({windowId,keys:["CTRL","+"],holdMs:40}); keys has 2..8 entries, named
modifiers CTRL/SHIFT/ALT/META first, primary key last. Character keys use the target
keymap; required character modifiers (for example Shift for +) are added automatically.
typeText({windowId,text,intervalMs:0,inputMethod:"keymap"}); max 512 characters,
intervalMs/holdMs 0..60000. "unicode-hex" is an opt-in application convention.
resizeWindow({windowId,width,height}) requests 1..8192 logical surface units; wait for
a subsequent commit and reread dimensions/preview scale before using coordinates.
resetInputState() clears helper pointer routing state, not delivered pressed keys.
wayland.keyNames lists supported names; keymap-aware character helpers honor the active layout.
keyboardEvent({windowId,event:{type:"key",key:57,state:1}}) presses Space; state:0 releases.
Raw pointerEvent({windowId,event}) types/fields:
enter{x,y,serial?}, leave{serial?}, motion{x,y,time?}, button{button,state,serial?,time?},
axis{axis,value,time?}, axis_source{axis_source}, axis_stop{axis,time?},
axis_discrete{axis,discrete}, axis_value120{axis,value120}, axis_relative_direction{axis,direction},
relative_motion{utime_hi,utime_lo,dx,dy,dx_unaccel,dy_unaccel}, frame{}.
Raw keyboardEvent types/fields: enter{keys:[],serial?}, leave{serial?},
key{key,state,serial?,time?}, modifiers{mods_depressed,mods_latched,mods_locked,group,serial?},
repeat_info{rate,delay}. Raw key/button sequences must enter their explicit target first;
raw pointer batches end with frame. Helpers establish targets and release pressed keys/buttons.
Omitted raw time/serial are generated by the compositor; time is monotonic milliseconds
modulo 2^32. Keys use evdev codes; state 1 presses, 0 releases. Pointer axis 0 vertical,1 horizontal;
axis value/relative deltas are signed 24.8, axis_source 0 wheel,1 finger,2 continuous,3 wheel tilt.
touchEvent({windowId,event}) types: down{id,x,y}, motion{id,x,y}, up{id}, frame{}, cancel{},
shape{id,major,minor}, orientation{id,orientation}; coordinates/shape/orientation are signed 24.8.
inputCapabilities({windowId}) reports supported devices/protocol versions and coordinate spaces.
Raw pointer/keyboard accept surfaceId from an input event; pointer replay additionally
requires coordinateSpace:"surface-fixed". Stale/destroyed surface tokens are rejected.
waitForCommit({windowId,afterCommitSerial,timeoutMs:5000}) holds observation and returns
metadata only; timeout 1..10000. waitForWindowGone accepts the waitForWindow selectors.
onInput({windowId,origin:"human",devices:["pointer"]}, callback) observes delivered input.
origin may be "human", "model" or "all"; devices may contain pointer, keyboard, touch,
relative_pointer or pointer_constraints. Defaults: human and [pointer,keyboard].
Input callbacks receive {windowId,surfaceId,device,origin,event,sequence,timestampMs,
deliveryTimestampMs}; retain surfaceId and raw event values for surface-fixed replay.
Input coordinates are full window pixels; raw coordinateSpace:"surface-fixed" uses 24.8.
sleep(ms) waits without blocking the console's event callbacks; timers may run between calls.
Use ordinary JavaScript and callback timers to author your own automation.`;

const inputSubscriptions = new Map();
const earlyInput = new Map();
function acceptInput(message) {
  const state = inputSubscriptions.get(message.subscriptionId);
  if (!state) {
    let queue = earlyInput.get(message.subscriptionId);
    if (!queue) { if (earlyInput.size >= 32) return; queue = []; earlyInput.set(message.subscriptionId, queue); }
    if (queue.length < 257) queue.push(message);
    return;
  }
  if (message.type === "input_end") {
    state.end = message.end;
    if(message.end?.reason !== "unsubscribed") {
      state.cancelled = true;
      inputSubscriptions.delete(state.id);
    }
    state.resolveEnd();
    return;
  }
  if (state.cancelled) { state.discarded++; return; }
  const bytes = JSON.stringify(message.event).length;
  if (state.queued >= 256 || state.bytes + bytes > 4*1024*1024) {
    state.error = "input callback queue overflow; recording has a gap";
    state.cancelled = true;
    void inputContext.run({id:state.id}, () => native("unsubscribe_input", {id:state.id})).catch(() => {});
    return;
  }
  state.queued++; state.bytes += bytes;
  state.tail = state.tail.then(async () => {
    state.queued--; state.bytes -= bytes;
    if (state.cancelled) { state.discarded++; return; }
    await inputContext.run({id:state.id}, async () => {
      let timer;
      try {
        await Promise.race([
          Promise.resolve().then(() => state.callback(message.event)),
          new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("input callback exceeded its time limit")), state.callbackTimeoutMs); }),
        ]);
      } catch (error) {
        state.error = printable(error?.stack ?? error); state.cancelled = true;
        try { await native("unsubscribe_input", {id:state.id}); await native("finish_input", {id:state.id}); } catch {}
      } finally { clearTimeout(timer); }
    });
  });
}
async function subscribeStream(args, callback, method) {
  if (typeof callback !== "function") throw new TypeError("onInput callback must be a function");
  const callbackTimeoutMs = args?.callbackTimeoutMs ?? 5000;
  if (!Number.isInteger(callbackTimeoutMs) || callbackTimeoutMs < 1 || callbackTimeoutMs > 120000) throw new RangeError("callbackTimeoutMs must be from 1 through 120000");
  const start = await native(method, args);
  const state = {id:start.id, callback, callbackTimeoutMs, tail:Promise.resolve(), queued:0, bytes:0, cancelled:false, discarded:0, error:null, end:null};
  state.ended = new Promise(resolve => { state.resolveEnd = resolve; });
  inputSubscriptions.set(state.id, state);
  const buffered = earlyInput.get(state.id) ?? []; earlyInput.delete(state.id);
  for (const message of buffered) acceptInput(message);
  return Object.freeze({
    id:state.id, initialState:start.initialState,
    status: () => ({active:!state.end && !state.cancelled, queued:state.queued, discarded:state.discarded, error:state.error ?? state.end?.error ?? null, end:state.end}),
    ...(method.startsWith("subscribe_visual") ? {metrics: () => native("visual_metrics",{id:state.id})} : {}),
    unsubscribe: async ({drain=true}={}) => {
      if (drain && inputContext.getStore()?.id === state.id) throw new Error("cannot drain a subscription from its own callback; use drain:false");
      if (!drain) state.cancelled = true;
      if (!state.end) await native("unsubscribe_input", {id:state.id});
      await state.ended;
      if (drain) await state.tail;
      // Retain status on the returned handle, but release broker authority and routing state.
      if (drain) { await native("finish_input", {id:state.id}); inputSubscriptions.delete(state.id); }
      else { void state.tail.then(() => inputContext.run({id:state.id}, () => native("finish_input", {id:state.id}))).then(() => inputSubscriptions.delete(state.id)).catch(() => {}); }
      return {endSequence:state.end?.sequence, discarded:state.discarded, error:state.error ?? state.end?.error ?? null};
    },
  });
}

const onInput = (args, callback) => subscribeStream(args, callback, "subscribe_input");
const onVisual = (args, callback) => subscribeStream(args, callback, "subscribe_visual");

const onVisualProgram = (args,callback) => {
  if(typeof callback!=="function") throw new TypeError("onVisualProgram callback must be a function");
  let parameters=args.parameters??[];
  if(Object.prototype.toString.call(parameters)==="[object ArrayBuffer]") parameters=Array.from(new Uint8Array(parameters));
  else if(ArrayBuffer.isView(parameters)) parameters=Array.from(new Uint8Array(parameters.buffer,parameters.byteOffset,parameters.byteLength));
  return subscribeStream({...args,parameters}, event => {
    const {words,...metadata}=event;
    return callback(Uint32Array.from(words).buffer,Object.freeze(metadata));
  },"subscribe_visual_program");
};

const wayland = Object.freeze({
  help: apiHelp,
  onInput,
  onVisual,
  onVisualProgram,
  presentImage: (imageHandle) => native("present_image", {imageHandle}),
  disposeImage: (imageHandle) => native("dispose_image", {imageHandle}),
  keyNames: namedKeyNames,
  environment: () => native("environment"),
  diagnostics: () => native("diagnostics"),
  windows: () => native("windows"),
  visualInfo: (args) => native("visual_info",args),
  resizeWindow: (args) => native("resize_window", args),
  screenshot: (args = {}) => native("screenshot", args),
  captureNextFrame: (args = {}) => native("capture_next_frame", args),
  pointerEvent: async (args) => {
    pointerTarget.windowId = null;
    return await native("pointer_event", args);
  },
  touchEvent: (args) => native("touch_event", args),
  inputCapabilities: (args) => native("input_capabilities", args),
  beginObservation: (args) => native("begin_observation", args),
  endObservation: (args) => native("end_observation", args),
  keyboardEvent: async (args) => {
    return await native("keyboard_event", args);
  },
  waitForWindow,
  waitForWindowGone,
  waitForCommit,
  actAndCapture,
  click,
  doubleClick,
  move,
  drag,
  scroll,
  pressKey,
  pressShortcut,
  typeText,
  resetInputState,
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
});

const context = vm.createContext({
  wayland,
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
  structuredClone,
  performance: Object.freeze({ now: () => performance.now() }),
  console: Object.freeze({
    log: (...args) => appendLog(args),
    error: (...args) => appendLog(args),
  }),
});
context.globalThis = context;

async function evaluate(message) {
  const { id, code } = message;
  const logs = [];
  await evalContext.run({ id, logs }, async () => {
    try {
      const source = `(async () => {\n${code}\n})()`;
      const value = await new vm.Script(source, { filename: "wayland-console.js" })
        .runInContext(context, { timeout: SYNC_EVAL_TIMEOUT_MS });
      write({ type: "eval_result", id, ok: true, value: boundedResult(value), logs });
    } catch (error) {
      write({
        type: "eval_result",
        id,
        ok: false,
        error: error?.stack ?? String(error),
        logs,
      });
    } finally {
      for (const [callId, call] of pendingNative) {
        if (call.evalId === id && call.subscriptionId === null) {
          pendingNative.delete(callId); call.reject(new Error("foreground evaluation ended before the native call completed"));
        }
      }
    }
  });
}

const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", (line) => {
  let message;
  try { message = JSON.parse(line); } catch (error) {
    write({ type: "runtime_error", error: `invalid host JSON: ${error}` });
    return;
  }
  if (message.type === "eval") {
    void evaluate(message);
    return;
  }
  if (message.type === "input_event" || message.type === "input_end") { acceptInput(message); return; }
  if (message.type === "native_result") {
    const pending = pendingNative.get(message.id);
    if (!pending) return;
    pendingNative.delete(message.id);
    if (message.ok) pending.resolve(message.value);
    else pending.reject(new Error(message.error));
  }
});

setInterval(() => write({type: "heartbeat"}), 500).unref();
write({type: "ready"});
