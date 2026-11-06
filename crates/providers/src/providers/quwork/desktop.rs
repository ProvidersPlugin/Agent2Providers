//! 本机账号只保存引用；不改 QuWork 的数据库、登录状态或 DSH 凭证文件。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

use super::{accounts, client};
use crate::errors::GatewayError;

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn data_root() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("qurwork-desktop"))
    }
    #[cfg(target_os = "macos")]
    {
        home().map(|path| path.join("Library/Application Support/qurwork-desktop"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|path| path.join(".config")))
            .map(|path| path.join("qurwork-desktop"))
    }
}

fn read_database(path: &Path) -> Result<Option<Value>, GatewayError> {
    let database = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| {
        GatewayError::with_status(
            400,
            "无法只读打开 QuWork 登录数据库，请确认当前系统用户已登录 QuWork",
        )
    })?;
    database
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| GatewayError::with_status(400, "QuWork 登录数据库正忙，请稍后重试"))?;
    let row: Option<String> = database.query_row(
        "SELECT session_json FROM auth_session WHERE id = 1 AND schema_version = 2 AND length(session_json) <= 262144",
        [], |row| row.get(0),
    ).optional().map_err(|_| GatewayError::with_status(400, "QuWork 登录数据库格式不兼容"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&row).map_err(|_| {
        GatewayError::with_status(400, "QuWork 登录记录格式不正确，请重新登录客户端")
    })?;
    let mut secret = accounts::parse(&value)?;
    if secret.get("expiresAt").is_none() {
        if let Some(updated) = client::timestamp(value.get("updatedAt")) {
            secret["expiresAt"] = json!(updated.saturating_add(2 * 60 * 60_000));
        }
    }
    Ok(Some(secret))
}

fn read_local() -> Result<Value, GatewayError> {
    if let Some(root) = data_root() {
        let path = root.join("qurwork-desktop.sqlite");
        if path.is_file() {
            let mut secret = read_database(&path)?.ok_or_else(|| {
                GatewayError::with_status(401, "QuWork 当前未登录，请先在官方客户端登录后再导入")
            })?;
            secret["origin"] = json!(path.to_string_lossy());
            return Ok(secret);
        }
    }
    // 仅在没有会话数据库时接受独立的 DSH 模型凭证，不以旧凭证绕过客户端退出登录。
    if let Some(path) = home().map(|path| path.join(".qurwork-desktop-dsh/.dsh/.credentials.yaml"))
    {
        if path.is_file() {
            let size = std::fs::metadata(&path)
                .map_err(|_| GatewayError::with_status(400, "QuWork 凭证文件无法读取"))?
                .len();
            if size > accounts::MAX_CREDENTIAL_BYTES as u64 {
                return Err(GatewayError::with_status(
                    400,
                    "QuWork 凭证文件超过大小限制",
                ));
            }
            let content = std::fs::read_to_string(&path)
                .map_err(|_| GatewayError::with_status(400, "QuWork 凭证文件无法读取"))?;
            let mut secret = accounts::parse_text(&content)?;
            secret["origin"] = json!(path.to_string_lossy());
            return Ok(secret);
        }
    }
    Err(GatewayError::with_status(
        400,
        "未找到 QuWork 本机登录态，请先在运行网关的电脑上登录 QuWork；远程部署可粘贴凭证 JSON",
    ))
}

pub async fn current(reference: Option<&Value>) -> Result<Value, GatewayError> {
    let secret = tokio::task::spawn_blocking(read_local)
        .await
        .map_err(|_| GatewayError::with_status(500, "读取 QuWork 本机登录态失败"))??;
    if let Some(reference) = reference {
        if accounts::identity(&secret) != accounts::identity(reference) {
            return Err(GatewayError::with_status(
                409,
                "QuWork 客户端已切换账号，请重新导入；不会将原账号自动替换成其他账号",
            ));
        }
    }
    Ok(secret)
}

pub async fn device_identifier() -> Result<String, GatewayError> {
    tokio::task::spawn_blocking(|| {
        let path = home()
            .map(|path| path.join(".qurwork-desktop-dsh/.user-device/device-identity.json"))
            .ok_or_else(|| {
                GatewayError::with_status(
                    400,
                    "未找到 QuWork 本机设备标识，请到官方客户端完成权益兑换",
                )
            })?;
        let bytes = std::fs::read(&path).map_err(|_| {
            GatewayError::with_status(
                400,
                "未找到 QuWork 本机设备标识，请到官方客户端完成权益兑换",
            )
        })?;
        if bytes.len() > 16_384 {
            return Err(GatewayError::with_status(
                400,
                "QuWork 设备标识文件格式异常",
            ));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| GatewayError::with_status(400, "QuWork 设备标识文件格式异常"))?;
        let identifier = accounts::text(&value, &["identifier"]);
        if identifier.len() != 64 || !identifier.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GatewayError::with_status(
                400,
                "QuWork 设备标识文件格式异常",
            ));
        }
        Ok(identifier)
    })
    .await
    .map_err(|_| GatewayError::with_status(500, "读取 QuWork 设备标识失败"))?
}

pub async fn reference() -> Result<Value, GatewayError> {
    let secret = current(None).await?;
    let llm = accounts::text(&secret, &["llmToken"]);
    let public = accounts::public(&secret);
    Ok(json!({
        "source": "desktop",
        "user": secret.get("user"),
        "modelFingerprint": crate::flight::fingerprint(&llm),
        "tokenTail": public.get("tokenTail"),
        "hasLoginToken": !accounts::text(&secret, &["accessToken"]).is_empty(),
        "expiresAt": secret.get("expiresAt"),
        "origin": secret.get("origin"),
    }))
}
