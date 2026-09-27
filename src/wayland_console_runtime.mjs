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

const pointerFocus = { windowId: null };
const keyboardFocus = { windowId: null };

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

async function ensurePointerFocus(windowId, x, y, events) {
  if (pointerFocus.windowId !== windowId) {
    if (pointerFocus.windowId !== null) {
      const previousWindowId = pointerFocus.windowId;
      pointerFocus.windowId = null;
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
    events.push(await native("pointer_event", { windowId, event: { type: "enter", x, y } }));
    pointerFocus.windowId = windowId;
  }
}

async function move({ windowId, ...coordinates }) {
  const { x, y } = fullPoint(coordinates);
  const events = [];
  await ensurePointerFocus(windowId, x, y, events);
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
  await ensurePointerFocus(windowId, start.x, start.y, events);
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
  await ensurePointerFocus(windowId, x, y, events);
  events.push(await native("pointer_event", { windowId, event: { type: "motion", x, y } }));
  events.push(await native("pointer_event", { windowId, event: { type: "axis_source", axis_source: 0 } }));
  events.push(await native("pointer_event", { windowId, event: { type: "axis", axis: 0, value } }));
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  return { delivered: true, point: { x, y }, deltaY, rawFixedValue: value, events };
}

async function ensureKeyboardFocus(windowId, events) {
  if (keyboardFocus.windowId !== windowId) {
    events.push(await native("keyboard_event", { windowId, event: { type: "enter", keys: [] } }));
    keyboardFocus.windowId = windowId;
  }
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
  await ensureKeyboardFocus(windowId, events);
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
    throw new TypeError("keys must contain from 2 through 8 key names or evdev codes");
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
  await ensureKeyboardFocus(windowId, events);
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
  await ensureKeyboardFocus(windowId, events);
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
  return native("capture_next_frame", { windowId, afterCommitSerial, timeoutMs });
}

async function actAndCapture({ windowId, action, afterCommitSerial, timeoutMs = 5000 }) {
  if (typeof action !== "function") throw new TypeError("action must be a function returning an input-operation promise");
  const beforeWindows = await native("windows");
  const before = beforeWindows.find((window) => window.window_id === windowId && window.mapped !== false);
  if (!before) throw new Error(`window ${JSON.stringify(windowId)} is not mapped`);
  const baselineCommitSerial = afterCommitSerial ?? before.commit_serial;
  const startedAt = Date.now();
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
}

function resetInputState() {
  pointerFocus.windowId = null;
  keyboardFocus.windowId = null;
  return { pointerFocusReset: true, keyboardFocusReset: true };
}

const apiHelp = `Persistent JavaScript console. State survives calls.
Start with the built-in helpers:
  return await wayland.click({windowId:"window-id", x:100, y:100});
Routine helpers: wayland.waitForWindow(selector), wayland.waitForWindowGone(selector), wayland.click(args),
wayland.doubleClick(args), wayland.move(args),
wayland.drag({windowId,from,to,durationMs,steps}),
wayland.resizeWindow({windowId,width,height}),
wayland.scroll({windowId,x,y,deltaY}), wayland.pressKey({windowId,key,holdMs}),
wayland.pressShortcut({windowId,keys}), wayland.typeText({windowId,text}),
wayland.waitForCommit(args), wayland.actAndCapture(args), wayland.resetInputState(). Raw focus events automatically invalidate helper focus state. Use resetInputState
after external focus changes; it clears cached focus only, not pressed keys. Coordinates default to
full screenshot pixels; pass coordinateSpace:"preview" and the returned
previewToFullScale object (or the response's preview_to_full_scale object) to
convert preview coordinates safely.
typeText and character keys in pressShortcut use the exact XKB keymap sent to
the target client and its active layout group; characters absent from that
layout fail explicitly instead of falling back to hardcoded physical keys.
For applications supporting Ctrl+Shift+U hexadecimal Unicode entry (including
Chromium in the tested environment), explicitly use
wayland.typeText({windowId,text:"🌘",inputMethod:"unicode-hex"}). This types every
code point through that application convention; it is not universal and does
not use the clipboard.
pressKey accepts evdev codes, the names in wayland.keyNames, and one-character
keys case-insensitively; character keys also use the target client's XKB map.
Raw calls: environment(), diagnostics(), windows(), screenshot({windowId}),
captureNextFrame({windowId, afterCommitSerial, timeoutMs}),
beginObservation({windowId, durationMs}), endObservation({id}),
resizeWindow({windowId,width,height}),
pointerEvent({windowId,event}), keyboardEvent({windowId,event}),
touchEvent({windowId,surfaceId,event}), inputCapabilities({windowId}), sleep(ms).
environment() returns WAYLAND_DISPLAY, XDG_RUNTIME_DIR, an absolute socket_path,
and launch_preflight; caller namespace and render-node access remain not_tested.
diagnostics() reports sanitized proxy and sandbox status.
onInput({windowId,origin:"human",devices:["pointer"]}, callback) subscribes to
ordered delivered input and works between evaluations. The handle has initialState,
status(), and unsubscribe({drain:true}). Call drain:false from within a callback.
Events use signed 24.8 surface-fixed coordinates and trusted origin labels.
Queues are bounded; status reports overflow or callback failures. No recorder or
player is built in: collect events and replay them using ordinary JS.
windows() reports capture-output and backend-output membership separately.
Each input call emits exactly one compositor-side protocol event; author
sequences yourself. wl_pointer event types and fields:
  enter{x,y,serial?}, leave{serial?}, motion{x,y,time?}, button{button,state,serial?,time?},
  axis{axis,value,time?}, axis_source{axis_source}, axis_stop{axis,time?},
  axis_discrete{axis,discrete}, axis_value120{axis,value120},
  axis_relative_direction{axis,direction}, frame{},
  relative_motion{utime_hi,utime_lo,dx,dy,dx_unaccel,dy_unaccel}.
Relative deltas are signed 24.8 fixed integers; timestamps are 64-bit microseconds
split into two unsigned words. onInput devices also accepts relative_pointer,
pointer_constraints and touch. Pointer constraints are requested by the client;
model focus activates its lock/confinement, and absolute motion is rejected while
locked. inputCapabilities lists bound resources and versions for the target.
touchEvent uses surface-local signed 24.8 fixed x/y values and fresh serials/time:
  down{id,x,y}, motion{id,x,y}, up{id}, shape{id,major,minor},
  orientation{id,orientation}, frame{}, cancel{}.
Touch contact IDs remain active until up/cancel; call frame explicitly.
Recording and playback of these events remain agent-written JS.
axis.value is the raw signed wl_fixed 24.8 integer. x/y are full screenshot
pixels mapped to the input wl_surface. wl_keyboard event types and fields:
  enter{serial?,keys?:[evdevKey...]}, leave{serial?},
  key{key,state,serial?,time?},
  modifiers{mods_depressed,mods_latched,mods_locked,group,serial?},
  repeat_info{rate,delay}.
Key values are Linux evdev key codes; the client applies the XKB +8 offset.
Use ordinary JavaScript functions, loops, Promise.all, and timers to build
precise or repeated behavior. Track focus in your helpers: enter is a focus
transition, not a prefix for every action.`;

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
async function onInput(args, callback) {
  if (typeof callback !== "function") throw new TypeError("onInput callback must be a function");
  const callbackTimeoutMs = args?.callbackTimeoutMs ?? 5000;
  if (!Number.isInteger(callbackTimeoutMs) || callbackTimeoutMs < 1 || callbackTimeoutMs > 120000) throw new RangeError("callbackTimeoutMs must be from 1 through 120000");
  const start = await native("subscribe_input", args);
  const state = {id:start.id, callback, callbackTimeoutMs, tail:Promise.resolve(), queued:0, bytes:0, cancelled:false, discarded:0, error:null, end:null};
  state.ended = new Promise(resolve => { state.resolveEnd = resolve; });
  inputSubscriptions.set(state.id, state);
  const buffered = earlyInput.get(state.id) ?? []; earlyInput.delete(state.id);
  for (const message of buffered) acceptInput(message);
  return Object.freeze({
    id:state.id, initialState:start.initialState,
    status: () => ({active:!state.end && !state.cancelled, queued:state.queued, discarded:state.discarded, error:state.error ?? state.end?.error ?? null, end:state.end}),
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

const wayland = Object.freeze({
  help: apiHelp,
  onInput,
  presentImage: (imageHandle) => native("present_image", {imageHandle}),
  disposeImage: (imageHandle) => native("dispose_image", {imageHandle}),
  keyNames: namedKeyNames,
  environment: () => native("environment"),
  diagnostics: () => native("diagnostics"),
  windows: () => native("windows"),
  resizeWindow: (args) => native("resize_window", args),
  screenshot: (args = {}) => native("screenshot", args),
  captureNextFrame: (args = {}) => native("capture_next_frame", args),
  pointerEvent: async (args) => {
    pointerFocus.windowId = null;
    return await native("pointer_event", args);
  },
  touchEvent: (args) => native("touch_event", args),
  inputCapabilities: (args) => native("input_capabilities", args),
  beginObservation: (args) => native("begin_observation", args),
  endObservation: (args) => native("end_observation", args),
  keyboardEvent: async (args) => {
    keyboardFocus.windowId = null;
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
