pub mod daemon;

use std::path::PathBuf;

pub fn memex_root() -> PathBuf {
    if let Ok(root) = std::env::var("MEMEX_ROOT") {
        return PathBuf::from(root);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".memex")
}
