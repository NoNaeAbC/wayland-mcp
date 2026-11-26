const portalName = "org.freedesktop.portal.Desktop";
const portalPath = "/org/freedesktop/portal/desktop";
const portalRequest = "org.freedesktop.portal.Request";
const portalChooser = "org.freedesktop.portal.FileChooser";
const portalClipboard = "org.freedesktop.portal.Clipboard";
const portalTimeoutMs = 10_000;
const chooserResponseGraceMs = 500;
const clipboardLimit = 64 * 1024 * 1024;

const dbusGuides = Object.freeze({
  "file-chooser": `Mock OpenFile for an app on a private session bus.
  const mock = await wayland.dbus.fileChooser.onChooseFile(bridge.session,
    ({ title, options, signal }) => ({ files: ["/absolute/fixture.png"] }));
Launch the app with bridge.environment. Return null to cancel. The helper
uses observed Response matches and sends a directed response after a short
grace period for clients with a local subscription. The callback signal aborts
on Request.Close, timeout or teardown. Call await mock.unsubscribe() to release the mock.`,
  "portal-clipboard": `Transfer clipboard data through an already-started,
clipboard-enabled XDG RemoteDesktop session on the supplied bus.
  const clip = wayland.dbus.clipboard(bus, sessionPath);
  const offer = await clip.set({ "text/plain": "sample" });
Use await clip.read("text/plain") to read data offered by another owner.
Use clip.onChange(callback) for portal ownership notifications. Call
await offer.unsubscribe() before publishing a replacement with this client.`,
  "at-spi": `Traverse an existing AT-SPI accessibility bus.
  const at = wayland.dbus.atspi(bus);
  const node = await at.find({ name: "Apply", role: "button" });
  if (node) await at.invoke(node, "press");
Use roots(), children(node) and inspect(node) for discovery.
Use focus(node), readText(node) and setText(node, text) on supported nodes.`,
});

function dbusProtocolHelp(protocol) {
  const topics = Object.keys(dbusGuides);
  if (protocol === undefined) return topics;
  if (!Object.hasOwn(dbusGuides, protocol)) {
    throw new RangeError(`Unknown D-Bus guide. Available topics: ${topics.join(", ")}`);
  }
  return dbusGuides[protocol];
}

async function dbusHelperCall(bus, message) {
  const reply = await bus.call(message);
  if (reply.type === "error") {
    const error = new Error(`${reply.errorName}: ${reply.body[0] ?? ""}`);
    error.dbusName = reply.errorName;
    throw error;
  }
  return reply.body[0];
}

async function dbusWithin(work, timeoutMs, message) {
  let timer;
  try {
    return await Promise.race([
      work(),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), timeoutMs); }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

// The fields that can match Response contain bus names, paths and identifiers.
// Argument filters cannot match its u/a{sv} body.
function portalResponseMatch(rule) {
  if (typeof rule !== "string") return;
  const entry = /\s*([a-z_]+)\s*=\s*(?:'([^']*)'|([^,'\s]*))\s*(,|$)/gy;
  const entries = [];
  let offset = 0;
  while (offset < rule.length) {
    const field = entry.exec(rule);
    if (!field) return;
    const [, name, quoted, plain] = field;
    if (!["type", "sender", "interface", "member", "path", "path_namespace", "destination", "eavesdrop"].includes(name)) return;
    entries.push([name, quoted ?? plain]);
    offset = entry.lastIndex;
  }
  const fields = Object.fromEntries(entries);
  if ((fields.type && fields.type !== "signal")
      || (fields.interface && fields.interface !== portalRequest)
      || (fields.member && fields.member !== "Response")) return;
  const namespace = fields.path_namespace;
  return {
    key: JSON.stringify(entries.sort(([a], [b]) => a.localeCompare(b))),
    accepts(sender, destination, path) {
      return (!fields.sender || fields.sender === portalName || fields.sender === sender)
        && (!fields.destination || fields.destination === destination)
        && (!fields.path || fields.path === path)
        && (!namespace || namespace === "/" || path === namespace || path.startsWith(`${namespace}/`));
    },
  };
}

