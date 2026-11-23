//! 固定本机来源只经宿主读取，不允许插件指定路径、SQL 或命令。
//! CLI 换证与客户端写回不属于只读关联范围。

use agent2api_wasm_sdk::{json, Value};

pub fn read() -> agent2api_wasm_sdk::Result<crate::credentials::Credentials> {
    let value = agent2api_wasm_sdk::native_call("desktop.credentials.kiro.v1", "read", json!({}))?;
    let mut credentials = crate::credentials::Credentials::parse(&value)?;
    if !credentials.local() || credentials.access.is_empty() {
        return Err(crate::credentials::local_unavailable());
    }
    if credentials.profile.is_empty() {
        match crate::refresh::lookup_profile(&credentials) {
            Ok(profile) => credentials.profile = profile,
            Err(error) if crate::errors::host_stop(&error) => return Err(error),
            _ => {}
        }
    }
    Ok(credentials)
}

pub fn status() -> Value {
    json!({ "available": true, "mode": "read_only",
        "message": "固定读取官方登录文件；另行授权后可通过宿主续期并安全写回，不运行 CLI",
        "sources": [
            { "id": "kiro-cli", "format": "sqlite", "table": "auth_kv",
                "paths": { "win": "%APPDATA%/kiro-cli/data.sqlite3",
                    "mac": "~/Library/Application Support/kiro-cli/data.sqlite3",
                    "linux": "$XDG_DATA_HOME/kiro-cli/data.sqlite3（默认 ~/.local/share）" },
                "keys": ["kirocli:social:token", "kirocli:odic:token", "kirocli:external-idp:token", "kirocli:odic:device-registration"],
                "stateKeys": ["auth.idc.region", "api.codewhisperer.profile"] },
            { "id": "kiro-ide", "format": "json", "path": "~/.aws/sso/cache/kiro-auth-token.json",
                "companion": "同目录内经文件名约束的clientIdHash.json" }
        ],
        "systemDecryption": false,
        "minimumProjection": ["source", "method", "accessToken", "refreshToken", "expiresAt",
            "region", "profileArn", "clientId", "clientSecret", "tokenURL", "loginProvider"],
        "identityRule": "新读出的凭据需向管理面验证实际邮箱并绑定；重新读取必须检查身份未变，不能以渠道uid或旧快照代替",
        "ownershipRule": "只读关联与续期授权分开；不执行 CLI，只有额外确认后才消费当前账号刷新凭据并安全写回" })
}
