import { stringify, returned } from "skyhook:values";
import { configure, describeError, errorText } from "skyhook:errors";
import { createConsole } from "skyhook:console";
import { install as installTools } from "skyhook:tools";
import { WorkPool } from "skyhook:pool";
import { scheduling } from "skyhook:web";

export { define as prepare } from "skyhook:web";

// Thirty minutes, as for the wait tool.
const MAX_SLEEP = 30 * 60 * 1000;

function delay(ms) {
  if (typeof ms !== "number" || !(ms >= 0 && ms <= MAX_SLEEP)) {
    throw new TypeError(`sleep(ms) requires milliseconds from 0 to ${MAX_SLEEP}`);
  }
  return ms;
}

// Timers make a missing, negative or NaN delay zero, as browsers do.
function timerDelay(ms) {
  const value = Number(ms);
  return delay(value > 0 ? value : 0);
}

export function install({builders, terminal, filename}, hostCall, hostSleep, hostWake, log, now) {
  configure(filename);
  const {tool, receive} = installTools(builders, terminal, hostCall);
  const sleep = ms => hostSleep(delay(ms))[0].then(() => {});
  const performance = Object.freeze({now});
  const console = createConsole(log);
  const scheduled = scheduling(timerDelay, hostSleep, hostWake, error => console.log("Uncaught", error));
  Object.assign(globalThis, {tool, receive, sleep, console, performance, WorkPool, ...scheduled});
  // Only some V8 builds include WebAssembly, and each context installs it anew.
  delete globalThis.WebAssembly;
  // V8 settles a waitAsync timeout through a delayed task, which nothing here runs.
  delete Atomics.waitAsync;
  // Results hold JSON a tool already decoded (a command's JSON stdout, say) as the value
  // itself, so parsing one returns a copy of it rather than failing on "[object Object]".
  const parse = JSON.parse;
  JSON.parse = function (text, reviver) {
    if (text !== null && typeof text === "object") text = JSON.stringify(text);
    return parse(text, reviver);
  };
}

// An error's text is its message and stack; its other properties are details.
// Any other thrown value is reported as its JSON.
function failure(error) {
  const description = describeError(error);
  const message = errorText(description);
  if (message === undefined) return {message: stringify(description)};
  const details = {...description};
  delete details.name;
  delete details.message;
  if (typeof details.stack === "string") delete details.stack;
  return Object.keys(details).length ? {message, details} : {message};
}

export function fail(error) {
  return stringify(failure(error));
}

export async function run(body) {
  // Keyed by pointers, which never name a prototype member; a null prototype would
  // take the whole envelope off JSON.stringify's fast path.
  const presented = {};
  try {
    const {value, problem} = await returned(await body(), presented);
    return stringify({value, presented, ...(problem && {failure: {message: problem}})});
  } catch (error) {
    return stringify({value: null, presented: {}, failure: failure(error)});
  }
}
