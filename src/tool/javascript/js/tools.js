import { IDENTIFIER, parse, stringify, argument, annotate } from "skyhook:values";

const RESERVED = new Set(["then", "catch", "finally", "set"]);
const calls = new WeakMap();
const executions = new WeakMap();
let host;

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
  const error = new Error(["failed", "cancelled", "interrupted"].includes(state)
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

// A builder is an immutable snapshot of converted arguments that executes once,
// when first awaited; each setter derives a new builder.
function builder(manifest, input) {
  Object.freeze(input);
  const call = {};
  const set = (key, value) => {
    if (!manifest.properties.includes(key)) throw new TypeError(`unknown argument ${String(key)} for ${manifest.call}`);
    return builder(manifest, argument({...input, [key]: value}, `${manifest.call} argument`));
  };
  Object.defineProperties(call, {
    then: {value: (onFulfilled, onRejected) => execute(call).then(onFulfilled, onRejected)},
    catch: {value: onRejected => execute(call).catch(onRejected)},
    finally: {value: onFinally => execute(call).finally(onFinally)},
    set: {value: set},
  });
  for (const key of manifest.setters) Object.defineProperty(call, key, {value: value => set(key, value)});
  calls.set(call, {name: manifest.name, input});
  return Object.freeze(call);
}

function object(manifest, values) {
  if (values.length > 1) throw new TypeError(`${manifest.call} accepts zero arguments or one argument object`);
  const input = values.length ? values[0] : {};
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    throw new TypeError(`${manifest.call} argument must be a plain object`);
  }
  return argument(input, `${manifest.call} argument`);
}

export function install(manifests, call) {
  host = call;
  for (const manifest of manifests) {
    manifest.call = manifest.binding === "job_method" ? `tool.job(id).${manifest.method}` : `tool.${manifest.name}`;
    manifest.setters = manifest.properties.filter(key => IDENTIFIER.test(key) && !RESERVED.has(key));
  }
  const tool = Object.create(null);
  for (const manifest of manifests) {
    if (manifest.binding === "top_level") {
      tool[manifest.name] = (...values) => builder(manifest, object(manifest, values));
    }
  }
  tool.job = job => {
    if (calls.has(job)) throw new TypeError("tool.job received an unresolved tool builder; await the background call and pass its response's id (launch.id)");
    if (!Number.isSafeInteger(job) || job < 1) throw new TypeError("tool.job requires a positive safe integer job ID");
    const methods = Object.create(null);
    for (const manifest of manifests) {
      if (manifest.binding !== "job_method") continue;
      methods[manifest.method] = (...values) => {
        const input = object(manifest, values);
        if (Object.hasOwn(input, manifest.job_argument)) throw new TypeError(`${manifest.call} cannot override its bound job ID`);
        return builder(manifest, {...input, [manifest.job_argument]: job});
      };
    }
    return Object.freeze(methods);
  };
  return {tool: Object.freeze(tool), receive: () => request({type: "receive"})};
}
