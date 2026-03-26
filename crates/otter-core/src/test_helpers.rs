/// Test utilities for writing and executing scripts in tests.
///
/// Handles platform-specific differences (Unix file permissions, Windows .bat wrappers, etc.)
/// and ensures proper synchronization before subprocess execution.

use std::fs;
use std::path::{Path, PathBuf};

/// Returns the executable filename for a script name on the current platform.
///
/// On Unix this is the name unchanged. On Windows, `write_executable_script` creates a `.bat`
/// wrapper, so the executable name has `.bat` appended.
pub fn executable_name(script_name: &str) -> String {
    #[cfg(windows)]
    return format!("{script_name}.bat");
    #[cfg(not(windows))]
    return script_name.to_string();
}

/// Returns a path string safe to embed in bash scripts: forward slashes, no Windows `\\?\` prefix.
pub fn bash_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    // Strip the extended-length path prefix that Windows adds during canonicalization.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "/")
}

/// Writes an executable script to disk and returns a path that can be executed directly.
///
/// On Unix: writes the script with 0o755 permissions and fsyncs for durability.
/// On Windows: writes the bash script as-is, then writes a `.bat` wrapper that invokes
/// `bash <script>` — and returns the `.bat` path. This is needed because Windows cannot
/// execute shebang scripts directly (os error 193).
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
        return Ok(script_path);
    }

    #[cfg(windows)]
    {
        // Write a .bat wrapper so the path is directly executable on Windows.
        // The script content uses bash syntax, so we delegate to bash.
        let bat_name = format!("{name}.bat");
        let bat_path = dir.join(&bat_name);
        // Use forward slashes in the bash path argument to avoid escaping issues.
        let script_str = script_path.to_string_lossy().replace('\\', "/");
        let bat_content = format!("@bash \"{script_str}\" %*\r\n");
        fs::write(&bat_path, bat_content)?;
        return Ok(bat_path);
    }
}
