import vm from "node:vm";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";
import { Buffer } from "node:buffer";
import { inspect } from "node:util";

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
const backgroundLogs = [];
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

function appendLog(args, logs = evalContext.getStore()?.logs) {
  if (!logs) return;
  if (logs.length < MAX_LOG_ENTRIES) {
    logs.push(args.map(printable).join(" "));
  } else if (logs.length === MAX_LOG_ENTRIES) {
    logs.push(`[additional console output omitted after ${MAX_LOG_ENTRIES} entries]`);
  }
}

process.on("unhandledRejection", reason => {
  let detail;
  try {
    detail = inspect(reason, { customInspect: false, depth: 1, maxArrayLength: 10, maxStringLength: MAX_LOG_CHARS });
  } catch {
    detail = "Rejected promise could not be inspected";
  }
  appendLog(["Unhandled promise rejection:", detail], backgroundLogs);
});

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
  const message = {type:"native_call", id, evalId, subscriptionId, method, args};
  if (method.startsWith("dbus_")) {
    const frame = `${JSON.stringify(message)}\n`;
    if (Buffer.byteLength(frame) > 2 * 1024 * 1024) {
      return Promise.reject(new RangeError("D-Bus request exceeds the 2 MiB console limit"));
    }
    process.stdout.write(frame);
  } else {
    write(message);
  }
  const promise = new Promise((resolve, reject) => pendingNative.set(id, {resolve, reject, evalId, subscriptionId}));
  // Evaluation cleanup can reject an internal call after its consumer has ended.
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
  events.push(await native("keyboard_event", { windowId, ensureFocus: true, event: { type: "enter", keys: [] } }));
}

