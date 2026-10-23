export class WorkPool {
  #concurrency;
  get concurrency() { return this.#concurrency; }
  constructor(concurrency) {
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError("concurrency must be positive");
    if (arguments.length !== 1) throw new TypeError("WorkPool accepts only a concurrency limit");
    this.#concurrency = concurrency;
  }
  map(items, worker) {
    if (typeof worker !== "function") throw new TypeError("WorkPool.map expects a worker function: map(items, item => ...)");
    return this.#map(Array.from(items), worker);
  }
  async *#map(values, worker) {
    const completed = [], running = new Set();
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
