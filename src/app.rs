// SPDX-License-Identifier: MIT

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// macOS bundle identifier for the Antigravity desktop application.
pub const MACOS_APP_BUNDLE_ID: &str = "com.google.antigravity";

/// Fixed lazy session ID used by the Antigravity desktop app.
pub const APP_SESSION_ID: &str = "app";

/// Display label used by the Antigravity desktop app in `agy-gyro monitor` and logs.
pub const APP_SESSION_LABEL: &str = "Antigravity";

/// Resolved launch mechanism for the Antigravity desktop application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppLaunchTarget {
    /// macOS LaunchServices launch by bundle identifier (`open -b <bundle_id>`).
    MacOsOpenBundleId(String),
    /// macOS LaunchServices launch for an explicit `.app` bundle path (`open -a <path>`).
    MacOsOpenBundlePath(PathBuf),
    /// Direct detached executable launch (Linux, Windows, or non-bundle binary on macOS).
    DirectExecutable(PathBuf),
}

fn trim_trailing_path_separators(raw: &str) -> &str {
    let trimmed = raw.trim();
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    if stripped.is_empty() {
        trimmed
    } else {
        stripped
    }
}

/// Expands a leading `~/` (or `~\`) against the user's home directory so TOML configs like
/// `app_path = "~/Applications/Antigravity.app"` work without shell expansion.
pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    if trimmed == "~"
        && let Some(home) = std::env::home_dir()
    {
        return home;
    }
    PathBuf::from(trimmed)
}

/// Returns `true` if `path` is a regular file with execute permission (on Unix) or a regular file (on Windows).
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Returns `true` if `raw` looks like a macOS `.app` bundle path.
pub fn is_macos_app_bundle_path(raw: &str) -> bool {
    let normalized = trim_trailing_path_separators(raw);
    Path::new(normalized)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

/// Resolves the macOS launch target for an optional configured `app_path`.
pub fn resolve_macos_target(configured: Option<&str>) -> AppLaunchTarget {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        None => AppLaunchTarget::MacOsOpenBundleId(MACOS_APP_BUNDLE_ID.to_string()),
        Some(raw) if is_macos_app_bundle_path(raw) => {
            let trimmed = trim_trailing_path_separators(raw);
            AppLaunchTarget::MacOsOpenBundlePath(expand_tilde(trimmed))
        }
        Some(raw) => AppLaunchTarget::DirectExecutable(expand_tilde(raw)),
    }
}

/// Searches `path_env` (a `PATH`-formatted variable) for any executable file named in `names`.
pub fn find_in_path(names: &[&str], path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        for name in names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Returns the first existing executable file among `candidates`, falling back to `path_names` in `path_env`.
pub fn find_executable_from_candidates(
    candidates: &[PathBuf],
    path_names: &[&str],
    path_env: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(found) = candidates.iter().find(|p| is_executable_file(p)) {
        return Some(found.clone());
    }
    find_in_path(path_names, path_env)
}

#[cfg(target_os = "windows")]
fn windows_default_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local_app_data);
        candidates.push(
            base.join("Programs")
                .join("Antigravity")
                .join("Antigravity.exe"),
        );
        candidates.push(base.join("Antigravity").join("Antigravity.exe"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(
            PathBuf::from(program_files)
                .join("Antigravity")
                .join("Antigravity.exe"),
        );
    }
    candidates
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn linux_default_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/Antigravity/antigravity"),
        PathBuf::from("/usr/local/bin/antigravity"),
        PathBuf::from("/usr/bin/antigravity"),
        PathBuf::from("/usr/lib/antigravity/antigravity"),
    ];
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".local").join("bin").join("antigravity"));
    }
    candidates
}

