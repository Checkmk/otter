/// Test utilities for writing and executing scripts in tests.
///
/// Handles platform-specific differences (Unix file permissions, etc.) and ensures
/// proper synchronization before subprocess execution.

use std::fs;
use std::path::{Path, PathBuf};

/// Writes an executable script to disk with proper permissions and durability guarantees.
///
/// On Unix systems: sets permissions to 0o755 and calls fsync() to ensure durability.
/// On other systems: just writes the file.
///
/// Returns the path to the written script.
pub fn write_executable_script(dir: &Path, name: &str, content: &str) -> std::io::Result<PathBuf> {
    let script_path = dir.join(name);
    fs::write(&script_path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
        // Ensure file is durably written before subprocess accesses it
        if let Ok(file) = fs::File::open(&script_path) {
            let _ = file.sync_all();
        }
    }

    Ok(script_path)
}
