use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DekaConfig {
    #[serde(default)]
    pub app: AppConfig,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub compile: CompileConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default = "default_app_name")]
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowConfig {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub min_width: Option<u32>,
    #[serde(default)]
    pub min_height: Option<u32>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub max_height: Option<u32>,
    #[serde(default = "default_true")]
    pub resizable: bool,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default = "default_true")]
    pub decorations: bool,
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default)]
    pub transparent: bool,
    #[serde(default)]
    pub center: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileConfig {
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub bundle: bool,
    #[serde(default)]
    pub desktop: bool,
}

// Default functions
fn default_app_name() -> String {
    "DekaApp".to_string()
}

fn default_version() -> String {
    "1.0.0".to_string()
}

fn default_title() -> String {
    "Deka App".to_string()
}

fn default_width() -> u32 {
    1200
}

fn default_height() -> u32 {
    800
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: default_app_name(),
            version: default_version(),
            identifier: None,
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            width: default_width(),
            height: default_height(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            resizable: true,
            fullscreen: false,
            decorations: true,
            always_on_top: false,
            transparent: false,
            center: false,
        }
    }
}

impl Default for CompileConfig {
    fn default() -> Self {
        Self {
            entry: None,
            bundle: false,
            desktop: false,
        }
    }
}

impl DekaConfig {
    /// Load config from simien.json in the current directory
    pub fn load(dir: &PathBuf) -> Result<Self, String> {
        let config_path = dir.join("simien.json");

        if !config_path.exists() {
            // Return default config if file doesn't exist
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read simien.json: {}", e))?;

        serde_json::from_str(&contents).map_err(|e| format!("Failed to parse simien.json: {}", e))
    }
}

impl Default for DekaConfig {
    fn default() -> Self {
        Self {
            app: AppConfig::default(),
            window: WindowConfig::default(),
            compile: CompileConfig::default(),
        }
    }
}