/// Resolves the platform-appropriate [`AppLaunchTarget`] for launching the Antigravity desktop app.
pub fn resolve_app_target(configured: Option<&str>) -> Result<AppLaunchTarget, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(resolve_macos_target(configured))
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(raw) = configured.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(AppLaunchTarget::DirectExecutable(expand_tilde(raw)));
        }
        let candidates = windows_default_candidates();
        find_executable_from_candidates(
            &candidates,
            &["Antigravity.exe", "antigravity.exe"],
            std::env::var_os("PATH").as_deref(),
        )
        .map(AppLaunchTarget::DirectExecutable)
        .ok_or_else(|| {
            "unable to locate Antigravity desktop app: set AGY_GYRO_APP_PATH or configure `app_path` in ~/.gemini/config/agy-gyro.toml"
                .to_string()
        })
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        if let Some(raw) = configured.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(AppLaunchTarget::DirectExecutable(expand_tilde(raw)));
        }
        let path_env = std::env::var_os("PATH");
        if let Some(found) =
            find_in_path(&["antigravity", "antigravity-desktop"], path_env.as_deref())
        {
            return Ok(AppLaunchTarget::DirectExecutable(found));
        }
        let candidates = linux_default_candidates();
        find_executable_from_candidates(&candidates, &[], None)
            .map(AppLaunchTarget::DirectExecutable)
            .ok_or_else(|| {
                "unable to locate Antigravity desktop app: set AGY_GYRO_APP_PATH or configure `app_path` in ~/.gemini/config/agy-gyro.toml"
                    .to_string()
            })
    }
}

/// Builds argv for macOS `/usr/bin/open` from a macOS launch target, gateway env vars, and passthrough args.
pub fn build_macos_open_args(
    target: &AppLaunchTarget,
    gateway_env: &[(&str, &str)],
    app_args: &[String],
) -> Option<Vec<OsString>> {
    let mut args = Vec::new();
    for (k, v) in gateway_env {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("{}={}", k, v)));
    }

    match target {
        AppLaunchTarget::MacOsOpenBundleId(bundle_id) => {
            args.push(OsString::from("-b"));
            args.push(OsString::from(bundle_id));
        }
        AppLaunchTarget::MacOsOpenBundlePath(path) => {
            args.push(OsString::from("-a"));
            args.push(path.as_os_str().to_os_string());
        }
        AppLaunchTarget::DirectExecutable(_) => return None,
    }

    if !app_args.is_empty() {
        args.push(OsString::from("--args"));
        for arg in app_args {
            args.push(OsString::from(arg));
        }
    }

    Some(args)
}

