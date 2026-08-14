//! Validate the launcher-established environment for the private GStreamer runtime.
//!
//! The bundled launcher must run the process. Rust deliberately does not mutate
//! process-wide environment variables: it only locates the bundle and rejects a
//! process that was not started with the exact private runtime contract.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::{debug, info};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimePaths {
    vendor_dir: PathBuf,
    lib_dir: PathBuf,
    plugin_dir: PathBuf,
    scanner_path: PathBuf,
    launcher_path: PathBuf,
}

impl RuntimePaths {
    fn new(vendor_dir: PathBuf) -> Self {
        let lib_dir = vendor_dir.join("lib");
        Self {
            plugin_dir: lib_dir.join("gstreamer-1.0"),
            scanner_path: vendor_dir.join("libexec/gstreamer-1.0/gst-plugin-scanner"),
            launcher_path: vendor_dir.join("bin/lumina-gstreamer-runtime"),
            lib_dir,
            vendor_dir,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !is_non_empty_dir(&self.lib_dir) {
            return Err(format!(
                "vendored-runtime: library directory is missing or empty: {}",
                self.lib_dir.display()
            ));
        }
        if !is_non_empty_dir(&self.plugin_dir) {
            return Err(format!(
                "vendored-runtime: plugin directory is missing or empty: {}",
                self.plugin_dir.display()
            ));
        }
        if !self.scanner_path.is_file() {
            return Err(format!(
                "vendored-runtime: plugin scanner is missing: {}",
                self.scanner_path.display()
            ));
        }
        if !self.launcher_path.is_file() {
            return Err(format!(
                "vendored-runtime: runtime launcher is missing: {}",
                self.launcher_path.display()
            ));
        }
        Ok(())
    }

    fn validate_environment(
        &self,
        getenv: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<(), String> {
        self.require_path("GST_PLUGIN_PATH_1_0", &self.plugin_dir, getenv)?;
        self.require_empty("GST_PLUGIN_SYSTEM_PATH_1_0", getenv)?;
        self.require_empty("GST_PLUGIN_PATH", getenv)?;
        self.require_empty("GST_PLUGIN_SYSTEM_PATH", getenv)?;
        self.require_path("GST_PLUGIN_SCANNER_1_0", &self.scanner_path, getenv)?;
        self.require_empty("GST_PLUGIN_SCANNER", getenv)?;
        self.require_path("LD_LIBRARY_PATH", &self.lib_dir, getenv)?;
        self.require_empty("GST_REGISTRY", getenv)?;
        self.require_value("GST_REGISTRY_REUSE_PLUGIN_SCANNER", "no", getenv)?;

        let registry_path = self.environment_path("GST_REGISTRY_1_0", getenv)?;
        if registry_path.file_name() != Some(OsStr::new("gstreamer-1.0.registry")) {
            return Err(format!(
                "vendored-runtime: GST_REGISTRY_1_0 must end in gstreamer-1.0.registry, got {}",
                registry_path.display()
            ));
        }
        match fs::symlink_metadata(&registry_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "vendored-runtime: GST_REGISTRY_1_0 is a symbolic link: {}",
                    registry_path.display()
                ));
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(format!(
                    "vendored-runtime: GST_REGISTRY_1_0 is not a regular file: {}",
                    registry_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "vendored-runtime: GST_REGISTRY_1_0 cannot be inspected ({}): {error}",
                    registry_path.display()
                ));
            }
        }
        let registry_parent = registry_path.parent().ok_or_else(|| {
            "vendored-runtime: GST_REGISTRY_1_0 has no parent directory".to_string()
        })?;
        let registry_parent = fs::canonicalize(registry_parent).map_err(|error| {
            format!("vendored-runtime: GST_REGISTRY_1_0 parent is unavailable: {error}")
        })?;
        if !registry_parent.is_dir() {
            return Err(format!(
                "vendored-runtime: GST_REGISTRY_1_0 parent is not a directory: {}",
                registry_parent.display()
            ));
        }
        if registry_parent.starts_with(&self.vendor_dir) {
            return Err(format!(
                "vendored-runtime: GST_REGISTRY_1_0 points inside the runtime bundle: {}",
                registry_path.display()
            ));
        }
        let cache_root = self.environment_path("XDG_CACHE_HOME", getenv)?;
        let cache_root = fs::canonicalize(&cache_root).map_err(|error| {
            format!(
                "vendored-runtime: XDG_CACHE_HOME is unavailable ({}): {error}",
                cache_root.display()
            )
        })?;
        if !cache_root.is_dir() {
            return Err(format!(
                "vendored-runtime: XDG_CACHE_HOME is not a directory: {}",
                cache_root.display()
            ));
        }
        if cache_root.starts_with(&self.vendor_dir) || !registry_parent.starts_with(&cache_root) {
            return Err(format!(
                "vendored-runtime: registry is outside XDG_CACHE_HOME: {}",
                registry_path.display()
            ));
        }
        Ok(())
    }

    fn environment_path(
        &self,
        name: &str,
        getenv: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, String> {
        let value = getenv(name).ok_or_else(|| {
            format!("vendored-runtime: {name} is not established by the launcher")
        })?;
        if value.as_os_str().is_empty() {
            return Err(format!("vendored-runtime: {name} is empty"));
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(format!(
                "vendored-runtime: {name} is not absolute: {}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn require_path(
        &self,
        name: &str,
        expected: &Path,
        getenv: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<(), String> {
        let actual = self.environment_path(name, getenv)?;
        let actual = fs::canonicalize(&actual).map_err(|error| {
            format!(
                "vendored-runtime: {name} path is unavailable ({}): {error}",
                actual.display()
            )
        })?;
        let expected = fs::canonicalize(expected).map_err(|error| {
            format!(
                "vendored-runtime: expected {name} path is unavailable ({}): {error}",
                expected.display()
            )
        })?;
        if actual != expected {
            return Err(format!(
                "vendored-runtime: {name} is {}, expected {}",
                actual.display(),
                expected.display()
            ));
        }
        Ok(())
    }

    fn require_empty(
        &self,
        name: &str,
        getenv: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<(), String> {
        match getenv(name) {
            Some(value) if value.is_empty() => Ok(()),
            Some(value) => Err(format!(
                "vendored-runtime: {name} must be empty, got {value:?}"
            )),
            None => Err(format!(
                "vendored-runtime: {name} is not established by the launcher"
            )),
        }
    }

    fn require_value(
        &self,
        name: &str,
        expected: &str,
        getenv: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<(), String> {
        match getenv(name) {
            Some(value) if value.as_os_str() == OsStr::new(expected) => Ok(()),
            Some(value) => Err(format!(
                "vendored-runtime: {name} must be {expected:?}, got {value:?}"
            )),
            None => Err(format!(
                "vendored-runtime: {name} is not established by the launcher"
            )),
        }
    }
}

/// Validate the complete private runtime established by the bundled launcher.
///
/// A missing bundle or an environment that was not established by the launcher
/// is an error; callers must surface it as `VideoError::DecoderInit`.
pub fn validate() -> Result<(), String> {
    let paths = locate_runtime_paths()?;
    paths.validate()?;
    let getenv = |name: &str| env::var_os(name);
    paths.validate_environment(&getenv)?;

    info!(
        "vendored-runtime: validated private GStreamer bundle from {}",
        paths.vendor_dir.display()
    );
    debug!(
        plugin_path = %paths.plugin_dir.display(),
        registry = ?env::var_os("GST_REGISTRY_1_0"),
        scanner = %paths.scanner_path.display(),
        launcher = %paths.launcher_path.display(),
        "vendored-runtime: launcher contract validated"
    );
    Ok(())
}

fn locate_runtime_paths() -> Result<RuntimePaths, String> {
    let exe_path = env::current_exe()
        .map_err(|error| format!("vendored-runtime: failed to get executable path: {error}"))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| "vendored-runtime: executable has no parent directory".to_string())?;
    let exe_dir = fs::canonicalize(exe_dir).map_err(|error| {
        format!("vendored-runtime: executable directory is unavailable: {error}")
    })?;

    let candidates = [
        exe_dir.join("vendor/linux-x86_64"),
        exe_dir.join("../vendor/linux-x86_64"),
        exe_dir.join("../../vendor/linux-x86_64"),
        PathBuf::from("/usr/share/lumina-video/vendor/linux-x86_64"),
    ];
    for candidate in &candidates {
        let Ok(vendor_dir) = fs::canonicalize(candidate) else {
            continue;
        };
        let paths = RuntimePaths::new(vendor_dir);
        if paths.validate().is_ok() {
            return Ok(paths);
        }
    }
    Err(format!(
        "vendored-runtime: private bundle missing or incomplete; searched {:?}",
        candidates
    ))
}

fn is_non_empty_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn validates_real_contract_without_process_environment_mutation() -> Result<(), String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "lumina-gstreamer-runtime-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).map_err(|error| format!("create test root: {error}"))?;
        let _cleanup = TestDir(root.clone());

        let vendor_dir = root.join("vendor/linux-x86_64");
        let lib_dir = vendor_dir.join("lib");
        let plugin_dir = lib_dir.join("gstreamer-1.0");
        let scanner_path = vendor_dir.join("libexec/gstreamer-1.0/gst-plugin-scanner");
        let launcher_path = vendor_dir.join("bin/lumina-gstreamer-runtime");
        let cache_root = root.join("cache");
        let cache_dir = cache_root.join("lumina-video/gstreamer-1.0");
        fs::create_dir_all(&plugin_dir).map_err(|error| format!("create plugin dir: {error}"))?;
        fs::create_dir_all(
            scanner_path
                .parent()
                .ok_or_else(|| "scanner parent".to_string())?,
        )
        .map_err(|error| format!("create scanner dir: {error}"))?;
        fs::create_dir_all(
            launcher_path
                .parent()
                .ok_or_else(|| "launcher parent".to_string())?,
        )
        .map_err(|error| format!("create launcher dir: {error}"))?;
        fs::create_dir_all(&cache_dir).map_err(|error| format!("create cache dir: {error}"))?;
        fs::create_dir_all(vendor_dir.join("registry"))
            .map_err(|error| format!("create bundle registry dir: {error}"))?;
        fs::write(lib_dir.join("libgstreamer-1.0.so.0"), [])
            .map_err(|error| format!("write library marker: {error}"))?;
        fs::write(plugin_dir.join("libgstcoreelements.so"), [])
            .map_err(|error| format!("write plugin marker: {error}"))?;
        fs::write(&scanner_path, []).map_err(|error| format!("write scanner marker: {error}"))?;
        fs::write(&launcher_path, []).map_err(|error| format!("write launcher marker: {error}"))?;

        let registry_path = cache_dir.join("gstreamer-1.0.registry");
        let mut values = HashMap::from([
            ("GST_PLUGIN_PATH_1_0", plugin_dir.clone().into_os_string()),
            ("GST_PLUGIN_SYSTEM_PATH_1_0", OsString::new()),
            ("GST_PLUGIN_PATH", OsString::new()),
            ("GST_PLUGIN_SYSTEM_PATH", OsString::new()),
            (
                "GST_PLUGIN_SCANNER_1_0",
                scanner_path.clone().into_os_string(),
            ),
            ("GST_PLUGIN_SCANNER", OsString::new()),
            ("LD_LIBRARY_PATH", lib_dir.clone().into_os_string()),
            ("GST_REGISTRY", OsString::new()),
            ("GST_REGISTRY_REUSE_PLUGIN_SCANNER", OsString::from("no")),
            ("GST_REGISTRY_1_0", registry_path.into_os_string()),
            ("XDG_CACHE_HOME", cache_root.clone().into_os_string()),
        ]);
        let paths = RuntimePaths::new(vendor_dir.clone());
        paths.validate()?;
        {
            let getenv = |name: &str| values.get(name).cloned();
            paths.validate_environment(&getenv)?;
        }

        let marker_path = vendor_dir.join("registry/marker");
        fs::write(&marker_path, []).map_err(|error| format!("write registry marker: {error}"))?;
        symlink(&marker_path, &cache_dir.join("gstreamer-1.0.registry"))
            .map_err(|error| format!("create registry symlink: {error}"))?;
        let error = {
            let getenv = |name: &str| values.get(name).cloned();
            paths
                .validate_environment(&getenv)
                .err()
                .ok_or_else(|| "registry symlink was accepted".to_string())?
        };
        if !error.contains("symbolic link") {
            return Err(format!("unexpected registry symlink error: {error}"));
        }
        fs::remove_file(cache_dir.join("gstreamer-1.0.registry"))
            .map_err(|error| format!("remove registry symlink: {error}"))?;
        values.insert(
            "GST_REGISTRY_1_0",
            vendor_dir
                .join("registry/gstreamer-1.0.registry")
                .into_os_string(),
        );
        let error = {
            let getenv = |name: &str| values.get(name).cloned();
            paths
                .validate_environment(&getenv)
                .err()
                .ok_or_else(|| "bundle-local registry was accepted".to_string())?
        };
        if !error.contains("inside the runtime bundle") {
            return Err(format!("unexpected bundle-local registry error: {error}"));
        }
        Ok(())
    }
}
