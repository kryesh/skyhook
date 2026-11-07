import { errorKeys, summary } from "skyhook:errors";

export const parse = JSON.parse;
export const stringify = JSON.stringify;
export const IDENTIFIER = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

// Counted from the converted root, leaving the stored result's and the host
// envelopes' nesting inside the host parser's limit.
const DEPTH = 100;
const REPORTED = 10;
const presentations = new WeakMap();
const memberPresentations = new WeakMap();

// Responses arrive with their declared presentations, which follow the
// containers and members they name into returned values.
export function annotate(value, presented) {
  for (const [pointer, presentation] of Object.entries(presented)) {
    let parent, key, child = value;
    for (key of pointer.split("/").slice(1).map(key => key.replace(/~1/g, "/").replace(/~0/g, "~"))) {
      parent = child;
      child = child[key];
    }
    if (child !== null && typeof child === "object") presentations.set(child, presentation);
    if (parent) {
      let members = memberPresentations.get(parent);
      if (!members) memberPresentations.set(parent, members = new Map());
      members.set(key, presentation);
    }
  }
}

function kind(value) {
  if (value === undefined) return "undefined";
  if (typeof value !== "object") return `a ${typeof value}`;
  let name;
  try { name = value.constructor?.name; } catch { /* Report the value without its type. */ }
  return typeof name === "string" && name ? `${/^[aeiou]/i.test(name) ? "an" : "a"} ${name}` : "an unreadable object";
}

const PLAIN = Object.freeze({});
const MEMBER = Object.freeze({enumerable: true, writable: true, configurable: true});

// Objects whose contents cannot be read.
const OPAQUE = [WeakMap, WeakSet, WeakRef, FinalizationRegistry];

const bytes = (buffer, offset = 0, length = buffer.byteLength) => Array.from(new Uint8Array(buffer, offset, length));

// The JSON form of a built-in that has none of its own: a primitive converted in
// its place, the elements of an array, or the members of an object.
function natural(value) {
  if ([Number, String, Boolean, BigInt].some(type => value instanceof type)) return {primitive: value.valueOf()};
  if (value instanceof RegExp) return {primitive: String(value)};
  if (value instanceof DataView) return {list: bytes(value.buffer, value.byteOffset, value.byteLength)};
  if (ArrayBuffer.isView(value)) return {list: Array.from(value)};
  if (value instanceof ArrayBuffer || value instanceof SharedArrayBuffer) return {list: bytes(value)};
  if (value instanceof Set) return {list: [...value]};
  if (value instanceof Map) {
    const entries = [...value];
    const names = entries.map(([key]) => typeof key === "string" || (typeof key === "number" && Number.isFinite(key)) ? String(key) : undefined);
    // Keys an object can hold, still distinct as its names.
    if (names.every(name => name !== undefined) && new Set(names).size === names.length) {
      return {object: Object.fromEntries(entries.map(([, item], index) => [names[index], item]))};
    }
    return {list: entries};
  }
  return {};
}

function member(path, key, list) {
  if (list) return `${path}[${key}]`;
  return IDENTIFIER.test(key) ? `${path}.${key}` : `${path}[${stringify(key)}]`;
}

