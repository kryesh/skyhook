const __callKind = Symbol("skyhook.call");
const __executions = new WeakMap();
const __callData = new WeakMap();
const __parse = JSON.parse;
const __stringify = JSON.stringify;
const __annotatedArrays = new WeakSet();
const __annotatedProperties = new WeakMap();

function __rememberAnnotations(value, annotations) {
  if (value === null || typeof value !== "object") return;
  for (const pointer of annotations) {
    const keys = pointer === "" ? [] : pointer.slice(1).split("/").map(key => key.replace(/~1/g, "/").replace(/~0/g, "~"));
    let child = value, parent, key;
    for (key of keys) { parent = child; child = child[key]; }
    if (child !== null && typeof child === "object") __annotatedArrays.add(child);
    if (parent) {
      let properties = __annotatedProperties.get(parent);
      if (!properties) __annotatedProperties.set(parent, properties = new Set());
      properties.add(key);
    }
  }
}

function __consoleFormat(value) {
  if (typeof value === "string") return value;
  if (value instanceof Error) return value.stack || String(value);
  const ancestors = [];
  try {
    return __stringify(value, function (_key, item) {
      if (typeof item === "bigint") return `${item}n`;
      if (typeof item === "object" && item !== null) {
        while (ancestors.length && ancestors[ancestors.length - 1] !== this) ancestors.pop();
        if (ancestors.includes(item)) return "[Circular]";
        ancestors.push(item);
      }
      return item;
    }) ?? String(value);
  } catch {
    try { return String(value); } catch { return "[Unprintable]"; }
  }
}
const console = Object.freeze({
  log(...values) { __skyhookConsoleLog(values.map(__consoleFormat).join(" ")); },
});

function __plainObject(name, input) {
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    throw new TypeError(`tool.${name} argument must be a plain object`);
  }
  const prototype = Object.getPrototypeOf(input);
  if (prototype !== Object.prototype && prototype !== null) {
    throw new TypeError(`tool.${name} argument must be a plain object`);
  }
  return Object.assign(Object.create(null), input);
}

function __operation(name, input) {
  const operation = {};
  __callData.set(operation, { name, arguments: Object.freeze(Object.assign(Object.create(null), input)) });
  Object.defineProperty(operation, __callKind, { value: true });
  Object.defineProperty(operation, "then", {
    enumerable: false,
    value(onFulfilled, onRejected) {
      return __execute(operation).then(onFulfilled, onRejected);
    },
  });
  Object.defineProperty(operation, "catch", {
    enumerable: false,
    value(onRejected) { return __execute(operation).catch(onRejected); },
  });
  Object.defineProperty(operation, "finally", {
    enumerable: false,
    value(onFinally) { return __execute(operation).finally(onFinally); },
  });
  return operation;
}

const __reserved = new Set(["then", "catch", "finally", "set"]);
function __validIdentifier(value) {
  return typeof value === "string" && /^[A-Za-z_$][A-Za-z0-9_$]*(?![\s\S])/.test(value);
}

function __chainBuilder(manifest, input = Object.create(null)) {
  const operation = __operation(manifest.name, input);
  Object.defineProperty(operation, "set", {
    enumerable: false,
    value(key, value) {
      if (!manifest.properties.includes(key)) throw new TypeError(`unknown argument ${key} for tool.${manifest.name}`);
      return __chainBuilder(manifest, Object.assign(Object.create(null), input, {[key]: value}));
    },
  });
  for (const property of manifest.properties) {
    if (!__validIdentifier(property) || __reserved.has(property)) continue;
    Object.defineProperty(operation, property, {
      enumerable: false,
      value(value) { return __chainBuilder(manifest, Object.assign(Object.create(null), input, {[property]: value})); },
    });
  }
  return Object.freeze(operation);
}

