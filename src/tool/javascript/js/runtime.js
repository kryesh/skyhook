import { stringify, returned } from "skyhook:values";
import { configure, describeError, errorText } from "skyhook:errors";
import { createConsole } from "skyhook:console";
import { install as installTools } from "skyhook:tools";
import { WorkPool } from "skyhook:pool";

// The longest delay a JavaScript timer accepts.
const MAX_SLEEP = 2 ** 31 - 1;

export function install({builders, source}, hostCall, hostSleep, log) {
  configure(source);
  const {tool, receive} = installTools(builders, hostCall);
  const sleep = ms => {
    if (typeof ms !== "number" || !(ms >= 0 && ms <= MAX_SLEEP)) {
      throw new TypeError(`sleep(ms) requires milliseconds from 0 to ${MAX_SLEEP}`);
    }
    return hostSleep(ms).then(elapsed => {
      if (!elapsed) throw new Error("sleep(ms) was cancelled");
    });
  };
  Object.assign(globalThis, {tool, receive, sleep, console: createConsole(log), WorkPool});
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
  const presented = Object.create(null);
  try {
    const {value, problem} = await returned(await body(), presented);
    return stringify({value, presented, ...(problem && {failure: {message: problem}})});
  } catch (error) {
    return stringify({value: null, presented: {}, failure: failure(error)});
  }
}
