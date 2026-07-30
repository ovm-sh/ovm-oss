use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum OvmError {
    #[error("Version {0} is already installed")]
    VersionAlreadyInstalled(String),

    #[error("No active version set. Run: `ovm use <product> <version>`.")]
    NoActiveVersion,

    #[error("Version {0} not found in npm registry")]
    VersionNotFound(String),

    #[error("Failed to read symlink at {}: {source}", path.display())]
    SymlinkRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to create symlink at {}: {source}", path.display())]
    SymlinkCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("npm not found. Install Node.js via fnm or your package manager.")]
    NpmNotFound,

    #[error("npm install failed for version {version}: {message}")]
    NpmInstallFailed { version: String, message: String },

    #[error("Failed to download {url}: {message}")]
    DownloadFailed { url: String, message: String },

    #[error("Failed to extract archive: {0}")]
    ExtractionFailed(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("{0}")]
    Message(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, OvmError>;
