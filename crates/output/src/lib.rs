//! Agent-first output, as in the zega and TypeScript simien CLIs: JSON on
//! stdout by default. With --human, stdio prints for people (problems on
//! stderr, the rest on stdout). A failure is JSON on stderr (stdio's error
//! with --human) and exit code 1; a usage mistake is the same with code 2.

pub mod banner;

use serde::Serialize;
use serde_json::{Map, Value, json};

/// The global output flags, read once and put on the command's context.
#[derive(Debug, Clone, Copy, Default)]
pub struct Globals {
    pub human: bool,
    pub nopretty: bool,
}

impl Globals {
    pub fn from_args(args: &dcore::Args) -> Self {
        Self {
            human: args.flags.contains_key("--human"),
            nopretty: args.flags.contains_key("--nopretty"),
        }
    }

    /// The globals of a running command.
    pub fn of(context: &dcore::Context) -> Self {
        context
            .extensions()
            .get::<Globals>()
            .copied()
            .unwrap_or_else(|| Self::from_args(&context.args))
    }

    pub fn json(&self, value: &impl Serialize) -> String {
        if self.nopretty {
            serde_json::to_string(value)
        } else {
            serde_json::to_string_pretty(value)
        }
        .expect("output is serializable")
    }
}

/// A command's answer: JSON, or `human` printing it through stdio.
pub fn result(value: &impl Serialize, globals: Globals, human: impl FnOnce()) {
    if globals.human {
        human();
    } else {
        println!("{}", globals.json(value));
    }
}

/// One line per event, for long-running commands.
pub fn event(value: &impl Serialize, globals: Globals, human: impl FnOnce()) {
    if globals.human {
        human();
    } else {
        println!(
            "{}",
            serde_json::to_string(value).expect("output is serializable")
        );
    }
}

/// An error the user can act on: what happened and, when known, details
/// such as where (file, line, column) or what to try (hint).
#[derive(Debug, Clone)]
pub struct SimienError {
    pub message: String,
    pub details: Map<String, Value>,
}

impl SimienError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_string(), value.into());
        self
    }
}

impl<T: std::fmt::Display> From<T> for SimienError {
    fn from(error: T) -> Self {
        Self::new(error.to_string())
    }
}

/// Prints a failure on stderr.
pub fn failure(error: &SimienError, globals: Globals) {
    if globals.human {
        stdio::error("error", &error.message);
        if let Some(Value::String(hint)) = error.details.get("hint") {
            stdio::hint(hint);
        }
    } else {
        let mut body = Map::new();
        body.insert("message".into(), Value::String(error.message.clone()));
        body.extend(error.details.clone());
        eprintln!("{}", json!({ "ok": false, "error": body }));
    }
}

/// A command-line mistake: JSON on stderr like any other error, with a
/// suggestion when there is one.
pub fn usage_error(message: &str, suggestion: Option<&str>, globals: Globals) {
    if globals.human {
        stdio::error("usage", message);
        stdio::next_step("see the commands", "simien --help --human");
        return;
    }
    let mut error = json!({ "code": "usage", "message": message, "hint": "simien --help" });
    if let Some(suggestion) = suggestion {
        error["suggestion"] = Value::String(suggestion.to_string());
    }
    eprintln!("{}", json!({ "ok": false, "error": error }));
}