async function onChooseFile(bus, choose) {
  if (typeof choose !== "function") throw new TypeError("onChooseFile requires a callback");
  const requests = new Map();
  const owners = new Map();
  const matchCalls = new Map();
  const matches = new Map();
  const remember = (entries, key, value) => {
    entries.set(key, value);
    if (entries.size > 256) entries.delete(entries.keys().next().value);
  };
  let matchChanged = Promise.withResolvers();
  let nextToken = 0;
  let service;
  let closing;

  const recordMatch = (owner, { rule, delta }) => {
    const key = JSON.stringify([owner, rule.key]);
    const count = Math.max(0, (matches.get(key)?.count ?? 0) + delta);
    // Retain removals so a client known to have unsubscribed still waits
    // for a renewed match, even after the directed-response grace period.
    remember(matches, key, { owner, rule, count });
    matchChanged.resolve();
    matchChanged = Promise.withResolvers();
  };
  const observer = await bus.observe({}, message => {
    const { connection, direction, destination } = message;
    if (direction === "fromApp" && message.type === "methodCall" &&
        destination === "org.freedesktop.DBus" && message.interface === "org.freedesktop.DBus" &&
        message.path === "/org/freedesktop/DBus" && ["AddMatch", "RemoveMatch"].includes(message.member)) {
      const rule = portalResponseMatch(message.body[0]);
      if (!rule) return;
      const change = { connection, rule, delta: message.member === "AddMatch" ? 1 : -1 };
      if (message.noReplyExpected) {
        // libdbus can send AddMatch and RemoveMatch without requesting a
        // broker reply. The client's local signal handler is already set.
        const owner = owners.get(connection);
        if (owner) recordMatch(owner, change);
      } else {
        remember(matchCalls, `${connection}:${message.serial}`, change);
      }
      return;
    }
    if (direction !== "toApp") return;
    // The Hello reply identifies the client before it sends later requests.
    // An OpenFile reply also identifies clients that connected before the mock.
    if (!owners.has(connection) && destination?.startsWith(":")) {
      remember(owners, connection, destination);
    }
    if (!message.replySerial) return;
    const callKey = `${connection}:${message.replySerial}`;
    const change = matchCalls.get(callKey);
    if (!change) return;
    matchCalls.delete(callKey);
    if (message.type === "methodReturn" && destination) recordMatch(destination, change);
  });

  const error = (kind, errorMessage) => ({ errorName: `org.freedesktop.DBus.Error.${kind}`, errorMessage });
  const stop = async () => {
    try {
      await observer.unsubscribe();
    } finally {
      const active = [...requests.values()];
      for (const request of active) request.cancel("File chooser mock stopped");
      try {
        await Promise.allSettled(active.map(request => request.dispose()));
      } finally {
        await service?.close();
      }
    }
  };

  try {
    service = await bus.serve({ name: portalName });
    await service.exportObject({
      path: portalPath, interface: "org.freedesktop.DBus.Properties",
      methods: { Get: { in: "ss", out: "v" }, GetAll: { in: "s", out: "a{sv}" } },
    }, {
      Get({ body: [iface, property] }) {
        if (iface !== portalChooser) return error("UnknownInterface", iface);
        if (property !== "version") return error("UnknownProperty", property);
        return { body: [{ signature: "u", value: 1 }] };
      },
      GetAll({ body: [iface] }) {
        if (iface !== portalChooser) return error("UnknownInterface", iface);
        return { body: [{ version: { signature: "u", value: 1 } }] };
      },
    });
    await service.exportObject({
      path: portalPath, interface: portalChooser, methods: { OpenFile: { in: "ssa{sv}", out: "o" } },
    }, {
      async OpenFile(call) {
        if (closing) return error("Failed", "File chooser mock stopped");
        const [parent, title, options] = call.body;
        const token = options.handle_token ? options.handle_token.value : `wayland_mcp_${++nextToken}`;
        if (typeof token !== "string" || !/^[A-Za-z0-9_]+$/.test(token)) {
          return error("InvalidArgs", "File chooser handle token must be an object path element");
        }
        const sender = call.sender.slice(1).replaceAll(".", "_");
        const path = `${portalPath}/request/${sender}/${token}`;
        if (requests.has(path)) return error("InvalidArgs", "File chooser handle token is already in use");
        const controller = new AbortController();
        const stopped = Promise.withResolvers();
        let timer;
        let disposal;
        const cancel = reason => {
          controller.abort(reason);
          stopped.resolve();
        };
        const dispose = () => {
          disposal ??= (async () => {
            clearTimeout(timer);
            try {
              const request = await exporting;
              await request.unexport();
            } finally {
              requests.delete(path);
            }
          })();
          return disposal;
        };
        const exporting = service.exportObject({
          path, interface: portalRequest, methods: { Close: { in: "", out: "" } },
        }, {
          Close(close) {
            if (close.sender !== call.sender) return error("AccessDenied", "Request belongs to another client");
            cancel("File chooser request closed");
          },
        });
        requests.set(path, { cancel, dispose });
        void stopped.promise.then(dispose).catch(error => console.error("File chooser cleanup failed", error));
        try {
          await exporting;
        } catch (failure) {
          requests.delete(path);
          throw failure;
        }
        if (controller.signal.aborted) return { body: [path] };
        timer = setTimeout(() => cancel("File chooser request timed out"), portalTimeoutMs);
        call.afterReply(async () => {
          let graceElapsed = false;
          let graceTimer;
          const grace = new Promise(resolve => {
            graceTimer = setTimeout(() => {
              graceElapsed = true;
              resolve();
            }, chooserResponseGraceMs);
          });
          try {
            if (controller.signal.aborted) return;
            let response;
            try {
              const choice = await Promise.race([
                choose({ parent, title, options, filters: options.filters?.value,
                  multiple: options.multiple?.value ?? false, signal: controller.signal }),
                stopped.promise,
              ]);
              if (controller.signal.aborted) return;
              if (choice === null) {
                response = [1, { uris: { signature: "as", value: [] } }];
              } else {
                if (!Array.isArray(choice?.files) || !choice.files.length) {
                  throw new TypeError("File chooser requires a nonempty files array");
                }
                const uris = choice.files.map(file => {
                  if (typeof file !== "string" || !file.startsWith("/") || file.includes("\0")) {
                    throw new TypeError("Selected files require absolute paths");
                  }
                  return `file://${file.split("/").map(encodeURIComponent).join("/")}`;
                });
                response = [0, { uris: { signature: "as", value: uris } }];
              }
            } catch {
              response = [2, {}];
            }
            const accepts = rule => rule.accepts(service.uniqueName, call.sender, path);
            while (!controller.signal.aborted) {
              const relevant = [...matches.values()].filter(match => match.owner === call.sender && accepts(match.rule));
              const pendingMatch = [...matchCalls.values()].some(change =>
                owners.get(change.connection) === call.sender && accepts(change.rule));
              // GTK subscribes locally before OpenFile. Its directed signal
              // can arrive without a bus match. Allow late registrants a
              // short grace period before using that path.
              if (relevant.some(match => match.count > 0) || (graceElapsed && !relevant.length && !pendingMatch)) {
                await service.emitSignal({ destination: call.sender, path, interface: portalRequest,
                  member: "Response", signature: "ua{sv}", body: response });
                return;
              }
              const wake = [matchChanged.promise, stopped.promise];
              if (!graceElapsed) wake.push(grace);
              await Promise.race(wake);
            }
          } finally {
            clearTimeout(graceTimer);
            await dispose();
          }
        });
        return { body: [path] };
      },
    });
  } catch (failure) {
    await stop();
    throw failure;
  }
  return Object.freeze({ unsubscribe() { return closing ??= stop(); } });
}