function __builder(manifest, values) {
  if (values.length === 1) return __operation(manifest.name, __plainObject(manifest.name, values[0]));
  if (values.length !== 0) throw new TypeError(`tool.${manifest.name} accepts zero arguments or one argument object`);
  if (manifest.properties.length === 0) return __operation(manifest.name, Object.create(null));
  return __chainBuilder(manifest);
}

function __jobBuilder(manifest, job, values) {
  const input = Object.assign(Object.create(null), {[manifest.job_argument]: job});
  if (values.length === 1) return __operation(manifest.name, Object.assign(input, __plainObject(`job.${manifest.method}`, values[0])));
  if (values.length !== 0) throw new TypeError(`tool.job(job).${manifest.method} accepts zero arguments or one argument object`);
  if (manifest.required.length === 0) return __operation(manifest.name, input);
  return __chainBuilder(manifest, input);
}

const tool = Object.create(null);
for (const manifest of __builders) {
  if (manifest.binding === "top_level") tool[manifest.name] = (...values) => __builder(manifest, values);
}

function __jobResponseError(response, message) {
  const error = new Error(message);
  error.response = response;
  error.output = response.result ?? null;
  error.code = response.meta?.code ?? null;
  error.executed = response.meta?.executed ?? null;
  return error;
}

function __unwrapResponse(response) {
  if (response.state === "completed") {
    if (response.has_result !== true) {
      throw __jobResponseError(
        response,
        `job ${response.id ?? "(unassigned)"} has no loaded result; inspect it with tool.jobs({job: id})`,
      );
    }
    if (!Object.hasOwn(response, "result") || response.result === undefined) {
      throw new TypeError("unwrap completed response requires a JSON result field (null is allowed)");
    }
    return response.result;
  }
  if (["failed", "cancelled", "interrupted"].includes(response.state)) {
    throw __jobResponseError(
      response,
      response.error || `job ${response.id ?? "(unassigned)"} ${response.state}`,
    );
  }
  const state = typeof response.state === "string" ? response.state : "unknown";
  throw __jobResponseError(
    response,
    `job ${response.id ?? "(unassigned)"} is not completed (state: ${state})`,
  );
}

tool.job = job => {
  if (job && job[__callKind] === true) {
    throw new TypeError("tool.job received an unresolved tool builder; await the background call and pass result.id");
  }
  if (!Number.isInteger(job) || job < 1) {
    throw new TypeError("tool.job requires a positive integer job ID");
  }
  const methods = Object.create(null);
  for (const manifest of __builders) {
    if (manifest.binding === "job_method") methods[manifest.method] = (...values) => __jobBuilder(manifest, job, values);
  }
  return Object.freeze(methods);
};

async function __request(request) {
  const response = __parse(await __skyhookHostCall(__stringify(request)));
  if (!response.ok) {
    throw new Error(response.error);
  }
  if (response.annotations) __rememberAnnotations(response.value, response.annotations);
  // Only tool calls produce JobViews. A received message or a nested payload
  // may look like one, but remains arbitrary user JSON without runtime methods.
  if (request.type === "call") {
    // The Rust response type owns the schema. Guard only the object boundary
    // needed to install a runtime method; do not duplicate its field list here.
    if (response.value === null || typeof response.value !== "object" || Array.isArray(response.value)) {
      throw new TypeError(`tool "${request.name}" returned an invalid JobView envelope: expected an object`);
    }
    Object.defineProperty(response.value, "unwrap", {
      enumerable: false,
      value() { return __unwrapResponse(response.value); },
    });
  }
  return response.value;
}

function __toolError(message, cause) {
  const error = new Error(message, { cause });
  for (const key of ["output", "code", "executed"]) {
    if (cause && Object.hasOwn(cause, key)) error[key] = cause[key];
  }
  return error;
}

function __execute(operation) {
  if (!operation || operation[__callKind] !== true) throw new TypeError("expected a tool builder");
  const data = __callData.get(operation);
  let execution = __executions.get(operation);
  if (!execution) {
    execution = __request({ type: "call", name: data.name, arguments: data.arguments })
      .catch(error => { throw __toolError(`tool "${data.name}" failed: ${error?.message ?? error}`, error); });
    __executions.set(operation, execution);
  }
  return execution;
}

