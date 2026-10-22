// simien:schema: the values schema.ts is written with. They carry each
// field's type at runtime, so `connect(schema)` knows the tables, and the
// schema engine reads the same file statically.

const validator = (type) => Object.freeze({ kind: "validator", type, optional: false });

export const v = Object.freeze({
  string: () => validator("string"),
  int: () => validator("int"),
  float: () => validator("float"),
  boolean: () => validator("bool"),
  optional: (inner) => Object.freeze({ ...inner, optional: true }),
});

export const defineTable = (fields) => Object.freeze({ kind: "table", fields: Object.freeze({ ...fields }) });

export const defineEdge = (from, to, fields = {}) =>
  Object.freeze({ kind: "edge", from, to, fields: Object.freeze({ ...fields }) });

export const defineSchema = (schema) => Object.freeze({ ...schema });
