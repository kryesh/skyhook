import { parse, stringify, argument, annotate } from "skyhook:values";

const calls = new WeakMap();
const executions = new WeakMap();
let host, terminal;

async function request(message) {
  const response = parse(await host(stringify(message)));
  if (response.outcome !== "ok") throw new Error(response.error);
  annotate(response.value, response.presented);
  return response.value;
}

function unwrap(view) {
  const state = view.state ?? "completed";
  if (state === "completed") return view.result ?? null;
  const id = view.id ?? "(unassigned)";
  const error = new Error(terminal.has(state)
    ? view.error || `job ${id} ${state}`
    : `job ${id} is not completed (state: ${state})`);
  Object.defineProperties(error, {response: {value: view}, output: {value: view.result}});
  if (view.id != null) error.job = view.id;
  if (view.meta?.code != null) error.code = view.meta.code;
  throw error;
}

function execute(call) {
  let execution = executions.get(call);
  if (!execution) {
    const {name, input} = calls.get(call);
    execution = request({type: "call", name, arguments: input}).then(
      view => Object.defineProperty(view, "unwrap", {value: () => unwrap(view)}),
      error => { throw new Error(`tool "${name}" failed: ${error.message}`, {cause: error}); },
    );
    executions.set(call, execution);
  }
  return execution;
}

// A builder is a snapshot of converted arguments that executes once, when first awaited.
function builder(name, input) {
  const call = {};
  Object.defineProperties(call, {
    then: {value: (onFulfilled, onRejected) => execute(call).then(onFulfilled, onRejected)},
    catch: {value: onRejected => execute(call).catch(onRejected)},
    finally: {value: onFinally => execute(call).finally(onFinally)},
  });
  calls.set(call, {name, input: Object.freeze(input)});
  return Object.freeze(call);
}

function object(call, values) {
  if (values.length > 1) throw new TypeError(`${call} accepts zero arguments or one argument object`);
  const input = argument(values.length ? values[0] : {}, `${call} argument`);
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    throw new TypeError(`${call} argument must be a plain object`);
  }
  return input;
}

export function install(names, ended, call) {
  host = call;
  terminal = new Set(ended);
  const tool = Object.create(null);
  for (const name of names) tool[name] = (...values) => builder(name, object(`tool.${name}`, values));
  return {tool: Object.freeze(tool), receive: inbox()};
}

// One host receive runs at a time on behalf of the waiting callers, oldest first. A
// value no caller waits for is kept for the next one, so releasing an abandoned receive
// never loses input: release() drops a waiting call, or hands the value it already took
// to the oldest waiting call, or back among the kept values in the order they arrived.
function inbox() {
  const kept = [], waiters = [];
  let pulling = false, arrived = 0;
  const keep = entry => {
    const later = kept.findIndex(other => other.order > entry.order);
    kept.splice(later < 0 ? kept.length : later, 0, entry);
  };
  const pull = () => {
    if (pulling || !waiters.length) return;
    pulling = true;
    request({type: "receive"}).then(
      value => {
        pulling = false;
        const entry = {value, order: arrived++}, waiter = waiters.shift();
        if (waiter) waiter.take(entry); else keep(entry);
        pull();
      },
      error => { pulling = false; waiters.shift()?.reject(error); pull(); },
    );
  };
  return () => {
    let waiter;
    const received = new Promise((resolve, reject) => {
      waiter = {reject, taken: undefined, take: entry => { waiter.taken = entry; resolve(entry.value); }};
    });
    const release = () => {
      const index = waiters.indexOf(waiter);
      if (index >= 0) waiters.splice(index, 1);
      else if (waiter.taken) {
        const entry = waiter.taken;
        waiter.taken = undefined;
        const oldest = waiters.shift();
        if (oldest) oldest.take(entry); else keep(entry);
      }
    };
    if (kept.length) waiter.take(kept.shift());
    else { waiters.push(waiter); pull(); }
    return Object.defineProperty(received, "release", {value: release});
  };
}
