use super::security::{enforce_read, enforce_write};
use super::*;

#[derive(serde::Serialize)]
pub(super) struct PhpDirEntry {
    name: String,
    is_dir: bool,
    is_file: bool,
}

#[op2]
#[buffer]
pub(super) fn op_php_read_file_sync(
    #[string] path: String,
) -> Result<Vec<u8>, simien_core::error::CoreError> {
    enforce_read(Some(&path))?;
    std::fs::read(&path).map_err(|e| {
        simien_core::error::CoreError::from(std::io::Error::new(
            e.kind(),
            format!("Failed to read file '{}': {}", path, e),
        ))
    })
}

#[op2(fast)]
pub(super) fn op_php_write_file_sync(
    #[string] path: String,
    #[buffer] data: &[u8],
) -> Result<(), simien_core::error::CoreError> {
    enforce_write(Some(&path))?;
    std::fs::write(&path, data).map_err(|e| {
        simien_core::error::CoreError::from(std::io::Error::new(
            e.kind(),
            format!("Failed to write file '{}': {}", path, e),
        ))
    })
}

#[op2(fast)]
pub(super) fn op_php_mkdirs(#[string] path: String) -> Result<(), simien_core::error::CoreError> {
    enforce_write(Some(&path))?;
    std::fs::create_dir_all(&path).map_err(|e| {
        simien_core::error::CoreError::from(std::io::Error::new(
            e.kind(),
            format!("Failed to create dir '{}': {}", path, e),
        ))
    })
}

#[op2]
#[string]
pub(super) fn op_php_cwd() -> Result<String, simien_core::error::CoreError> {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| simien_core::error::CoreError::from(e))
}

/// Resolve all symlinks in `path` via the OS and return the canonical absolute
/// path, or None (null in JS) if the path does not exist or cannot be resolved.
/// Used by the __dekaFs / dekaFsAllow guards to prevent symlink-based
/// path-traversal out of the tenant root.
#[op2]
#[string]
pub(super) fn op_php_canonicalize(#[string] path: String) -> Option<String> {
    std::fs::canonicalize(&path)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

#[op2(fast)]
pub(super) fn op_php_file_exists(#[string] path: String) -> bool {
    if enforce_read(Some(&path)).is_err() {
        return false;
    }
    std::path::Path::new(&path).exists()
}

#[op2]
#[string]
pub(super) fn op_php_path_resolve(#[string] base: String, #[string] path: String) -> String {
    let _ = enforce_read(Some(&base));
    let _ = enforce_read(Some(&path));
    if let Some(stripped) = path.strip_prefix("@/") {
        // The `@/` alias resolves against the module root the dispatch layer
        // installed explicitly; the process environment is not consulted
        // (deka#801). Fall back to the cwd, as before.
        let root = crate::host_config::handler_paths()
            .and_then(|paths| paths.module_root.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
        if !root.is_empty() {
            return std::path::Path::new(&root)
                .join(stripped)
                .to_string_lossy()
                .to_string();
        }
    }

    let base_path = std::path::Path::new(&base);
    let target_path = std::path::Path::new(&path);

    let resolved = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        base_path.join(target_path)
    };

    resolved.to_string_lossy().to_string()
}

#[op2]
#[serde]
pub(super) fn op_php_read_dir(
    #[string] path: String,
) -> Result<Vec<PhpDirEntry>, simien_core::error::CoreError> {
    enforce_read(Some(&path))?;
    let entries = std::fs::read_dir(&path).map_err(|e| {
        simien_core::error::CoreError::from(std::io::Error::new(
            e.kind(),
            format!("Failed to read dir '{}': {}", path, e),
        ))
    })?;

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            simien_core::error::CoreError::from(std::io::Error::new(
                e.kind(),
                format!("Failed to read dir entry in '{}': {}", path, e),
            ))
        })?;
        let file_type = entry.file_type().map_err(|e| {
            simien_core::error::CoreError::from(std::io::Error::new(
                e.kind(),
                format!("Failed to read dir entry type in '{}': {}", path, e),
            ))
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            simien_core::error::CoreError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Failed to read dir entry name: name is not valid UTF-8",
            ))
        })?;
        out.push(PhpDirEntry {
            name,
            is_dir: file_type.is_dir(),
            is_file: file_type.is_file(),
        });
    }
    Ok(out)
}