class WorkPool {
  #concurrency;
  get concurrency() { return this.#concurrency; }
  constructor(concurrency) {
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError("concurrency must be positive");
    if (arguments.length !== 1) throw new TypeError("WorkPool accepts only a concurrency limit");
    this.#concurrency = concurrency;
  }
  async *map(items, worker) {
    const values = Array.from(items), completed = [], running = new Set();
    let next = 0, stopped = false, wake;
    const signal = () => { if (wake) { const resolve = wake; wake = undefined; resolve(); } };
    const schedule = () => {
      while (!stopped && next < values.length && running.size < this.#concurrency) {
        const index = next++;
        const task = Promise.resolve().then(() => worker(values[index], index)).then(
          value => { completed.push({index, value}); },
          error => { console.log(`WorkPool item ${index} failed:`, error?.message ?? error); },
        ).finally(() => { running.delete(task); signal(); });
        running.add(task);
      }
    };
    try {
      schedule();
      while (next < values.length || running.size || completed.length) {
        if (completed.length) {
          yield completed.shift();
          schedule();
        } else {
          schedule();
          if (running.size) await new Promise(resolve => { wake = resolve; });
        }
      }
    } finally {
      stopped = true;
      await Promise.all(running);
      completed.length = 0;
    }
  }
  run(tasks) {
    if (arguments.length !== 1 || !Array.isArray(tasks)) {
      throw new TypeError("WorkPool.run expects one array of functions: run([f1, f2])");
    }
    const values = Array.from(tasks);
    for (let index = 0; index < values.length; index++) {
      if (typeof values[index] !== "function") {
        throw new TypeError(`WorkPool.run task at index ${index} must be a function`);
      }
    }
    return this.map(values, task => task());
  }
}

const receive = async () => __request({type:"receive"});

async function __resolve(value, path, ancestors, pointer, presentation) {
  if (value && value[__callKind] === true) {
    value = await __execute(value).catch(error => {
      throw __toolError(`deferred tool call at ${path} failed: ${error?.message ?? error}`, error);
    });
  }
  if (value === undefined) {
    if (path === "$") return null;
    throw new TypeError(`undefined at ${path} is not JSON-compatible`);
  }
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError(`non-finite number at ${path}`);
    return value;
  }
  if (typeof value !== "object") throw new TypeError(`${typeof value} at ${path} is not JSON-compatible`);
  if (ancestors.has(value)) throw new TypeError(`circular value at ${path}`);
  const nested = new Set(ancestors); nested.add(value);
  if (__annotatedArrays.has(value)) presentation.fields.push(pointer);
  const properties = __annotatedProperties.get(value);
  const output = Array.isArray(value) ? new Array(value.length) : Object.create(null);
  const keys = Array.isArray(value) ? value.map((_, index) => String(index)) : Object.keys(value);
  await Promise.all(keys.map(async key => {
    const childPointer = `${pointer}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`;
    if (properties?.has(key)) presentation.fields.push(childPointer);
    output[key] = await __resolve(value[key], Array.isArray(value) ? `${path}[${key}]` : `${path}.${key}`, nested, childPointer, presentation);
  }));
  return output;
}

// Capture thrown arrays and Error properties before crossing the JSON bridge.
function __describeError(error, seen = new Set()) {
  if (error === null || typeof error === "string" || typeof error === "boolean") return error;
  if (typeof error === "number" && Number.isFinite(error)) return error;
  if (typeof error !== "object") return String(error);
  if (seen.has(error)) return "[circular error]";
  const nested = new Set(seen); nested.add(error);
  if (Array.isArray(error)) return error.map(value => __describeError(value, nested));
  const result = {};
  for (const key of new Set([...Object.keys(error), "message", "stack", "cause"])) {
    if (key in error) result[key] = __describeError(error[key], nested);
  }
  return result;
}
