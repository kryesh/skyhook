import { stringify } from "skyhook:values";
import { describeError, errorText } from "skyhook:errors";

function format(value) {
  if (typeof value === "string") return value.toWellFormed();
  const ancestors = [];
  try {
    if (value instanceof Error) return errorText(describeError(value));
    return (stringify(value, function (_key, item) {
      if (typeof item === "bigint") return `${item}n`;
      if (typeof item === "object" && item !== null) {
        while (ancestors.length && ancestors[ancestors.length - 1] !== this) ancestors.pop();
        if (ancestors.includes(item)) return "[Circular]";
        ancestors.push(item);
      }
      return item;
    }) ?? String(value)).toWellFormed();
  } catch {
    try { return String(value).toWellFormed(); } catch { return "[Unprintable]"; }
  }
}

export function createConsole(log) {
  return Object.freeze({
    log(...values) { log(values.map(format).join(" ")); },
  });
}