/// Launches the Antigravity desktop application detached with the given gateway environment variables.
pub fn launch_app(
    target: &AppLaunchTarget,
    gateway_env: &[(&str, &str)],
    app_args: &[String],
) -> Result<i32, Box<dyn std::error::Error>> {
    if let Some(open_args) = build_macos_open_args(target, gateway_env, app_args) {
        let mut cmd = std::process::Command::new("/usr/bin/open");
        cmd.args(&open_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        return match cmd.status() {
            Ok(status) => Ok(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("agy-gyro: failed to execute '/usr/bin/open': {}", err);
                Ok(127)
            }
        };
    }

    let AppLaunchTarget::DirectExecutable(exe) = target else {
        unreachable!("non-open target must be DirectExecutable");
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args(app_args)
        .env_remove("AGY_ENABLE_HUB")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    for (k, v) in gateway_env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    match cmd.spawn() {
        Ok(_child) => Ok(0),
        Err(err) => {
            eprintln!("agy-gyro: failed to launch '{}': {}", exe.display(), err);
            Ok(127)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_macos_target_classification() {
        assert_eq!(
            resolve_macos_target(None),
            AppLaunchTarget::MacOsOpenBundleId(MACOS_APP_BUNDLE_ID.to_string())
        );
        assert_eq!(
            resolve_macos_target(Some("  ")),
            AppLaunchTarget::MacOsOpenBundleId(MACOS_APP_BUNDLE_ID.to_string())
        );
        assert_eq!(
            resolve_macos_target(Some("/Applications/Antigravity.app")),
            AppLaunchTarget::MacOsOpenBundlePath(PathBuf::from("/Applications/Antigravity.app"))
        );
        assert_eq!(
            resolve_macos_target(Some("/Applications/Antigravity.app/")),
            AppLaunchTarget::MacOsOpenBundlePath(PathBuf::from("/Applications/Antigravity.app"))
        );
        assert_eq!(
            resolve_macos_target(Some("/Applications/Antigravity.APP/")),
            AppLaunchTarget::MacOsOpenBundlePath(PathBuf::from("/Applications/Antigravity.APP"))
        );
        assert_eq!(
            resolve_macos_target(Some("/tmp/custom-antigravity")),
            AppLaunchTarget::DirectExecutable(PathBuf::from("/tmp/custom-antigravity"))
        );
    }

    #[test]
    fn test_build_macos_open_args_bundle_id_and_args() {
        let env = [
            ("AGY_LLM_GATEWAY_URL", "http://127.0.0.1:6470"),
            ("AGY_LLM_GATEWAY_WIRE_PROTOCOL", "genai"),
            ("AGY_LLM_GATEWAY_API_KEY", "gyro:app:0:Antigravity"),
        ];

        // Empty app_args omits `--args`
        let target_id = AppLaunchTarget::MacOsOpenBundleId(MACOS_APP_BUNDLE_ID.to_string());
        let args = build_macos_open_args(&target_id, &env, &[]).unwrap();
        let strings: Vec<String> = args
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strings,
            vec![
                "--env",
                "AGY_LLM_GATEWAY_URL=http://127.0.0.1:6470",
                "--env",
                "AGY_LLM_GATEWAY_WIRE_PROTOCOL=genai",
                "--env",
                "AGY_LLM_GATEWAY_API_KEY=gyro:app:0:Antigravity",
                "-b",
                "com.google.antigravity",
            ]
        );

        // Non-empty app_args appends `--args` followed by passthrough args
        let target_path =
            AppLaunchTarget::MacOsOpenBundlePath(PathBuf::from("/Applications/Antigravity.app"));
        let passthrough = vec!["--headless".to_string(), "--ssh-host=dev".to_string()];
        let args = build_macos_open_args(&target_path, &env, &passthrough).unwrap();
        let strings: Vec<String> = args
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strings,
            vec![
                "--env",
                "AGY_LLM_GATEWAY_URL=http://127.0.0.1:6470",
                "--env",
                "AGY_LLM_GATEWAY_WIRE_PROTOCOL=genai",
                "--env",
                "AGY_LLM_GATEWAY_API_KEY=gyro:app:0:Antigravity",
                "-a",
                "/Applications/Antigravity.app",
                "--args",
                "--headless",
                "--ssh-host=dev",
            ]
        );

        // DirectExecutable returns None
        let direct = AppLaunchTarget::DirectExecutable(PathBuf::from("/usr/bin/antigravity"));
        assert!(build_macos_open_args(&direct, &env, &[]).is_none());
    }

    #[test]
    fn test_expand_tilde() {
        let home = std::env::home_dir().expect("home dir should resolve");
        assert_eq!(
            expand_tilde("~/Applications/Antigravity.app"),
            home.join("Applications/Antigravity.app")
        );
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(
            expand_tilde("/Applications/Antigravity.app"),
            PathBuf::from("/Applications/Antigravity.app")
        );

        assert_eq!(
            resolve_macos_target(Some("~/Applications/Antigravity.app/")),
            AppLaunchTarget::MacOsOpenBundlePath(home.join("Applications/Antigravity.app"))
        );
    }

    #[test]
    fn test_find_executable_from_candidates_and_path() {
        let base = std::env::temp_dir().join(format!(
            "agy-gyro-app-discover-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let dir_a = base.join("a");
        let dir_b = base.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        #[cfg(unix)]
        fn mark_executable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(not(unix))]
        fn mark_executable(_path: &Path) {}

        // On Unix, a non-executable file earlier in PATH must be skipped
        let non_exec_a = dir_a.join("antigravity");
        std::fs::write(&non_exec_a, b"not executable").unwrap();

        let bin_b = dir_b.join("antigravity");
        std::fs::write(&bin_b, b"mock").unwrap();
        mark_executable(&bin_b);

        let path_env = std::env::join_paths([&dir_a, &dir_b]).unwrap();

        // Missing candidate falls back to PATH lookup (skipping non-executable file on Unix)
        let found = find_executable_from_candidates(
            &[dir_a.join("missing")],
            &["antigravity"],
            Some(path_env.as_os_str()),
        );
        #[cfg(unix)]
        assert_eq!(found, Some(bin_b.clone()));
        #[cfg(not(unix))]
        assert_eq!(found, Some(non_exec_a));

        // Existing executable candidate takes priority over PATH
        let bin_a = dir_a.join("explicit");
        std::fs::write(&bin_a, b"mock").unwrap();
        mark_executable(&bin_a);
        let found = find_executable_from_candidates(
            std::slice::from_ref(&bin_a),
            &["antigravity"],
            Some(path_env.as_os_str()),
        );
        assert_eq!(found, Some(bin_a));

        // Neither candidate nor PATH exists -> None
        let none = find_executable_from_candidates(
            &[dir_a.join("nope")],
            &["nonexistent-binary"],
            Some(path_env.as_os_str()),
        );
        assert_eq!(none, None);

        let _ = std::fs::remove_dir_all(&base);
    }
}
