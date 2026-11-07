// Web platform globals on V8's own conversions: UTF-8 text coding, base64 strings,
// structured cloning, URLs and timers.
import { define as defineUrl, needs } from "skyhook:url";

// Bytes per String.fromCharCode call, within any engine's argument limit.
const CHUNK = 0x8000;
const LABELS = new Set(["unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8", "utf-8", "utf8", "x-unicode20utf8"]);

// There is no DOMException; its names mark these errors as they would.
function named(name, message) {
  const error = new Error(message);
  Object.defineProperty(error, "name", {value: name, writable: true, configurable: true});
  return error;
}

function bytesOf(input) {
  if (input === undefined) return new Uint8Array(0);
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  if (input instanceof ArrayBuffer || input instanceof SharedArrayBuffer) return new Uint8Array(input);
  throw new TypeError("TextDecoder.decode requires an ArrayBuffer or a view of one");
}

// Marks a web API object, whose state a structured clone cannot copy, so cloning
// it throws rather than making an empty object.
export let brand;

// Installed once, into the snapshot, with the host's UTF-8 and cloning functions.
export function define(utf8Length, encodeInto, decode, clone, uncloneable, ...url) {
  brand = uncloneable;
  defineUrl(brand, ...url);
  class TextEncoder {
    constructor() { brand(this); }
    get encoding() { return "utf-8"; }
    encode(input = "") {
      const text = `${input}`;
      const bytes = new Uint8Array(utf8Length(text));
      encodeInto(text, bytes);
      return bytes;
    }
    encodeInto(source, destination) {
      if (!(destination instanceof Uint8Array)) throw new TypeError("TextEncoder.encodeInto requires a Uint8Array");
      const [read, written] = encodeInto(`${source}`, destination);
      return {read, written};
    }
  }
  class TextDecoder {
    #fatal;
    #ignoreBOM;
    constructor(label = "utf-8", options = {}) {
      brand(this);
      if (!LABELS.has(`${label}`.replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, "").toLowerCase())) {
        throw new RangeError(`TextDecoder supports only UTF-8, not ${label}`);
      }
      this.#fatal = Boolean(options?.fatal);
      this.#ignoreBOM = Boolean(options?.ignoreBOM);
    }
    get encoding() { return "utf-8"; }
    get fatal() { return this.#fatal; }
    get ignoreBOM() { return this.#ignoreBOM; }
    decode(input, options) {
      if (options?.stream) throw new TypeError("TextDecoder does not support streaming");
      let bytes = bytesOf(input);
      if (!this.#ignoreBOM && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) bytes = bytes.subarray(3);
      return decode(bytes, this.#fatal);
    }
  }
  // Forgiving base64, as browsers decode it.
  function atob(data) {
    needs(arguments.length, 1, "atob");
    let text = String(data).replace(/[\t\n\f\r ]/g, "");
    if (text.length % 4 === 0) text = text.replace(/==?$/, "");
    if (text.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(text)) {
      throw named("InvalidCharacterError", "atob() requires a base64 string");
    }
    const bytes = Uint8Array.fromBase64(text, {lastChunkHandling: "loose"});
    let decoded = "";
    for (let start = 0; start < bytes.length; start += CHUNK) {
      decoded += String.fromCharCode(...bytes.subarray(start, start + CHUNK));
    }
    return decoded;
  }
  function btoa(data) {
    needs(arguments.length, 1, "btoa");
    const text = String(data);
    const bytes = new Uint8Array(text.length);
    for (let index = 0; index < text.length; index++) {
      const code = text.charCodeAt(index);
      if (code > 0xff) throw named("InvalidCharacterError", "btoa() requires characters up to U+00FF");
      bytes[index] = code;
    }
    return bytes.toBase64();
  }
  const structuredClone = (...args) => {
    if (!args.length) throw new TypeError("structuredClone() requires a value");
    const [value, options] = args;
    if (options?.transfer !== undefined && Array.from(options.transfer).length) {
      throw named("DataCloneError", "structuredClone() does not transfer values");
    }
    return clone(value);
  };
  Object.assign(globalThis, {TextEncoder, TextDecoder, atob, btoa, structuredClone});
}

// Callbacks the runtime schedules itself: their result is never adopted, and what one
// throws, or an async one rejects with, is reported rather than ending the script.
export function scheduling(delay, hostSleep, hostWake, report) {
  const invoke = (callback, args) => {
    try {
      const result = callback(...args);
      if (result instanceof Promise) Promise.prototype.catch.call(result, report);
    } catch (error) {
      report(error);
    }
  };
  const queueMicrotask = callback => {
    if (typeof callback !== "function") throw new TypeError("queueMicrotask(callback) requires a function");
    Promise.resolve().then(() => invoke(callback, []));
  };
  // Timers wait on the host's sleep, so a pending one does not keep the script alive.
  // Clearing one wakes its sleep by number, which ends the timer and lets its callback go.
  const active = new Map();
  let last = 0;
  const schedule = (repeat, callback, ms, args) => {
    if (typeof callback !== "function") throw new TypeError("timer callbacks must be functions");
    const wait = delay(ms), id = ++last, timer = {};
    active.set(id, timer);
    (async () => {
      try {
        do {
          let sleep;
          [sleep, timer.sleep] = hostSleep(wait);
          await sleep;
          if (!active.has(id)) return;
          invoke(callback, args);
        } while (repeat && active.has(id));
      } finally {
        active.delete(id);
      }
    })();
    return id;
  };
  const clear = id => {
    const timer = active.get(id);
    if (!timer) return;
    active.delete(id);
    hostWake(timer.sleep);
  };
  return {
    queueMicrotask,
    setTimeout: (callback, ms, ...args) => schedule(false, callback, ms, args),
    setInterval: (callback, ms, ...args) => schedule(true, callback, ms, args),
    clearTimeout: clear,
    clearInterval: clear,
  };
}