async function finishKeyboardEvent(windowId, event, events) {
  const result = await native("keyboard_event", { windowId, event, allowMissingWindow: true });
  if (result.surfaceDisappeared) return true;
  events.push(result);
  return false;
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
  let surfaceDisappeared = false;
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
    surfaceDisappeared = await finishKeyboardEvent(windowId, { type: "key", key: keyCode, state: 0 }, events);
    keyDown = false;
  } finally {
    try {
      if (keyDown) {
        surfaceDisappeared = await finishKeyboardEvent(windowId, { type: "key", key: keyCode, state: 0 }, events);
      }
    } finally {
      if (modifiersChanged && !surfaceDisappeared) {
        surfaceDisappeared = await finishKeyboardEvent(windowId, {
          type: "modifiers",
          mods_depressed: plan.restore_mods_depressed,
          mods_latched: plan.restore_mods_latched,
          mods_locked: plan.restore_mods_locked,
          group: plan.layout_group,
        }, events);
      }
    }
  }
  return {
    delivered: true, key, keyCode, holdMs, keymapDriven: plan !== null,
    modifierMask: stroke?.modifiers ?? null, events, ...(surfaceDisappeared ? {surfaceDisappeared} : {}),
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
  let surfaceDisappeared = false;
  try {
    modifiersChanged = true;
    events.push(await native("keyboard_event", { windowId, event: {
      type: "modifiers", mods_depressed: depressed, mods_latched: 0, mods_locked: 0,
      group: plan.layout_group,
    } }));
    keyDown = true;
    events.push(await native("keyboard_event", { windowId, event: { type: "key", key: keyCode, state: 1 } }));
    if (holdMs > 0) await new Promise((resolve) => setTimeout(resolve, holdMs));
    surfaceDisappeared = await finishKeyboardEvent(windowId, { type: "key", key: keyCode, state: 0 }, events);
    keyDown = false;
  } finally {
    try {
      if (keyDown) {
        surfaceDisappeared = await finishKeyboardEvent(windowId, { type: "key", key: keyCode, state: 0 }, events);
      }
    } finally {
      if (modifiersChanged && !surfaceDisappeared) {
        surfaceDisappeared = await finishKeyboardEvent(windowId, {
          type: "modifiers",
          mods_depressed: plan.restore_mods_depressed,
          mods_latched: plan.restore_mods_latched,
          mods_locked: plan.restore_mods_locked,
          group: plan.layout_group,
        }, events);
      }
    }
  }
  return { delivered: true, keys, keyCode, modifierMask: depressed, holdMs, events, ...(surfaceDisappeared ? {surfaceDisappeared} : {}) };
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
  const plan = await native("keyboard_text_plan", { windowId, text, allowCompose: true });
  const composeStarts = new Set(plan.compose_starts);
  const events = [];
  await enterKeyboardTarget(windowId, events);
  let depressed = null;
  let keyDown = null;
  try {
    for (const [index, stroke] of plan.strokes.entries()) {
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
      // Give the toolkit time to process the prior edit, the dead key, and
      // its compose commit. Plain typing keeps the caller's requested pace.
      const compose = composeStarts.has(index + 1) || composeStarts.has(index) || composeStarts.has(index - 1);
      const delay = compose ? Math.max(intervalMs, 35) : intervalMs;
      if (delay > 0) await new Promise((resolve) => setTimeout(resolve, delay));
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
    const mappedWindow = async () => (await native("windows"))
      .find((window) => window.window_id === windowId && window.mapped !== false);
    let capture = null;
    let captureError = null;
    let after = await mappedWindow();
    if (after) {
      try {
        capture = await native("capture_next_frame", { windowId, afterCommitSerial: baselineCommitSerial, timeoutMs });
      } catch (error) {
        captureError = error instanceof Error ? error.message : String(error);
      }
      after = await mappedWindow();
      if (!after && (captureError?.startsWith("unknown windowId") || captureError === "capture window disappeared")) {
        captureError = null;
      }
    }
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
Await foreground operations before returning. Unhandled promise rejections
from background work appear in the next evaluation's console output.

D-Bus scripts use wayland.dbus. Open a normal client with connect({address})
and invoke bus.call({destination,path,interface,member,signature,body}).
For an app-specific bus, use open({session:true,accessibility:true}) and pass
bridge.environment to the target app at launch. Use bridge.session and
bridge.accessibility to call methods, observe app traffic, or serve JS handlers.
Read ordinary client signals with bus.subscribe(matchRule, callback).
Publish a local service with bus.serve({name}) and service.exportObject().
List D-Bus protocol guides with wayland.dbus.help().
Close resources with bus.close() or bridge.close().

Open a Wayland connection over SSH from this console.
  return await wayland.connectSsh({host:"user@host"})
The result says connected after a Wayland check makes the round trip over SSH.
It includes the environment to use when starting the app on the SSH host.
For example, if connectSsh for user@host returns
environment.WAYLAND_DISPLAY = "/tmp/wmc-app-EXAMPLE/wayland", run this in your terminal:
  ssh -t user@host 'env -u WAYLAND_SOCKET WAYLAND_DISPLAY=/tmp/wmc-app-EXAMPLE/wayland vkcube --wsi wayland'
Use the actual WAYLAND_DISPLAY from the result. Leave the app running while you use this console.
Use return await wayland.windows() to see the app after it opens. Use the same
wayland.screenshot(), wayland.click() and other commands you use for local apps.
This MCP captures frames and sends input. The bridge carries the app's Wayland
traffic over SSH. Use return await wayland.disconnectSsh() to close the bridge.
Connecting to a new host closes the previous bridge after the new one is ready.

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
windows() has no desktop focus flag. diagnostics() reports sanitized proxy listener,
launch preflight and console permission status.
An upstream connection or registry-discovery failure rejects the client with
wl_display.error and is reported on stderr. There is no local compositor.
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
Wine and Waydroid can put the GPU frame in a child of a shared-memory window.
For Wine, launch with the proxy environment and DISPLAY unset to use the native
Wayland driver. Wine using the X11 driver bypasses this proxy.
For Waydroid, set WAYLAND_DISPLAY to environment().socket_path when running
\`waydroid session start\`. Keep the desktop's XDG_RUNTIME_DIR so Waydroid can find
the session services. Launch the Android app with \`waydroid app launch PACKAGE\`.
If window capture reports shared memory, inspect windows().subsurfaces for a child
with buffer_kind "dmabuf" and has_committed_buffer true. Pass its numeric surface_id
as surfaceId to the buffer screenshot above. Capture during animation, or start the
screenshot before an action that causes a redraw. An idle child can time out.
The raw child capture leaves out window geometry, decorations and viewport cropping.
onVisual and onVisualProgram accept a numeric surfaceId for an independently committed DMA-BUF child.
videoFrame({windowId}) captures a root DMA-BUF as a verified H.265 IDR picture. It prefers
10-bit 4:4:4 SCC and falls back to 8-bit 4:2:0 Main based on both codec devices.
Set chroma (for example 444 or "4:4:4") and bitDepth to choose sampling and depth. For 4:4:4, omit screenContent
to prefer SCC with a range-extension fallback, or set it to pin the profile. Options include
encodeGpu and decodeGpu (DRM render-node
paths), qp (0..51), toneMap ("auto", "clip", "knee", "reinhard", "curve"),
exposure, and background ("black", "checkerboard"). Explicit tone maps apply to FP16 and
tagged packed 10-bit HDR. A custom curve accepts
toneCurve: [[0,0],[1,0.8],[4,1]] in linear reference-white units. The Vulkan
shader evaluates it. Set afterCommitSerial to require a newer commit, with an
optional timeoutMs. When the producer omits DRM feedback, captureGpu selects
the render node used to import its buffer. Each call can change the options.
startRecording({windowId,maxFrames,frameRate:30}) starts background H.265 capture
and returns a recordingId and a .265 path on the capture host. maxFrames is required,
positive, and limited to eight hours at frameRate (1..60).
recordingStatus({recordingId}) reports framesWritten, bytesWritten and stopReason.
endRecording({recordingId}) stops early and finishes the buffered pictures.
The server also stops at maxFrames, the recording duration, 512 MiB, or low disk space.
Recordings include the window's subsurfaces and alpha blending against black.
An idle SHM surface with no retained pixels must redraw after capture starts.
configureSshVideo(options) changes video settings for the connected bridge.
For terminal testing, start the bridge with --ssh-wayland user@host -- bash.
A bridge started from a terminal uses WAYLAND_MCP_SSH_VIDEO_SOCKET for video
settings. See Wayland over SSH in the README. configureSshVideo accepts
chroma, bitDepth, screenContent, captureGpu, encodeGpu, decodeGpu, rateControl, toneMap,
toneCurve, exposure, background and sourceColor. The bridge preserves alpha by default.
rateControl defaults to {mode:"auto"}, which prefers constant quality and uses
VBR when no compatible configuration supports constant quality. Set
{mode:"constantQuality",quality:24,frameRate:120} or
{mode:"vbr",targetBitrate:6000000,frameRate:120} to choose a mode explicitly.
Set background to "black" or "checkerboard" to composite it on the GPU.
Newly encoded buffers use the settings.
screenshot in window coordinates waits up to 250 ms for new pixels, then returns
the readable current frame. Each screenshot reports commitSerial, baselineCommitSerial,
and fresh. fresh is true when the captured pixels were updated after the baseline.
A new frame can still show an app partway through processing input.
captureNextFrame({windowId,afterCommitSerial,timeoutMs:1000}) requires newer pixels;
timeoutMs is 1..10000. Omitting afterCommitSerial uses the current commit as baseline.
Prefer actAndCapture({windowId,action:()=>wayland.click({windowId,x,y}),timeoutMs:5000});
it starts observation BEFORE action, captures relative to the prior commit, and returns
actionResult, frameObserved, surfaceDisappeared, capture, captureError and commit serials.
If the action closes the window, surfaceDisappeared is true and capture can be null.
Delivered input or a newer commit does not prove the application accepted the action.
beginObservation({windowId,durationMs:5000}) returns {id,durationMs}; duration is 1..120000.
endObservation({id}) ends that lease. Leases expire automatically (at most 32 active).
Capture/input leases and active visual subscriptions drive committed frame callbacks
without raising host windows. Input keeps its target rendering/capturable for 500 ms.
The proxy retains SHM and GPU pixels for capture during a lease and may reuse an
owned copy. It cannot reread released buffers. Enable observation before an action
to retain its fresh pixels.
Continuous onVisual/onVisualProgram analysis keeps frames/history/scratch/state on GPU;
only declared bounded result records reach JS. No shader-file/SPIR-V uploads.
GLSL source strings compile in memory; fixed trusted kernels are embedded at build time.
Observe mapped proxied windows with environment(), diagnostics(), windows(), waitForWindow().
visualInfo({windowId,surfaceId}) reports buffer dimensions, scale/viewport, format and DRM affinity;
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
Callbacks can conditionally emit input between console calls. handle.metrics() reports
cost, skips and last pixel commit age, including after the subscription ends.
Set requireEveryFrame:true to terminate on the first skipped pixel commit.
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
Events: {windowId,surfaceId,ruleId,active,frame,commitSerial,timestampMs,deliveryTimestampMs,sequence}.
Only events are transferred to CPU. Rule order is stable within each observed commit.
The callback runs between evaluations and may perform asynchronous input operations.
Handle: {id,initialState,status(),unsubscribe({drain:true})}; use drain:false inside callback.
callbackTimeoutMs defaults to 5000 (1..120000); a timeout ends with reason callback_timeout.
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
intervalMs/holdMs 0..60000. For spacing accents such as ^ on German keyboards,
typeText can send the dead key followed by Space with a minimum interval of 35 ms around
the sequence. This uses the app's compose support.
"unicode-hex" is an opt-in application convention.
Use wayland.clipboard.write({windowId,formats:{"text/html":"<b>Hello</b>","text/plain":"Hello"}})
then pressShortcut({windowId,keys:["CTRL","v"]}) for rich text. Clipboard methods
use the private model selection for the window's seat. types({windowId}) lists MIME
types. read({windowId,mimeType}) returns a Uint8Array or null if the format is absent.
readText({windowId,mimeType}) decodes UTF-8. Writes accept strings or Uint8Arrays.
An empty formats map clears the selection. The console clipboard limit is 1 MiB.
Pass a seatId from inputCapabilities if the window binds multiple seats.
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
    state.error = state.visual ? "visual callback queue overflow; stream has a gap" : "input callback queue overflow; recording has a gap";
    state.cancelled = true;
    void inputContext.run({id:state.id}, () => stopFailedCallback(state, "callback_overflow"));
    return;
  }
  state.queued++; state.bytes += bytes;
  state.tail = state.tail.then(async () => {
    state.queued--; state.bytes -= bytes;
    if (state.cancelled) { state.discarded++; return; }
    await inputContext.run({id:state.id}, async () => {
      let timer;
      let timedOut = false;
      try {
        await Promise.race([
          Promise.resolve().then(() => state.callback(message.event)),
          new Promise((_, reject) => {
            timer = setTimeout(() => {
              timedOut = true;
              reject(new Error(`${state.visual ? "visual" : "input"} callback exceeded its time limit`));
            }, state.callbackTimeoutMs);
          }),
        ]);
      } catch (error) {
        state.error = printable(error?.stack ?? error); state.cancelled = true;
        await stopFailedCallback(state, timedOut ? "callback_timeout" : "callback_error");
      } finally { clearTimeout(timer); }
    });
  });
}
async function stopFailedCallback(state, reason) {
  try {
    const end = await native("unsubscribe_input", {id:state.id, reason});
    if (!state.end && end.reason !== "already_stopped") {
      state.end = end;
      state.resolveEnd();
      inputSubscriptions.delete(state.id);
    }
    await native("finish_input", {id:state.id});
  } catch {}
}
async function subscribeStream(args, callback, method) {
  if (typeof callback !== "function") throw new TypeError("onInput callback must be a function");
  const callbackTimeoutMs = args?.callbackTimeoutMs ?? 5000;
  if (!Number.isInteger(callbackTimeoutMs) || callbackTimeoutMs < 1 || callbackTimeoutMs > 120000) throw new RangeError("callbackTimeoutMs must be from 1 through 120000");
  const start = await native(method, args);
  const state = {id:start.id, callback, callbackTimeoutMs, visual:method.startsWith("subscribe_visual"), tail:Promise.resolve(), queued:0, bytes:0, cancelled:false, discarded:0, error:null, end:null};
  state.ended = new Promise(resolve => { state.resolveEnd = resolve; });
  inputSubscriptions.set(state.id, state);
  const buffered = earlyInput.get(state.id) ?? []; earlyInput.delete(state.id);
  for (const message of buffered) acceptInput(message);
  return Object.freeze({
    id:state.id, initialState:start.initialState,
    status: () => ({active:!state.end && !state.cancelled, queued:state.queued, discarded:state.discarded, error:state.error ?? state.end?.error ?? null, end:state.end}),
    ...(state.visual ? {metrics: async () => {
      if (state.end?.metrics) return structuredClone(state.end.metrics);
      try {
        return await native("visual_metrics",{id:state.id});
      } catch (error) {
        if (error.message !== "visual subscription is not active") throw error;
        await state.ended;
        if (!state.end?.metrics) throw error;
        return structuredClone(state.end.metrics);
      }
    }} : {}),
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

const dbusDescriptors = new WeakMap();

function dbusDescriptor(id) {
  const descriptor = Object.freeze({
    async read(maxBytes = 65536) {
      return DBusBytes.from(Buffer.from(await native("dbus_fd_read", { id, limit: maxBytes }), "base64"));
    },
    async write(bytes) {
      if (Object.prototype.toString.call(bytes) !== "[object Uint8Array]") {
        throw new TypeError("descriptor writes require a Uint8Array");
      }
      let offset = 0;
      while (offset < bytes.length) {
        const chunk = bytes.subarray(offset, offset + 4096);
        const count = await native("dbus_fd_write", { id, bytes: Buffer.from(chunk).toString("base64") });
        if (!count) throw new Error("descriptor write made no progress");
        offset += count;
      }
      return offset;
    },
    close: () => native("dbus_fd_close", { id }),
  });
  dbusDescriptors.set(descriptor, id);
  return descriptor;
}

function encodeDBusValue(value, depth) {
  if (depth >= 64) throw new RangeError("D-Bus value nesting exceeds the console limit");
  if (typeof value === "bigint") return value.toString();
  if (typeof value === "number" && (!Number.isFinite(value) || Object.is(value, -0))) {
    return { double: Object.is(value, -0) ? "-0" : String(value) };
  }
  const tag = Object.prototype.toString.call(value);
  if (tag === "[object Uint8Array]") return { bytes: Buffer.from(value).toString("base64") };
  if (Array.isArray(value)) return Array.from(value, item => encodeDBusValue(item, depth + 1));
  if (tag === "[object Object]") {
    if (dbusDescriptors.has(value)) return { descriptor: dbusDescriptors.get(value) };
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, encodeDBusValue(item, depth + 1)]));
  }
  if (value === null || ["number", "string", "boolean"].includes(typeof value)) {
    return value;
  }
  throw new TypeError(`unsupported D-Bus value ${tag}`);
}

function encodeDBusMessage(message) {
  if (Object.prototype.toString.call(message) !== "[object Object]") {
    throw new TypeError("expected a D-Bus message or reply object");
  }
  const { body, ...fields } = message;
  if (body === undefined) return fields;
  if (!Array.isArray(body)) throw new TypeError("D-Bus body must be an array");
  return { ...fields, body: encodeDBusValue(body, 0) };
}

function decodeDBusValue(value) {
  if (Array.isArray(value)) return value.map(decodeDBusValue);
  switch (value?.kind) {
    case "integer": return BigInt(value.value);
    case "double": return Number(value.value);
    case "bytes": return DBusBytes.from(Buffer.from(value.value, "base64"));
    case "fd": return dbusDescriptor(value.value);
    case "fdMetadata": return Object.freeze({ type: "unixFd" });
    case "variant": return { signature: value.signature, value: decodeDBusValue(value.value) };
    case "dictionary": {
      const pairs = value.value.map(decodeDBusValue);
      return value.stringKeys ? Object.fromEntries(pairs) : pairs;
    }
    default: return value;
  }
}

const dbusHandlers = new Map();
let nextDBusHandler = 1;

function removeDBusHandlers(predicate) {
  for (const [id, entry] of dbusHandlers) {
    if (predicate(entry)) dbusHandlers.delete(id);
  }
}

function dbusHandle({ id, uniqueName }, bridgeId, appFacing = false) {
  let closing;
  const register = async (operation, fields, callback) => {
    const handler = nextDBusHandler++;
    dbusHandlers.set(handler, { scope: id, bridgeId, callback });
    try {
      await native(operation, { id, handler, ...fields });
    } catch (error) {
      dbusHandlers.delete(handler);
      throw error;
    }
    return handler;
  };
  const unregister = async (operation, handler) => {
    try {
      await native(operation, { id, handler });
    } finally {
      dbusHandlers.delete(handler);
    }
  };
  return Object.freeze({
    uniqueName,
    fds: Object.freeze({ async pipe() {
      const result = await native("dbus_pipe", { id });
      return { readEnd: decodeDBusValue(result.readEnd), writeEnd: decodeDBusValue(result.writeEnd) };
    } }),
    async call(message) {
      const result = await native("dbus_call", { id, message: encodeDBusMessage(message) });
      return { ...result, body: decodeDBusValue(result.body) };
    },
    async emitSignal(message) {
      await native("dbus_signal", { id, message: encodeDBusMessage(message) });
    },
    async subscribe(rule, callback) {
      if (typeof callback !== "function") throw new TypeError("subscribe requires a callback");
      const handler = await register("dbus_subscribe", { rule }, callback);
      return Object.freeze({ unsubscribe: () => unregister("dbus_unsubscribe", handler) });
    },
    close() {
      closing ??= native("dbus_close", { id }).then(() => {
        removeDBusHandlers(entry => entry.scope === id);
      }).catch(error => {
        closing = undefined;
        throw error;
      });
      return closing;
    },
    ...(appFacing ? {
      async observe(match, callback) {
        if (typeof callback !== "function") throw new TypeError("observe requires a callback");
        const handler = await register("dbus_observe", { match }, callback);
        return Object.freeze({ unsubscribe: () => unregister("dbus_unobserve", handler) });
      },
      async serve({ name }) {
        const service = await native("dbus_serve", { id, name });
        return dbusHandle(service, bridgeId);
      },
    } : bridgeId === null ? {} : {
      async exportObject(definition, methods) {
        const callbacks = new Map(Object.keys(definition.methods).map(name => {
          if (!Object.hasOwn(methods, name) || typeof methods[name] !== "function") {
            throw new TypeError(`missing handler for ${name}`);
          }
          return [name, methods[name]];
        }));
        const handler = await register("dbus_export", { definition }, call => callbacks.get(call.member)(call));
        return Object.freeze({ unexport: () => unregister("dbus_unexport", handler) });
      },
    }),
  });
}

async function acceptDBusEvent(event) {
  const registration = dbusHandlers.get(event.handler);
  if (!registration || registration.scope !== event.scope) return;
  const message = { ...event.message, body: decodeDBusValue(event.message.body) };
  if (event.direction !== undefined) {
    message.direction = event.direction;
    message.connection = event.connection;
  }
  return inputContext.run({ id: event.scope }, async () => {
    if (event.call === undefined) {
      await registration.callback(message);
      return;
    }
    const afterReply = [];
    let replying = false;
    let answer;
    try {
      const result = await registration.callback({ ...message, afterReply(callback) {
        if (replying) throw new Error("afterReply must be registered before returning a reply");
        if (typeof callback !== "function") throw new TypeError("afterReply requires a callback");
        afterReply.push(callback);
      } });
      answer = encodeDBusMessage(result ?? {});
    } catch (error) {
      answer = { errorName: "org.freedesktop.DBus.Error.Failed", errorMessage: String(error).slice(0, 4096) };
    }
    replying = true;
    await native("dbus_reply", { id: event.call, answer });
    if (!answer.errorName) {
      for (const callback of afterReply) await callback();
    }
  });
}

const dbus = Object.freeze({
  help: dbusProtocolHelp,
  async connect({ address }) {
    return dbusHandle(await native("dbus_connect", { address }), null);
  },
  async open(options = {}) {
    const { id, environment, session, accessibility } = await native("dbus_open", options);
    let closing;
    return Object.freeze({ environment: Object.freeze(environment),
      session: session ? dbusHandle(session, id, true) : undefined,
      accessibility: accessibility ? dbusHandle(accessibility, id, true) : undefined,
      close() {
        closing ??= native("dbus_shutdown", { id }).then(() => {
          removeDBusHandlers(entry => entry.bridgeId === id);
        }).catch(error => {
          closing = undefined;
          throw error;
        });
        return closing;
      },
    });
  },
  atspi,
  clipboard,
  fileChooser: Object.freeze({ onChooseFile }),
});

async function readPrivateClipboard(args) {
  const base64 = await native("clipboard_read", args);
  return base64 === null ? null : Buffer.from(base64, "base64");
}

const privateClipboard = Object.freeze({
  types: args => native("clipboard_types", args),
  async write({ windowId, seatId, formats }) {
    if (formats === null || typeof formats !== "object" || Array.isArray(formats)) {
      throw new TypeError("clipboard write requires a MIME type to data map");
    }
    const entries = Object.entries(formats);
    if (entries.length > 64) throw new Error("clipboard MIME limit exceeded");
    let size = 0;
    const encoded = Object.fromEntries(entries.map(([mime, data]) => {
      if (!mime || Buffer.byteLength(mime) > 512 || mime.includes("\0")) {
        throw new TypeError("invalid clipboard MIME type");
      }
      if (typeof data !== "string" && Object.prototype.toString.call(data) !== "[object Uint8Array]") {
        throw new TypeError("clipboard data must be a string or Uint8Array");
      }
      size += Buffer.byteLength(data);
      if (size > 1024 * 1024) throw new Error("console clipboard payload exceeds 1 MiB");
      return [mime, Buffer.from(data).toString("base64")];
    }));
    await native("clipboard_write", { windowId, seatId, formats: encoded });
  },
  async read(args) {
    const bytes = await readPrivateClipboard(args);
    return bytes === null ? null : Uint8Array.from(bytes);
  },
  async readText(args) {
    const bytes = await readPrivateClipboard(args);
    return bytes === null ? null : bytes.toString("utf8");
  },
});

const wayland = Object.freeze({
  help: apiHelp,
  clipboard: privateClipboard,
  dbus,
  onInput,
  onVisual,
  onVisualProgram,
  presentImage: (imageHandle) => native("present_image", {imageHandle}),
  disposeImage: (imageHandle) => native("dispose_image", {imageHandle}),
  keyNames: namedKeyNames,
  connectSsh: (options) => native("ssh_connect", options),
  disconnectSsh: () => native("ssh_disconnect"),
  environment: () => native("environment"),
  diagnostics: () => native("diagnostics"),
  windows: () => native("windows"),
  visualInfo: (args) => native("visual_info",args),
  resizeWindow: (args) => native("resize_window", args),
  screenshot: (args = {}) => native("screenshot", args),
  videoFrame: (args = {}) => native("video_frame", args),
  startRecording: (args = {}) => native("start_recording", args),
  recordingStatus: (args = {}) => native("recording_status", args),
  endRecording: (args = {}) => native("end_recording", args),
  configureSshVideo: (options = {}) => native("configure_ssh_video", options),
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
const DBusBytes = vm.runInContext("Uint8Array", context);

async function evaluate(message) {
  const { id, code } = message;
  const logs = backgroundLogs.splice(0);
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
  if (message.type === "dbus_event") {
    void acceptDBusEvent(message)
      .catch(error => console.error("D-Bus callback failed", error))
      .finally(() => {
        if (message.event !== undefined) write({ type: "dbus_event_done", event: message.event });
      });
    return;
  }
  if (message.type === "dbus_closed") {
    removeDBusHandlers(entry => entry.scope === message.scope);
    return;
  }
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
