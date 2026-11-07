// Failure reports are best-effort: no getter or proxy trap on a thrown value may
// replace the original failure. Exception stacks keep only frames of the
// submitted source.
const DEPTH = 32;
const SOURCE_FRAMES = 10;

// Stacks of the submitted source's own frames, in its own lines and columns. Every
// frame is the body's, so V8's frame text names the line instead of the file.
export function configure(filename) {
  // Runtime frames, such as a nested argument's conversion, must not crowd the body's out.
  Error.stackTraceLimit = 1000;
  Error.prepareStackTrace = (_error, sites) => sites
    .filter(site => site.getFileName() === filename)
    .slice(0, SOURCE_FRAMES)
    .map(site => {
      const frame = String(site), line = site.getLineNumber(), column = site.getColumnNumber();
      const at = frame.lastIndexOf(`${filename}:${line}:${column}`);
      return `    at ${frame.slice(0, at)}line ${line}:${column}${frame.slice(at + `${filename}:${line}:${column}`.length)}`;
    })
    .join("\n");
}

export function text(value) {
  try { return String(value).toWellFormed(); }
  catch { return "[Unprintable]"; }
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
    const description = Object.create(null);
    let keys;
    try { keys = errorKeys(error); } catch { keys = errorKeys({}); }
    keys.add("stack");
    for (const key of keys) {
      try {
        if (!(key in error)) continue;
        const value = error[key];
        description[text(key)] = describeError(value, ancestors);
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
