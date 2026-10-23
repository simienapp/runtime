// simien:db's transport inside Simien (src/binding.rs): a query object goes
// to the engine through an op. The module apps import (modules::db_native)
// finds it here, under a symbol; nothing else is put on globalThis.

import { core } from "ext:core/mod.js";
import { op_simien_data_query } from "ext:core/ops";

// The tables a live query (APS 4) reads, collected while it runs: each read
// adds its table to the set of the live query it belongs to, however many
// awaits deep.
const reading = new core.AsyncVariable();
const READS = new Set(["findMany", "findFirst", "count"]);

const send = async (query) => {
  const tables = reading.get();
  if (tables !== undefined && READS.has(query.op)) tables.add(query.table);
  return JSON.parse(await op_simien_data_query(JSON.stringify(query)));
};

// Runs `run` and answers its result with the tables its reads named.
function track(run) {
  const tables = new Set();
  const previous = reading.enter(tables);
  let pending;
  try {
    pending = run();
  } finally {
    core.setAsyncContext(previous);
  }
  return Promise.resolve(pending).then((result) => ({ result, tables: [...tables].sort() }));
}

Object.defineProperty(globalThis, Symbol.for("simien.data.send"), { value: send });
Object.defineProperty(globalThis, Symbol.for("simien.data.track"), { value: track });
