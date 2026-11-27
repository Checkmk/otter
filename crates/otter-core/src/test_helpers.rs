/// Test utilities for writing and executing scripts in tests.
///
/// Handles platform-specific differences (Unix file permissions, Windows .bat wrappers, etc.)
/// and ensures proper synchronization before subprocess execution.
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
/// On Unix: a child process writes the script with 0o755 permissions. Writing it from
/// the test process would race with parallel tests: a subprocess forked meanwhile
/// inherits the open write handle until it execs, and executing the script during that
/// window fails with "Text file busy".
/// On Windows: writes the bash script as-is, then writes a `.bat` wrapper that invokes
/// `bash <script>` — and returns the `.bat` path. This is needed because Windows cannot
/// execute shebang scripts directly (os error 193).
pub fn write_executable_script(dir: &Path, name: &str, content: &str) -> std::io::Result<PathBuf> {
    let script_path = dir.join(name);

    #[cfg(unix)]
    {
        use std::io::Write;
        let mut writer = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&script_path)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        writer
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(content.as_bytes())?;
        let status = writer.wait()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "writing {} failed: {status}",
                script_path.display()
            )));
        }
        return Ok(script_path);
    }

    #[cfg(windows)]
    {
        std::fs::write(&script_path, content)?;
        // Write a .bat wrapper so the path is directly executable on Windows.
        // The script content uses bash syntax, so we delegate to bash.
        let bat_name = format!("{name}.bat");
        let bat_path = dir.join(&bat_name);
        // Use forward slashes in the bash path argument to avoid escaping issues.
        let script_str = script_path.to_string_lossy().replace('\\', "/");
        let bat_content = format!("@bash \"{script_str}\" %*\r\n");
        std::fs::write(&bat_path, bat_content)?;
        return Ok(bat_path);
    }
}
