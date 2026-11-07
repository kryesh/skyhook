// `URL` and `URLSearchParams` on the host's WHATWG URL parser: the classes keep a URL
// as its href and ask the host for its components. Arguments convert as WebIDL has
// them: a missing required one throws, while an explicit undefined is a value.

// A web API call given fewer arguments than it requires throws, as WebIDL has it.
export function needs(given, count, call) {
  if (given < count) throw new TypeError(`${call} requires ${count} argument${count > 1 ? "s" : ""}, but only ${given} present`);
}

export function define(brand, parse, get, set, formParse, formSerialize) {
  const text = value => `${value}`.toWellFormed();
  const optional = value => value === undefined ? undefined : text(value);
  const object = value => (typeof value === "object" && value !== null) || typeof value === "function";
  let replace, bind;

  class URLSearchParams {
    #list = [];
    #update;
    static {
      replace = (params, list) => { params.#list = list; };
      bind = (params, update) => { params.#update = update; };
    }
    constructor(init = "") {
      brand(this);
      if (!object(init)) {
        const query = text(init);
        this.#list = formParse(query.startsWith("?") ? query.slice(1) : query);
        return;
      }
      const iterator = init[Symbol.iterator];
      if (iterator === undefined || iterator === null) {
        for (const name of Object.keys(init)) this.#list.push([text(name), text(init[name])]);
        return;
      }
      if (typeof iterator !== "function") throw new TypeError("URLSearchParams init's Symbol.iterator is not a function");
      // Each element converts as it is read, as a sequence does in WebIDL, so an iterator
      // reusing a buffer or an element gives what it held then.
      for (const pair of {[Symbol.iterator]: () => iterator.call(init)}) {
        if (!object(pair)) throw new TypeError("URLSearchParams pairs must be iterable");
        const entry = [];
        for (const item of pair) entry.push(text(item));
        if (entry.length !== 2) throw new TypeError("URLSearchParams pairs need a name and a value");
        this.#list.push(entry);
      }
    }
    #changed() { this.#update?.(formSerialize(this.#list)); }
    get size() { return this.#list.length; }
    append(name, value) {
      needs(arguments.length, 2, "URLSearchParams.append");
      this.#list.push([text(name), text(value)]);
      this.#changed();
    }
    delete(name, value) {
      needs(arguments.length, 1, "URLSearchParams.delete");
      [name, value] = [text(name), optional(value)];
      this.#list = this.#list.filter(([key, item]) => key !== name || (value !== undefined && item !== value));
      this.#changed();
    }
    get(name) {
      needs(arguments.length, 1, "URLSearchParams.get");
      name = text(name);
      return this.#list.find(([key]) => key === name)?.[1] ?? null;
    }
    getAll(name) {
      needs(arguments.length, 1, "URLSearchParams.getAll");
      name = text(name);
      return this.#list.filter(([key]) => key === name).map(([, item]) => item);
    }
    has(name, value) {
      needs(arguments.length, 1, "URLSearchParams.has");
      [name, value] = [text(name), optional(value)];
      return this.#list.some(([key, item]) => key === name && (value === undefined || item === value));
    }
    set(name, value) {
      needs(arguments.length, 2, "URLSearchParams.set");
      [name, value] = [text(name), text(value)];
      const index = this.#list.findIndex(([key]) => key === name);
      if (index < 0) this.#list.push([name, value]);
      else {
        this.#list[index][1] = value;
        this.#list = this.#list.filter(([key], at) => at <= index || key !== name);
      }
      this.#changed();
    }
    sort() {
      this.#list.sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
      this.#changed();
    }
    *entries() { for (let index = 0; index < this.#list.length; index++) yield [...this.#list[index]]; }
    *keys() { for (const [name] of this.entries()) yield name; }
    *values() { for (const [, value] of this.entries()) yield value; }
    forEach(callback, thisArg) {
      for (const [name, value] of this.entries()) callback.call(thisArg, value, name, this);
    }
    [Symbol.iterator]() { return this.entries(); }
    toString() { return formSerialize(this.#list); }
  }

  class URL {
    #href;
    #query = new URLSearchParams();
    static {
      // The components a URL reads and sets alike, through the host.
      for (const name of ["protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"]) {
        Object.defineProperty(URL.prototype, name, {
          get() { return get(this.#href, name); },
          set(value) { this.#set(name, value); },
          configurable: true,
        });
      }
    }
    static canParse(input, base) {
      needs(arguments.length, 1, "URL.canParse");
      return parse(text(input), optional(base)) !== null;
    }
    static parse(input, base) {
      needs(arguments.length, 1, "URL.parse");
      const href = parse(text(input), optional(base));
      return href === null ? null : new URL(href);
    }
    constructor(input, base) {
      needs(arguments.length, 1, "URL");
      brand(this);
      const href = parse(text(input), optional(base));
      if (href === null) throw new TypeError(`Invalid URL: ${text(input)}`);
      this.#href = href;
      bind(this.#query, query => { this.#href = set(this.#href, "search", query); });
      this.#parsed();
    }
    // Keeps searchParams in step with the query the href now has.
    #parsed() { replace(this.#query, formParse(get(this.#href, "search").slice(1))); }
    #set(name, value) {
      this.#href = set(this.#href, name, text(value));
      this.#parsed();
    }
    get href() { return this.#href; }
    set href(value) {
      const href = set(this.#href, "href", text(value));
      if (href === null) throw new TypeError(`Invalid URL: ${text(value)}`);
      this.#href = href;
      this.#parsed();
    }
    get origin() { return get(this.#href, "origin"); }
    get searchParams() { return this.#query; }
    toString() { return this.#href; }
    toJSON() { return this.#href; }
  }

  Object.assign(globalThis, {URL, URLSearchParams});
}
