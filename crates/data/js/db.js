// simien:db's client (APS 4): typed tables whose calls become query objects.
// `send` carries a query object to the engine and returns its result; the
// engine checks every query against schema.ts and runs it on Postgres.

export class SimienDataError extends Error {
  constructor(kind, message) {
    super(message);
    this.name = "SimienDataError";
    this.kind = kind;
  }
}

const selected = (columns) =>
  columns === undefined ? undefined : Object.keys(columns).filter((name) => columns[name]);

const ordered = (orderBy) => (orderBy === undefined ? [] : Object.entries(orderBy));

function table(name, send) {
  const read = (op, query = {}) =>
    send({
      op,
      table: name,
      where: query.where,
      columns: selected(query.columns),
      orderBy: ordered(query.orderBy),
      limit: query.limit,
    });
  return Object.freeze({
    findMany: (query) => read("findMany", query),
    findFirst: (query) => read("findFirst", query),
    count: (query = {}) => send({ op: "count", table: name, where: query.where }),
    insert: (values) => send({ op: "insert", table: name, values }),
    update: ({ where, set }) => send({ op: "update", table: name, where, values: set }),
    delete: ({ where }) => send({ op: "delete", table: name, where }),
  });
}

export function client(schema, send) {
  const db = { inc: (step) => Object.freeze({ $inc: step }) };
  for (const [name, definition] of Object.entries(schema)) {
    if (definition?.kind === "table") db[name] = table(name, send);
  }
  return Object.freeze(db);
}
