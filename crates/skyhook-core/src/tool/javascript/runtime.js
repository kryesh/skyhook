const __callKind = Symbol("skyhook.call");
const __executions = new WeakMap();
const __callData = new WeakMap();
const __parse = JSON.parse;
const __stringify = JSON.stringify;

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
  if (typeof value !== "string" || value.length === 0) return false;
  const first = value.charCodeAt(0);
  const firstOk = first === 36 || first === 95 || (first >= 65 && first <= 90) || (first >= 97 && first <= 122);
  if (!firstOk) return false;
  for (let index = 1; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (!(code === 36 || code === 95 || (code >= 48 && code <= 57) || (code >= 65 && code <= 90) || (code >= 97 && code <= 122))) return false;
  }
  return true;
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
    const error = new Error(response.error);
    for (const key of ["output", "code", "executed"]) {
      if (Object.hasOwn(response, key)) error[key] = response[key];
    }
    throw error;
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

// Root WorkPool iterators for this script runtime's lifetime as a workaround for
// QuickJS fix 7955cfd49e669f00d37aaf8cf37f868132f5a58d (closure-to-coroutine GC edges).
const __workPoolIterators = new Set();

class WorkPool {
  constructor(concurrency) {
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError("concurrency must be positive");
    if (arguments.length !== 1) throw new TypeError("WorkPool accepts only a concurrency limit");
    this.concurrency = concurrency;
  }
  map(items, worker) {
    const iterator = (async function* () {
      const values = Array.from(items), completed = [], running = new Set();
      let next = 0, stopped = false, wake;
      const signal = () => { if (wake) { const resolve = wake; wake = undefined; resolve(); } };
      const schedule = () => {
        while (!stopped && next < values.length && running.size < this.concurrency) {
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
    }).call(this);
    __workPoolIterators.add(iterator);
    return iterator;
  }
  run(tasks) { return this.map(tasks, task => task()); }
}

const receive = async () => __request({type:"receive"});

async function __resolve(value, path, ancestors) {
  if (value && value[__callKind] === true) {
    return __execute(value).catch(error => {
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
  if (Array.isArray(value)) return Promise.all(value.map((item, index) => __resolve(item, `${path}[${index}]`, nested)));
  const output = {};
  await Promise.all(Object.keys(value).map(async key => { output[key] = await __resolve(value[key], `${path}.${key}`, nested); }));
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
