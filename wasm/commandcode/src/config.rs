use agent2api_wasm_sdk::{json, settings, Error, Result, Value};

pub const API_BASE: &str = "https://api.commandcode.ai";
pub const PROTOCOL_VERSION: &str = "1.53.1";
pub const DEVICE_PLATFORM: &str = "win32";
pub const DEVICE_ARCH: &str = "x64";
pub const DEVICE_OS_RELEASE: &str = "10.0.22631";
pub const PROJECT_DIR: &str = "C:\\Users\\dev\\projects\\app";

/// 旧版环境覆盖改为宿主保存的插件设置；绝不读取 WASI/进程环境。
/// apiBase仍限制到官方API来源，标准/provider前缀归一后再拼固定路由。
pub struct Config {
    pub api_base: String,
    pub protocol_version: String,
    pub cli_environment: String,
    pub fingerprint_salt: String,
    pub project_dir: String,
}

fn setting(key: &str, fallback: &str, limit: usize) -> Result<String> {
    let value = settings::get(key)?;
    let text = match value {
        Value::Null => fallback.to_string(),
        Value::String(value) if value.trim().is_empty() => fallback.to_string(),
        Value::String(value) => value.trim().to_string(),
        _ => {
            return Err(Error::http(
                400,
                format!("Command Code 设置 {key} 必须是文本"),
            ))
        }
    };
    if text.chars().count() > limit || text.contains(['\0', '\r', '\n']) {
        return Err(Error::http(
            400,
            format!("Command Code 设置 {key} 长度或格式无效"),
        ));
    }
    Ok(text)
}

impl Config {
    pub fn load() -> Result<Self> {
        let api_base = setting("apiBase", API_BASE, 2048)?
            .trim_end_matches('/')
            .to_string();
        let api_base = if matches!(
            api_base.as_str(),
            "https://api.commandcode.ai/provider" | "https://api.commandcode.ai/provider/v1"
        ) {
            API_BASE.to_string()
        } else {
            api_base
        };
        if api_base != API_BASE
            && !api_base.strip_prefix(API_BASE).is_some_and(|path| {
                path.starts_with('/')
                    && !path.contains(['?', '#', '\\'])
                    && !path.chars().any(char::is_whitespace)
            })
        {
            return Err(Error::coded(
                "Command Code 地址超出本插件声明的网络来源",
                "forbidden",
            ));
        }
        let protocol_version = setting("protocolVersion", PROTOCOL_VERSION, 80)?;
        let cli_environment = setting("cliEnvironment", "production", 80)?;
        for value in [&protocol_version, &cli_environment] {
            if !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                return Err(Error::http(400, "Command Code 客户端标识格式无效"));
            }
        }
        Ok(Self {
            api_base,
            protocol_version,
            cli_environment,
            fingerprint_salt: setting("fingerprintSalt", "", 2048)?,
            project_dir: setting("deviceProjectDir", PROJECT_DIR, 1024)?,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    pub fn headers(&self, api_key: &str) -> Value {
        // 逐字沿用 command-code@1.53.1 已实现的 wire 方言，不发送 x-co-flag。
        json!({
            "accept": "application/json", "content-type": "application/json", "user-agent": "cli",
            "x-command-code-version": self.protocol_version,
            "x-cli-environment": self.cli_environment,
            "authorization": format!("Bearer {}", api_key.trim())
        })
    }
}

fn translated(zh: &str, en: &str, hant: &str, ja: &str, ko: &str, pt: &str) -> Value {
    json!({ "zh-CN": zh, "en": en, "zh-Hant": hant, "ja": ja, "ko": ko, "pt-BR": pt })
}

pub fn schema() -> Value {
    let field = |key: &str, label: Value, default: Value, max_length: usize, pattern: Value| {
        json!({
            "key": key, "label": label, "type": "string", "default": default,
            "trim": true, "empty_is_default": true, "max_length": max_length, "pattern": pattern
        })
    };
    json!({
        "version": 1, "title": "Command Code",
        "description": translated(
            "替代旧插件的环境变量覆盖；这些设置不读取本机信息，也不能扩大网络权限。",
            "Replaces legacy environment overrides. Settings do not read local device data or expand network permissions.",
            "取代舊外掛的環境變數覆寫；不讀取本機資訊，也不能擴大網路權限。",
            "従来の環境変数設定を置き換えます。端末情報の読み取りや通信権限の拡張は行いません。",
            "기존 환경 변수 설정을 대체합니다. 로컬 기기 정보를 읽거나 네트워크 권한을 넓히지 않습니다.",
            "Substitui as variáveis de ambiente antigas, sem ler dados locais nem ampliar permissões de rede."
        ),
        "runtime_overrides": false,
        "fields": [
            field("apiBase", translated("接口地址（仅已授权来源）", "API base (authorized origin only)",
                "介面位址（僅已授權來源）", "API URL（許可済み接続先のみ）", "API 주소 (허용된 출처만)",
                "Endereço da API (origem autorizada)"), json!(API_BASE), 2048,
                json!("^https://api\\.commandcode\\.ai(/[^?#\\s]*)?$")),
            field("protocolVersion", translated("客户端协议版本", "Client protocol version", "用戶端協議版本",
                "クライアントのプロトコル版", "클라이언트 프로토콜 버전", "Versão do protocolo do cliente"),
                json!(PROTOCOL_VERSION), 80, json!("^[A-Za-z0-9._-]+$")),
            field("cliEnvironment", translated("客户端环境标识", "Client environment", "用戶端環境標識",
                "クライアント環境", "클라이언트 환경", "Ambiente do cliente"),
                json!("production"), 80, json!("^[A-Za-z0-9._-]+$")),
            field("fingerprintSalt", translated("设备指纹派生盐", "Device fingerprint salt", "裝置指紋衍生鹽",
                "デバイス指紋のソルト", "기기 지문 솔트", "Salt da impressão digital"), Value::Null, 2048, Value::Null),
            field("deviceProjectDir", translated("上报的固定项目目录", "Reported fixed project path", "回報的固定專案目錄",
                "報告する固定プロジェクトパス", "보고할 고정 프로젝트 경로", "Caminho fixo informado"),
                json!(PROJECT_DIR), 1024, Value::Null)
        ]
    })
}
