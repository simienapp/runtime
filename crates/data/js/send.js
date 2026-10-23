// simien:db's transport inside Simien (src/binding.rs): a query object goes
// to the engine through an op. The module apps import (modules::db_native)
// finds it here, under a symbol; nothing else is put on globalThis.

import { op_simien_data_query } from "ext:core/ops";

const send = async (query) => JSON.parse(await op_simien_data_query(JSON.stringify(query)));

Object.defineProperty(globalThis, Symbol.for("simien.data.send"), { value: send });
