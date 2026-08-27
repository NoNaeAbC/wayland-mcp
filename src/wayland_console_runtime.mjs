import vm from "node:vm";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

const pendingNative = new Map();
let nextNativeId = 1;
const evalContext = new AsyncLocalStorage();
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
  const id = nextNativeId++;
  write({ type: "native_call", id, evalId: evalContext.getStore()?.id ?? null, method, args });
  return new Promise((resolve, reject) => pendingNative.set(id, { resolve, reject }));
}

const pointerFocus = { windowId: null };
const keyboardFocus = { windowId: null };

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
  events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 1 } }));
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  events.push(await native("pointer_event", { windowId, event: { type: "button", button, state: 0 } }));
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  return { delivered: true, point: moved.point, button, events };
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
  events.push(await native("pointer_event", { windowId, event: { type: "frame" } }));
  return { delivered: true, from: start, to: end, button, durationMs, steps, events };
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

async function pressKey({ windowId, key, holdMs = 40 }) {
  if (!Number.isInteger(key) || key < 0 || key > 0xffffffff) {
    throw new TypeError("key must be a non-negative 32-bit evdev key code");
  }
  finiteNumber(holdMs, "holdMs");
  if (holdMs < 0 || holdMs > 60_000) throw new RangeError("holdMs must be from 0 through 60000");
  const events = [];
  if (keyboardFocus.windowId !== windowId) {
    events.push(await native("keyboard_event", { windowId, event: { type: "enter", keys: [] } }));
    keyboardFocus.windowId = windowId;
  }
  events.push(await native("keyboard_event", { windowId, event: { type: "key", key, state: 1 } }));
  if (holdMs > 0) await new Promise((resolve) => setTimeout(resolve, holdMs));
  events.push(await native("keyboard_event", { windowId, event: { type: "key", key, state: 0 } }));
  return { delivered: true, key, holdMs, events };
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

async function waitForCommit({ windowId, afterCommitSerial, timeoutMs = 5000 }) {
  return native("capture_next_frame", { windowId, afterCommitSerial, timeoutMs });
}

function resetInputState() {
  pointerFocus.windowId = null;
  keyboardFocus.windowId = null;
  return { pointerFocusReset: true, keyboardFocusReset: true };
}

const apiHelp = `Persistent JavaScript console. State survives calls.
Define helpers with globalThis, then call them in later or the same evaluation:
  globalThis.pointerWindowId = null;
  globalThis.click = async function(windowId, x, y) {
    const button = 0x110; // BTN_LEFT
    if (pointerWindowId !== windowId) {
      await wayland.pointerEvent({windowId, event:{type:"enter", x, y}});
      pointerWindowId = windowId;
    }
    await wayland.pointerEvent({windowId, event:{type:"motion", x, y}});
    await wayland.pointerEvent({windowId, event:{type:"frame"}});
    await wayland.pointerEvent({windowId, event:{type:"button", button, state:1}});
    await wayland.pointerEvent({windowId, event:{type:"frame"}});
    await wayland.pointerEvent({windowId, event:{type:"button", button, state:0}});
    await wayland.pointerEvent({windowId, event:{type:"frame"}});
  };
  return await click("window-id", 100, 100);
Routine helpers: wayland.waitForWindow(selector), wayland.click(args),
wayland.doubleClick(args), wayland.move(args),
wayland.drag({windowId,from,to,durationMs,steps}),
wayland.scroll({windowId,x,y,deltaY}), wayland.pressKey({windowId,key,holdMs}),
wayland.waitForCommit(args), wayland.resetInputState(). Reset helper focus state
after emitting raw enter/leave events or replacing a client. Coordinates default to
full screenshot pixels; pass coordinateSpace:"preview" and the returned
previewToFullScale object (or the response's preview_to_full_scale object) to
convert preview coordinates safely.
Raw calls: environment(), diagnostics(), windows(), screenshot({windowId}),
captureNextFrame({windowId, afterCommitSerial, timeoutMs}),
pointerEvent({windowId,event}), keyboardEvent({windowId,event}), sleep(ms).
Each input call emits exactly one compositor-side protocol event; author
sequences yourself. wl_pointer event types and fields:
  enter{x,y,serial?}, motion{x,y,time?}, button{button,state,serial?,time?},
  axis{axis,value,time?}, axis_source{axis_source}, axis_stop{axis,time?},
  axis_discrete{axis,discrete}, axis_value120{axis,value120},
  axis_relative_direction{axis,direction}, frame{}.
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

const wayland = Object.freeze({
  help: apiHelp,
  environment: () => native("environment"),
  diagnostics: () => native("diagnostics"),
  windows: () => native("windows"),
  screenshot: (args = {}) => native("screenshot", args),
  captureNextFrame: (args = {}) => native("capture_next_frame", args),
  pointerEvent: (args) => native("pointer_event", args),
  keyboardEvent: (args) => native("keyboard_event", args),
  waitForWindow,
  waitForCommit,
  click,
  doubleClick,
  move,
  drag,
  scroll,
  pressKey,
  resetInputState,
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
});

const context = vm.createContext({
  wayland,
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
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
  if (message.type === "native_result") {
    const pending = pendingNative.get(message.id);
    if (!pending) return;
    pendingNative.delete(message.id);
    if (message.ok) pending.resolve(message.value);
    else pending.reject(new Error(message.error));
  }
});
