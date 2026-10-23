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
  if (typeof value === "number") return "a non-finite number";
  if (typeof value === "bigint") return "a BigInt";
  if (typeof value !== "object") return `a ${typeof value}`;
  let name;
  try { name = value.constructor?.name; } catch { /* Report the value without its type. */ }
  return typeof name === "string" && name ? `${/^[aeiou]/i.test(name) ? "an" : "a"} ${name}` : "a non-plain object";
}

function* indices(length) {
  for (let index = 0; index < length; index++) yield String(index);
}

function member(path, key, array) {
  if (array) return `${path}[${key}]`;
  return IDENTIFIER.test(key) ? `${path}.${key}` : `${path}[${stringify(key)}]`;
}

// Arguments and returns convert alike, except that a return awaits thenables,
// tool builders included, and converts an Error as its properties; it replaces
// unpaired surrogates that an argument rejects, and keeps undefined members as
// problems where an argument omits them. An argument fails at its first
// problem; a return records each, as null, and keeps converting, a member that
// throws or rejects included.
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
  function visit(value, path, pointer, ancestors) {
    try {
      return inspect(value, path, pointer, ancestors);
    } catch (error) {
      if (!returning) throw error;
      return fail("an uninspectable value", path, error);
    }
  }
  function inspect(value, path, pointer, ancestors) {
    if (value === undefined && path === "$") return null;
    if (value === null || typeof value === "boolean") return value;
    if (typeof value === "string") {
      if (returning) return value.toWellFormed();
      return value.isWellFormed() ? value : fail("an unpaired surrogate", path);
    }
    if (typeof value === "number" && Number.isFinite(value)) return value;
    if ((typeof value === "object" || typeof value === "function") && typeof value.then === "function") {
      if (!returning) return fail("an unawaited promise or tool call", path);
      return Promise.resolve(value).then(
        value => visit(value, path, pointer, ancestors),
        error => fail("a rejected promise", path, error),
      );
    }
    if (typeof value !== "object") return fail(kind(value), path);
    const array = Array.isArray(value);
    const error = returning && value instanceof Error;
    const prototype = Object.getPrototypeOf(value);
    if (!array && !error && prototype !== Object.prototype && prototype !== null) return fail(kind(value), path);
    if (ancestors.includes(value)) return fail("a cycle", path);
    // The limit's own path would spell every level; its start locates it.
    if (ancestors.length === DEPTH) return fail(`nesting deeper than ${DEPTH}`, `${path.slice(0, 48)}…`);
    ancestors = [...ancestors, value];
    if (presented && presentations.has(value)) presented[pointer] = presentations.get(value);
    const members = presented && memberPresentations.get(value);
    const keys = [], children = [];
    for (const key of array ? indices(value.length) : error ? errorKeys(value) : Object.keys(value)) {
      if (error && !(key in value)) continue;
      if (!key.isWellFormed()) {
        fail("an unpaired surrogate in a key", path);
        continue;
      }
      const childPath = member(path, key, array);
      let child;
      try {
        child = value[key];
      } catch (error) {
        if (!returning) throw error;
        keys.push(key);
        children.push(fail("a throwing property", childPath, error));
        continue;
      }
      if (child === undefined && !returning && !array) continue;
      const childPointer = `${pointer}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`;
      if (members?.has(key)) presented[childPointer] = members.get(key);
      keys.push(key);
      children.push(visit(child, childPath, childPointer, ancestors));
    }
    const assemble = values => {
      if (array) return values;
      const output = Object.create(null);
      keys.forEach((key, index) => { output[key] = values[index]; });
      return output;
    };
    return children.some(child => child instanceof Promise) ? Promise.all(children).then(assemble) : assemble(children);
  }
  const value = visit(root, "$", pointer, []);
  if (!returning) return value;
  return Promise.resolve(value).then(value => {
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