// Arguments and returns convert alike, as JSON.stringify does, toJSON included,
// but with a natural form wherever it would throw or lose data: BigInts become
// decimal strings, typed arrays, buffers and Sets arrays, Maps objects or entries,
// boxed primitives their values and an Error its properties. A return awaits
// thenables, tool builders included, and replaces unpaired surrogates that an
// argument rejects. An argument fails at its first problem, an undefined array
// element included; a return records each, as null, and keeps converting, a
// member that throws or rejects included.
// Each thenable continues as soon as it settles, so independent branches
// overlap; members are assigned only once all of a container's branches settle,
// so completion order never reorders them.
function convert(root, subject, returning, presented, pointer = "") {
  const problems = [];
  const fail = (problem, path, error) => {
    const reported = `${problem} at ${path}${error === undefined ? "" : `: ${summary(error)}`}`.toWellFormed();
    if (!returning) throw new TypeError(`${subject} has ${reported}`);
    problems.push(reported);
    return null;
  };
  // A value is the member `key` of the container `frame` converts, or the root. A
  // frame keeps its container and its depth;
  // paths and pointers are spelled only when a problem or presentation needs one.
  const pathOf = (frame, key) => frame ? member(pathOf(frame.up, frame.key), key, frame.list) : "$";
  const pointerOf = (frame, key) =>
    frame ? `${pointerOf(frame.up, frame.key)}/${`${key}`.replace(/~/g, "~0").replace(/\//g, "~1")}` : pointer;
  function visit(value, frame, key = "", json = true) {
    try {
      return inspect(value, frame, key, json);
    } catch (error) {
      if (!returning) throw error;
      return fail("an uninspectable value", pathOf(frame, key), error);
    }
  }
  // Undefined stays undefined: an absence its container omits or, in an array, fills.
  function inspect(value, frame, key, json) {
    if (value === undefined || value === null || typeof value === "boolean") return value;
    if (typeof value === "string") {
      if (returning) return value.toWellFormed();
      return value.isWellFormed() ? value : fail("an unpaired surrogate", pathOf(frame, key));
    }
    if (typeof value === "number") return Number.isFinite(value) ? value : null;
    if (typeof value === "bigint") return String(value);
    if ((typeof value === "object" || typeof value === "function") && typeof value.then === "function") {
      if (!returning) return fail("an unawaited promise or tool call", pathOf(frame, key));
      return Promise.resolve(value).then(
        value => visit(value, frame, key, json),
        error => fail("a rejected promise", pathOf(frame, key), error),
      );
    }
    if (typeof value !== "object") return fail(kind(value), pathOf(frame, key));
    const array = Array.isArray(value);
    const prototype = array ? Array.prototype : Object.getPrototypeOf(value);
    // Plain objects and arrays are most of any value: no built-in rule applies to them.
    const plain = array || prototype === Object.prototype || prototype === null;
    if (!plain && OPAQUE.some(type => value instanceof type)) return fail(kind(value), pathOf(frame, key));
    // As in JSON.stringify, toJSON goes first and cycles are those of what it gives; one
    // that never ends meets the depth limit.
    if (json && typeof value.toJSON === "function") {
      let converted;
      try {
        converted = value.toJSON(`${key}`);
      } catch (error) {
        return fail("a throwing toJSON", pathOf(frame, key), error);
      }
      return visit(converted, frame, key, false);
    }
    for (let up = frame; up; up = up.up) {
      if (up.value === value) return fail("a cycle", pathOf(frame, key));
    }
    const {primitive, list, object} = plain ? PLAIN : natural(value);
    if (primitive !== undefined) return visit(primitive, frame, key);
    const depth = frame ? frame.depth + 1 : 0;
    // The limit's own path would spell every level; its start locates it.
    if (depth === DEPTH) return fail(`nesting deeper than ${DEPTH}`, `${pathOf(frame, key).slice(0, 48)}…`);
    const source = list ?? object ?? value;
    const inside = {up: frame, key, value, depth, list: array || list !== undefined};
    if (presented && presentations.has(value)) presented[pointerOf(frame, key)] = presentations.get(value);
    const members = presented && memberPresentations.get(value);
    const place = (converted, key) => {
      if (converted === undefined) {
        if (!inside.list) return undefined;
        return returning ? null : fail("undefined", pathOf(inside, key));
      }
      if (members?.has(`${key}`)) presented[pointerOf(inside, key)] = members.get(`${key}`);
      return converted;
    };
    const child = key => {
      let item;
      try {
        item = source[key];
      } catch (error) {
        if (!returning) throw error;
        return fail("a throwing property", pathOf(inside, key), error);
      }
      const converted = visit(item, inside, key);
      return converted instanceof Promise ? converted.then(converted => place(converted, key)) : place(converted, key);
    };
    let pending = false;
    if (inside.list) {
      // Read once: a getter may change it while the elements are read.
      const length = source.length;
      const children = new Array(length);
      for (let index = 0; index < length; index++) {
        pending = (children[index] = child(index)) instanceof Promise || pending;
      }
      return pending ? Promise.all(children) : children;
    }
    const keys = [], children = [];
    const error = !plain && value instanceof Error;
    for (const name of error ? errorKeys(value) : Object.keys(source)) {
      if (error && !(name in value)) continue;
      if (!name.isWellFormed()) {
        fail("an unpaired surrogate in a key", pathOf(frame, key));
        continue;
      }
      keys.push(name);
      pending = (children[children.length] = child(name)) instanceof Promise || pending;
    }
    const assemble = values => {
      const output = {};
      for (let index = 0; index < keys.length; index++) {
        if (values[index] === undefined) continue;
        // Assigning `__proto__` would set the prototype rather than a member.
        if (keys[index] === "__proto__") Object.defineProperty(output, "__proto__", {value: values[index], ...MEMBER});
        else output[keys[index]] = values[index];
      }
      return output;
    };
    return pending ? Promise.all(children).then(assemble) : assemble(children);
  }
  const value = visit(root, null);
  if (!returning) return value ?? null;
  return Promise.resolve(value).then((value = null) => {
    if (!problems.length) return {value};
    const more = problems.length > REPORTED ? `; and ${problems.length - REPORTED} more` : "";
    return {value, problem: `${subject} has ${problems.slice(0, REPORTED).join("; ")}${more}`};
  });
}

export function argument(value, subject) {
  return convert(value, subject, false);
}

// Resolves to the returned JSON, with each unconvertible member null and
// reported, recording presentations where the script's own result places it.
export function returned(value, presented) {
  return convert(value, "the returned value", true, presented, "/result/value");
}
