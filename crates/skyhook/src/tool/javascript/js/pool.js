import { summary } from "skyhook:errors";

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
    let offered = 0;
    const next = (_running, free) => {
      const items = [];
      while (items.length < free && offered < values.length) items.push({key: offered++});
      return items;
    };
    for await (const result of this.#feed(next, ({key}) => worker(values[key], key))) {
      const index = result.key;
      yield "error" in result ? {index, error: result.error} : {index, value: result.value};
    }
  }
  feed(next, worker, options = {}) {
    if (typeof next !== "function" || typeof worker !== "function") {
      throw new TypeError("WorkPool.feed expects next and worker functions: feed((running, free) => [...items], item => ...)");
    }
    const {wake} = options;
    if (wake !== undefined && typeof wake !== "function") throw new TypeError("WorkPool.feed wake must be a function");
    const items = running => new Map([...running].map(([key, r]) => [key, r.item]));
    return this.#feed((running, free) => next(items(running), free), worker, wake);
  }
  async *#feed(next, worker, wake) {
    // Items `next` returned beyond the free slots wait here and start before `next` is
    // asked again; those whose key runs are dropped, and an answer of only those ends the
    // filling until something settles. A key stays running until its result is yielded.
    // A wake is requested only while work runs; one still pending when the feed ends is
    // released, so the value it would have taken reaches the next caller.
    const running = new Map(), waiting = [], completed = [];
    let pending, resume;
    const settle = result => {
      completed.push(result);
      if (resume) { const resolve = resume; resume = undefined; resolve(); }
    };
    const keyOf = item => {
      const key = item?.key;
      if (typeof key !== "string" && typeof key !== "number") throw new TypeError("WorkPool.feed items need a string or number key");
      return key;
    };
    const start = item => {
      const key = keyOf(item);
      if (running.has(key)) return;
      const task = Promise.resolve().then(() => worker(item)).then(
        value => settle({key, item, value}),
        error => settle({key, item, error: summary(error)}),
      );
      running.set(key, {item, task});
    };
    try {
      for (;;) {
        while (running.size < this.#concurrency) {
          if (!waiting.length) {
            const free = this.#concurrency - running.size;
            const offered = [...await next(running, free)];
            waiting.push(...offered.filter(item => !running.has(keyOf(item))));
            if (!waiting.length) break;
          }
          start(waiting.shift());
        }
        if (!running.size) return;
        if (wake && !pending) {
          const received = wake(), woken = Promise.resolve(received);
          pending = {received};
          const ready = () => settle({woken});
          woken.then(ready, ready);
        }
        if (!completed.length) await new Promise(resolve => { resume = resolve; });
        const settled = completed.shift();
        if (settled.woken) {
          pending = undefined;
          yield {wake: await settled.woken};
        } else {
          running.delete(settled.key);
          yield settled;
        }
      }
    } finally {
      pending?.received?.release?.();
      await Promise.allSettled([...running.values()].map(r => r.task));
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
