// The one Node API carved out for the framework so far (Simien APS 2):
// AsyncLocalStorage, which React Server Components (and so every App Router
// app) needs. It is built on core's async context, which V8 carries across
// await and promise jobs and which core's timers carry across callbacks.
//
// A context is an immutable map from each AsyncLocalStorage to its store; a
// change makes a new map, so a context captured earlier never sees it.

import { core, primordials } from "ext:core/mod.js";
const { getAsyncContext, setAsyncContext } = core;
const { MapPrototypeDelete, MapPrototypeForEach, MapPrototypeGet, MapPrototypeSet, ReflectApply, SafeMap } = primordials;

function current() {
  const context = getAsyncContext();
  return context instanceof SafeMap ? context : undefined;
}

function changed(storage, store, present) {
  const before = current();
  const next = new SafeMap();
  if (before) {
    MapPrototypeForEach(before, (value, key) => MapPrototypeSet(next, key, value));
  }
  if (present) {
    MapPrototypeSet(next, storage, store);
  } else {
    MapPrototypeDelete(next, storage);
  }
  return next;
}

function within(context, fn, thisArg, args) {
  const before = getAsyncContext();
  setAsyncContext(context);
  try {
    return ReflectApply(fn, thisArg, args);
  } finally {
    setAsyncContext(before);
  }
}

class AsyncLocalStorage {
  /** Runs `fn` with `store` as this storage's store, then restores the outer one. */
  run(store, fn, ...args) {
    return within(changed(this, store, true), fn, undefined, args);
  }

  /** Runs `fn` with no store for this storage. */
  exit(fn, ...args) {
    return within(changed(this, undefined, false), fn, undefined, args);
  }

  /** The store of the innermost `run` (or `enterWith`) on this storage. */
  getStore() {
    const context = current();
    return context ? MapPrototypeGet(context, this) : undefined;
  }

  /** Sets the store for the rest of the current synchronous execution and what it schedules. */
  enterWith(store) {
    setAsyncContext(changed(this, store, true));
  }

  /** Clears this storage's store in the current context. */
  disable() {
    setAsyncContext(changed(this, undefined, false));
  }

  /** `fn`, bound to the context it was bound in. */
  static bind(fn) {
    const context = getAsyncContext();
    return function bound(...args) {
      return within(context, fn, this, args);
    };
  }

  /** A function that runs any function in the context captured now. */
  static snapshot() {
    const context = getAsyncContext();
    return (fn, ...args) => within(context, fn, undefined, args);
  }
}

export { AsyncLocalStorage };
