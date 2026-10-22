// Drives simien:db as an app's server code does in dev: the modules come from
// the data service, and every call goes through it to the engine.
// Usage: bun client.mjs <.simien/data.json> <a folder to put the modules in>
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [file, folder] = process.argv.slice(2);
const { url, token } = JSON.parse(readFileSync(file, "utf8"));
const authorization = `Bearer ${token}`;

async function module(name) {
  const response = await fetch(`${url}/modules/${name}`, { headers: { authorization } });
  if (!response.ok) throw new Error(`${name}: ${response.status}`);
  const path = join(folder, `${name.replace(":", "-")}.mjs`);
  writeFileSync(path, await response.text());
  return import(pathToFileURL(path).href);
}

const { defineSchema, defineTable, v } = await module("simien:schema");
const { connect } = await module("simien:db");
const db = connect(defineSchema({ counters: defineTable({ label: v.string(), value: v.int() }) }));

const made = await db.counters.insert({ label: "clicks", value: 0 });
await Promise.all(
  Array.from({ length: 10 }, () => db.counters.update({ where: { id: made.id }, set: { value: db.inc(1) } })),
);
const after = await db.counters.findFirst({ where: { id: made.id } });
const labels = await db.counters.findMany({ columns: { label: true }, orderBy: { createdAt: "desc" } });
let refused = null;
try {
  await db.counters.insert({ label: "no value" });
} catch (error) {
  refused = { name: error.name, kind: error.kind, message: error.message };
}
const count = await db.counters.count();
const unauthorized = (await fetch(`${url}/query`, { method: "POST", body: "{}" })).status;
console.log(JSON.stringify({ made, after, labels, refused, count, unauthorized }));
