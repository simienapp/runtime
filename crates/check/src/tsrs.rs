//! tsrs, the Rust port of the TypeScript 7 checker, run in this process with
//! every diagnostic collected as data instead of printed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tsrs_ast::Diagnostic;
use tsrs_core::P;
use tsrs_diagnostics::Category;
use tsrs_execute::sys::{new_system, osSys};
use tsrs_execute::tsc::{ExitStatus, ReadWriteCloser, System};
use tsrs_vfs::FS;

/// Directories never searched for project files.
const SKIP: &[&str] = &["node_modules", ".git", ".simien", ".target", "target", "dist", ".tmp"];

/// Compiler options for a project without a tsconfig.json.
const DEFAULTS: &[&str] = &[
    "--strict",
    "--target",
    "esnext",
    "--module",
    "esnext",
    "--moduleResolution",
    "bundler",
    "--lib",
    "esnext,webworker",
    "--skipLibCheck",
];

/// What the checker found.
#[derive(Serialize, Debug)]
pub struct Report {
    pub ok: bool,
    pub files: usize,
    pub diagnostics: Vec<Item>,
}

/// One diagnostic; `file` is relative to the project root, line and column
/// 1-based.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Item {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    pub code: i32,
    pub category: &'static str,
    pub message: String,
}

type Sink = Box<dyn Fn(P<Diagnostic>) + Sync>;

/// tsrs's own system, with its output kept and every diagnostic handed to us
/// as data instead of being printed.
struct CheckSystem {
    os: osSys,
    output: Mutex<String>,
    sink: Sink,
}

impl System for CheckSystem {
    fn fs(&self) -> Arc<dyn FS> {
        self.os.fs()
    }
    fn default_library_path(&self) -> &str {
        self.os.default_library_path()
    }
    fn get_current_directory(&self) -> &str {
        self.os.get_current_directory()
    }
    fn write(&self, text: &str) {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).push_str(text);
    }
    fn flush(&self) {}
    fn write_output_is_tty(&self) -> bool {
        false
    }
    fn get_environment_variable(&self, name: &str) -> Option<String> {
        self.os.get_environment_variable(name)
    }
    fn spawn(&self, command: &[String], dir: &str, stderr: Option<Box<dyn Write + Send>>) -> Result<ReadWriteCloser, String> {
        self.os.spawn(command, dir, stderr)
    }
    fn now(&self) -> Instant {
        self.os.now()
    }
    fn since_start(&self) -> Duration {
        self.os.since_start()
    }
    fn diagnostic_sink(&self) -> Option<&(dyn Fn(P<Diagnostic>) + Sync)> {
        Some(&*self.sink)
    }
}

fn category(category: Category) -> &'static str {
    match category {
        Category::Error => "error",
        Category::Warning => "warning",
        Category::Suggestion => "suggestion",
        Category::Message => "message",
    }
}

/// The message with its chain, indented the way tsc prints it.
fn message(diagnostic: &Diagnostic) -> String {
    fn walk(d: &Diagnostic, depth: usize, out: &mut String) {
        if depth > 0 {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        }
        out.push_str(&d.localize());
        for next in d.message_chain() {
            walk(next, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(diagnostic, 0, &mut out);
    out
}

fn item(diagnostic: &Diagnostic, root: &Path) -> Item {
    let (file, line, column) = match diagnostic.file() {
        Some(file) => {
            let len = u32::try_from(file.text().len()).unwrap_or(u32::MAX);
            let pos = diagnostic.pos().unwrap_or(0).min(len);
            let (line, character) = tsrs_scanner::get_ecma_line_and_utf16_character_of_position(&*file, pos);
            let name = file.file_name().to_string();
            let shown = Path::new(&name)
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(name);
            (Some(shown), Some(line + 1), Some(character + 1))
        }
        None => (None, None, None),
    };
    Item {
        file,
        line,
        column,
        code: diagnostic.code(),
        category: category(diagnostic.category()),
        message: message(diagnostic),
    }
}

/// TypeScript files under `dir`, skipping build output and dependencies.
fn project_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP.contains(&name.as_str()) {
                project_files(&path, out);
            }
        } else if [".ts", ".tsx", ".mts", ".cts"].iter().any(|ext| name.ends_with(ext)) {
            out.push(path);
        }
    }
}

/// Type-checks the project in `root`. With a `tsconfig.json` there, that
/// config decides everything; without one, the project's TypeScript files are
/// checked with Simien's defaults (strict, ESNext, bundler resolution, and the
/// `webworker` library for the WinterTC globals). Type errors are a report,
/// not an `Err`; `Err` means the checker couldn't run.
///
/// tsrs reads files relative to the current directory, so this moves the
/// process into `root`.
pub fn run(root: &Path) -> Result<Report, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("can't open {}: {e}", root.display()))?;
    std::env::set_current_dir(&root).map_err(|e| format!("can't enter {}: {e}", root.display()))?;

    let mut args = vec!["--noEmit".to_string(), "--pretty".to_string(), "false".to_string()];
    let files = if root.join("tsconfig.json").is_file() {
        args.extend(["-p".to_string(), "tsconfig.json".to_string()]);
        0
    } else {
        let mut files = Vec::new();
        project_files(&root, &mut files);
        if files.is_empty() {
            return Ok(Report {
                ok: true,
                files: 0,
                diagnostics: Vec::new(),
            });
        }
        // A tsconfig.json further up belongs to something else: don't let it apply.
        args.push("--ignoreConfig".to_string());
        args.extend(DEFAULTS.iter().map(|s| s.to_string()));
        let count = files.len();
        args.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
        count
    };

    let found: Arc<Mutex<Vec<P<Diagnostic>>>> = Arc::default();
    let collect = Arc::clone(&found);
    // tsrs wants a 'static system; one is leaked per check.
    let system: &'static CheckSystem = Box::leak(Box::new(CheckSystem {
        os: new_system(),
        output: Mutex::new(String::new()),
        sink: Box::new(move |d| collect.lock().unwrap_or_else(|e| e.into_inner()).push(d)),
    }));

    let result = tsrs_execute::execute::command_line(system, args);
    let found = found.lock().unwrap_or_else(|e| e.into_inner());
    let diagnostics: Vec<Item> = found.iter().map(|d| item(d, &root)).collect();
    if matches!(
        result.status,
        ExitStatus::InvalidProject_OutputsSkipped | ExitStatus::NotImplemented
    ) && diagnostics.is_empty()
    {
        let output = system.output.lock().unwrap_or_else(|e| e.into_inner()).trim().to_string();
        return Err(if output.is_empty() {
            "tsrs could not check this project".to_string()
        } else {
            output
        });
    }
    let errors = diagnostics.iter().any(|d| d.category == "error");
    Ok(Report {
        ok: !errors,
        files,
        diagnostics,
    })
}
