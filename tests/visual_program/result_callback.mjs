const RESULT_BYTES = 8 * Uint32Array.BYTES_PER_ELEMENT;
const RESULT_WORDS = 8;

const FLAG_PRESENT = 1 << 0;
const FLAG_ENTERED = 1 << 1;
const FLAG_EXITED = 1 << 2;
const FLAG_MASK_CHANGED = 1 << 3;

function exactBytes(result) {
  if (result instanceof ArrayBuffer) {
    if (result.byteLength !== RESULT_BYTES) {
      throw new RangeError(`visual result must be exactly ${RESULT_BYTES} bytes`);
    }
    return new DataView(result);
  }

  if (ArrayBuffer.isView(result)) {
    if (result.byteLength !== RESULT_BYTES) {
      throw new RangeError(`visual result view must be exactly ${RESULT_BYTES} bytes`);
    }
    return new DataView(result.buffer, result.byteOffset, result.byteLength);
  }

  throw new TypeError("visual result must be an ArrayBuffer or an exact 32-byte view");
}

// Create a generic result callback. It receives only the fixed 32-byte result
// record and emits event summaries; frame pixels and GPU scratch never enter JS.
export function makeOnResult({ emit }) {
  if (typeof emit !== "function") throw new TypeError("emit must be a function");

  return async function onResult(resultBytes) {
    const view = exactBytes(resultBytes);
    const word = index => view.getUint32(index * 4, true);
    const flags = word(0);
    const isPresent = (flags & FLAG_PRESENT) !== 0;
    const bounds = isPresent
      ? { minX: word(1), minY: word(2), maxX: word(3), maxY: word(4) }
      : undefined;

    if (bounds && (bounds.minX > bounds.maxX || bounds.minY > bounds.maxY)) {
      throw new RangeError("present result has inverted bounds");
    }
    if (word(6) !== 0 || word(7) !== 0) {
      throw new RangeError("reserved result words must be zero");
    }

    if (flags & FLAG_ENTERED) await emit({ type: "target-entered", bounds });
    if (flags & FLAG_EXITED) await emit({ type: "target-exited" });
    if (flags & FLAG_MASK_CHANGED) {
      await emit(bounds
        ? { type: "target-mask-changed", bounds }
        : { type: "target-mask-changed" });
    }
  };
}
