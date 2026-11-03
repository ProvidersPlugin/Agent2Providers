//! 官方 1.1.5 同时保留旧 `.box-agent` 与带客户端标记的独立 profile。

use std::path::{Path, PathBuf};

const PROFILE_MARKER: &str = ".raccoon-lab-client.json";

pub(super) fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn configured_path(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

fn profile_roots(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut roots = Vec::new();
        if let Some(app_data) = configured_path(&["APPDATA"]) {
            roots.push(app_data.join("office-raccoon"));
        }
        roots.push(home.join(".office-raccoon"));
        roots
    }
    #[cfg(target_os = "macos")]
    {
        vec![home.join("Library/Application Support/office-raccoon")]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        vec![configured_path(&["XDG_CONFIG_HOME"])
            .unwrap_or_else(|| home.join(".config"))
            .join("office-raccoon")]
    }
}

fn marked_profile(root: &Path) -> bool {
    let path = root.join(PROFILE_MARKER);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
        return false;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|marker| {
            marker
                .get("application")
                .and_then(serde_json::Value::as_str)
                == Some("@raccoon/office-desktop")
                && marker.get("version").and_then(serde_json::Value::as_i64) == Some(1)
        })
}

pub(super) fn desktop_auth_file() -> PathBuf {
    if let Some(path) = configured_path(&["RACCOON_BOX_AGENT_CONFIG_DIR", "BOX_AGENT_CONFIG_DIR"]) {
        return path.join("auth.json");
    }
    if let Some(path) = configured_path(&["BOX_AGENT_HOME"]) {
        return path.join("config/auth.json");
    }
    let home = home_dir();
    let roots = profile_roots(&home);
    // 已有旧版 profile 优先于新版目录；不能误用另一个 profile 的账号。
    if roots
        .iter()
        .take(1)
        .any(|root| root.is_dir() && !root.join(PROFILE_MARKER).exists())
    {
        return home.join(".box-agent/config/auth.json");
    }
    if let Some(root) = roots.iter().find(|root| marked_profile(root)) {
        return root.join("agent/config/auth.json");
    }
    home.join(".box-agent/config/auth.json")
}
