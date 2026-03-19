use std::path::Path;

/// Extension trait that prepends a scripts directory to the PATH environment variable
/// of a subprocess command, giving workflow companion scripts priority over system binaries.
pub trait PrependScriptsDir {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self;
}

impl PrependScriptsDir for tokio::process::Command {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self {
        if let Some(dir) = scripts_dir {
            let old_path = std::env::var_os("PATH").unwrap_or_default();
            let mut parts: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
            parts.extend(std::env::split_paths(&old_path));
            if let Ok(new_path) = std::env::join_paths(parts) {
                self.env("PATH", new_path);
            }
        }
        self
    }
}
