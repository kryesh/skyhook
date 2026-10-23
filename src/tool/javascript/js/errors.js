// Failure reports are best-effort: no getter or proxy trap on a thrown value may
// replace the original failure. Exception stacks keep only frames of the
// submitted source.
const DEPTH = 32;
const FRAME = /^ +at (?:(.+) \()?([^()\s]+):(\d+):(\d+)\)?$/;
let source;

export function configure(layout) {
  source = layout;
}

function text(value) {
  try { return String(value).toWellFormed(); }
  catch { return "[Unprintable]"; }
}

// The body's wrapper opens on its first line, so only that line's columns are
// offset; QuickJS may place a failure at the body's start, inside the prefix. A
// line past the body is the wrapper's close, where input ended early.
function sourceStack(stack) {
  const frames = [];
  for (const line of stack.split("\n")) {
    const [, name, file, row, column] = FRAME.exec(line) ?? [];
    if (file !== source.filename) continue;
    const at = Number(row) > source.lines
      ? "end of source"
      : `${file}:${row}:${row === "1" ? Math.max(column - source.prefix, 1) : column}`;
    frames.push(name === undefined ? `    at ${at}` : `    at ${name} (${at})`);
  }
  return frames.join("\n");
}

// An error's own enumerable properties, then the standard ones it inherits or
// keeps non-enumerable.
export function errorKeys(error) {
  return new Set([...Object.keys(error), "name", "message", "cause", "errors"]);
}

export function describeError(error, ancestors = []) {
  if (error === null || typeof error === "boolean") return error;
  if (typeof error === "number" && Number.isFinite(error)) return error;
  if (typeof error !== "object") return text(error);
  if (ancestors.includes(error)) return "[circular]";
  if (ancestors.length === DEPTH) return "[nested too deeply]";
  ancestors.push(error);
  try {
    if (Array.isArray(error)) return Array.from(error, item => describeError(item, ancestors));
    const exception = error instanceof Error;
    const description = Object.create(null);
    let keys;
    try { keys = errorKeys(error); } catch { keys = errorKeys({}); }
    keys.add("stack");
    for (const key of keys) {
      try {
        if (!(key in error)) continue;
        const value = error[key];
        description[text(key)] = exception && key === "stack" && typeof value === "string"
          ? sourceStack(value)
          : describeError(value, ancestors);
      } catch {
        description[text(key)] = "[Uninspectable property]";
      }
    }
    return description;
  } catch {
    return "[Uninspectable thrown value]";
  } finally {
    ancestors.pop();
  }
}

// The text of a described error: its name unless plain `Error`, message and stack.
export function errorText(description) {
  if (description === null || typeof description !== "object" || typeof description.message !== "string") return;
  const {name, message, stack} = description;
  const headline = typeof name === "string" && name && name !== "Error" ? `${name}: ${message}` : message;
  return typeof stack === "string" && stack ? `${headline}\n${stack}` : headline;
}

// A thrown value in one line: an error's name and message, or its text.
export function summary(error) {
  const description = errorText(describeError(error));
  return description === undefined ? text(error) : description.split("\n", 1)[0];
}