const clipboardOffers = new WeakMap();

async function withClipboardDescriptor(descriptor, operation) {
  if (typeof descriptor?.close !== "function") throw new Error("Clipboard portal must return a descriptor");
  try {
    return await dbusWithin(() => operation(descriptor), portalTimeoutMs, "Clipboard descriptor transfer timed out");
  } finally {
    await descriptor.close();
  }
}

function clipboard(bus, session) {
  if (typeof session !== "string" || !/^\/(?:[a-zA-Z0-9_]+(?:\/[a-zA-Z0-9_]+)*)?$/.test(session)) {
    throw new TypeError("Clipboard portal requires a session object path");
  }
  let offers = clipboardOffers.get(bus);
  if (!offers) clipboardOffers.set(bus, offers = new Set());
  const call = (member, signature, body) => dbusHelperCall(bus, {
    destination: portalName, path: portalPath, interface: portalClipboard, member, signature, body,
  });
  // The first argument is an object path. Filter it in JS after the bus matches the signal headers.
  const on = (member, callback) => bus.subscribe(
    `type='signal',sender='${portalName}',path='${portalPath}',interface='${portalClipboard}',member='${member}'`,
    message => message.body[0] === session ? callback(message.body.slice(1)) : undefined,
  );
  return Object.freeze({
    onChange(callback) {
      if (typeof callback !== "function") throw new TypeError("onChange requires a callback");
      return on("SelectionOwnerChanged", ([options]) => callback(options));
    },
    async read(mimeType) {
      if (typeof mimeType !== "string" || !mimeType || mimeType.includes("\0")) {
        throw new TypeError("Clipboard requires a MIME type");
      }
      return withClipboardDescriptor(await call("SelectionRead", "os", [session, mimeType]), async descriptor => {
        const chunks = [];
        let size = 0;
        while (true) {
          const chunk = await descriptor.read();
          if (!chunk.length) break;
          size += chunk.length;
          if (size > clipboardLimit) throw new RangeError("Clipboard data exceeds 64 MiB");
          chunks.push(chunk);
        }
        const bytes = new DBusBytes(size);
        let offset = 0;
        for (const chunk of chunks) {
          bytes.set(chunk, offset);
          offset += chunk.length;
        }
        return bytes;
      });
    },
    async set(formats) {
      if (Object.prototype.toString.call(formats) !== "[object Object]") {
        throw new TypeError("Clipboard formats require a MIME type to data map");
      }
      if (offers.has(session)) throw new Error("Unsubscribe the active clipboard offer before replacing it");
      const payloads = new Map();
      let size = 0;
      for (const [mime, value] of Object.entries(formats)) {
        if (!mime || mime.includes("\0")) throw new TypeError("Clipboard requires a MIME type");
        if (typeof value === "string") {
          size += Buffer.byteLength(value);
        } else if (Object.prototype.toString.call(value) === "[object Uint8Array]") {
          size += value.byteLength;
        } else {
          throw new TypeError("Clipboard data requires a string or Uint8Array");
        }
        if (size > clipboardLimit) throw new RangeError("Clipboard data exceeds 64 MiB");
        payloads.set(mime, typeof value === "string" ? Buffer.from(value) : Uint8Array.from(value));
      }
      offers.add(session);
      let subscription;
      let closing;
      const transfers = new Map();
      const stop = async () => {
        try {
          await subscription?.unsubscribe();
        } finally {
          await Promise.allSettled(transfers.values());
          offers.delete(session);
        }
      };
      try {
        subscription = await on("SelectionTransfer", async ([mimeType, serial]) => {
          if (transfers.has(serial)) return;
          const transfer = (async () => {
            let success = false;
            try {
              const bytes = payloads.get(mimeType);
              if (!closing && bytes) {
                await withClipboardDescriptor(await call("SelectionWrite", "ou", [session, serial]),
                  descriptor => descriptor.write(bytes));
                success = true;
              }
            } finally {
              await call("SelectionWriteDone", "oub", [session, serial, success]);
            }
          })();
          transfers.set(serial, transfer);
          try { await transfer; } finally { transfers.delete(serial); }
        });
        await call("SetSelection", "oa{sv}", [session, { mime_types: { signature: "as", value: [...payloads.keys()] } }]);
      } catch (error) {
        closing = stop();
        await closing;
        throw error;
      }
      return Object.freeze({ unsubscribe() { return closing ??= stop(); } });
    },
  });
}

