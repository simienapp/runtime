//! The JavaScript modules apps import: `simien:schema`, and `simien:db` bound
//! to a transport. They ship inside the binary with the engine, so the client
//! and the engine are always the same version.

/// `simien:schema`: what schema.ts is written with.
pub const SCHEMA: &str = include_str!("../js/schema.js");

/// The transport-free client, shared by every way of reaching the engine.
const CLIENT: &str = include_str!("../js/db.js");

/// `simien:db` for an engine behind an HTTP endpoint (the dev data service):
/// each query is a POST of its query object with the service's token.
///
/// The module holds the token, so it is server-only: the app's framework must
/// refuse it in browser code.
pub fn db_over_http(endpoint: &str, token: &str) -> String {
    let endpoint = serde_json::to_string(endpoint).expect("a string");
    let token = serde_json::to_string(token).expect("a string");
    format!(
        r#"{CLIENT}
const endpoint = {endpoint};
const token = {token};

async function send(query) {{
  const response = await fetch(endpoint, {{
    method: "POST",
    headers: {{ authorization: `Bearer ${{token}}`, "content-type": "application/json" }},
    body: JSON.stringify(query),
  }});
  const body = await response.json();
  if (!response.ok) throw new SimienDataError(body.error.kind, body.error.message);
  return body.result;
}}

export function connect(schema) {{
  return client(schema, send);
}}
"#
    )
}