function atspi(bus) {
  const prefix = "org.a11y.atspi";
  const root = { destination: `${prefix}.Registry`, path: "/org/a11y/atspi/accessible/root" };
  const call = ({ destination, path }, interfaceName, member, signature = "", body = []) => dbusHelperCall(bus, {
    destination, path, interface: interfaceName, member, signature, body,
  });
  const accessible = (node, member) => call(node, `${prefix}.Accessible`, member);
  const property = async (node, interfaceName, name) => (await call(node,
    "org.freedesktop.DBus.Properties", "Get", "ss", [interfaceName, name])).value;
  const children = async node => (await accessible(node, "GetChildren"))
    .filter(([destination, path]) => destination && path !== "/org/a11y/atspi/null")
    .map(([destination, path]) => ({ destination, path }));
  const roots = () => children(root);
  const inspect = async node => {
    const [name, role] = await Promise.all([
      property(node, `${prefix}.Accessible`, "Name"), accessible(node, "GetRoleName"),
    ]);
    return { ...node, name, role };
  };

  return Object.freeze({ roots, children, inspect,
    async find({ within, name, role, timeoutMs = 7000 } = {}) {
      if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 120_000) {
        throw new RangeError("AT-SPI search timeout must be between 1 and 120000 ms");
      }
      const deadline = performance.now() + timeoutMs;
      const timeout = "AT-SPI search timed out";
      return dbusWithin(async () => {
        const queue = [];
        const visited = new Set();
        const enqueue = nodes => {
          for (const node of nodes) {
            const key = `${node.destination}:${node.path}`;
            if (visited.has(key)) continue;
            if (visited.size >= 4096) throw new RangeError("AT-SPI tree exceeds the search limit");
            visited.add(key);
            queue.push(node);
          }
        };
        enqueue(within ? [within] : await roots());
        for (const node of queue) {
          if (performance.now() >= deadline) throw new Error(timeout);
          try {
            const candidate = await inspect(node);
            if (performance.now() >= deadline) throw new Error(timeout);
            if ((name === undefined || name === candidate.name) && (role === undefined || role === candidate.role)) {
              return candidate;
            }
            enqueue(await children(node));
          } catch (error) {
            if (!["UnknownObject", "NameHasNoOwner", "ServiceUnknown"]
              .some(name => error.dbusName === `org.freedesktop.DBus.Error.${name}`)) throw error;
          }
        }
        return null;
      }, timeoutMs, timeout);
    },
    async invoke(node, action) {
      const target = `${prefix}.Action`;
      const count = await property(node, target, "NActions");
      for (let index = 0; index < count; index++) {
        if (await call(node, target, "GetName", "i", [index]) === action) {
          return call(node, target, "DoAction", "i", [index]);
        }
      }
      throw new Error(`AT-SPI node has no ${action} action`);
    },
    focus: node => call(node, `${prefix}.Component`, "GrabFocus"),
    readText: (node, start = 0, end = -1) => call(node, `${prefix}.Text`, "GetText", "ii", [start, end]),
    async setText(node, text) {
      const editable = `${prefix}.EditableText`;
      if (!(await accessible(node, "GetInterfaces")).includes(editable)) {
        throw new Error("AT-SPI node does not expose EditableText");
      }
      return call(node, editable, "SetTextContents", "s", [text]);
    },
  });
}
