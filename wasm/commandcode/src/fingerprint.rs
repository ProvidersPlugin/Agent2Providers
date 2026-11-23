//! 固定设备档案与确定性指纹来自原生 Command Code 实现；不读取宿主平台或路径。
//! 节流存在宿主的账号隔离临时状态中（重启重报），CAS 租约避免并发重复握手。

use agent2api_wasm_sdk::{self as sdk, http, json, state, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

use super::{
    account,
    config::{Config, DEVICE_ARCH, DEVICE_OS_RELEASE, DEVICE_PLATFORM},
    errors,
};

const FP_SALT: &str = "command-code:device-fingerprint:v1";
const INIT_REFRESH_MS: i64 = 8 * 60 * 60 * 1000;
const INIT_JITTER_MS: i64 = 2 * 60 * 60 * 1000;
const LEASE_MS: i64 = 180_000;
const STATE_TTL: i64 = 24 * 60 * 60 * 1000;
const STATE_SCOPE: &str = "commandcode-device-initialization";
const STATE_KEY: &str = "fingerprint-initialization";

const CPUS: [(&str, i64, i64); 15] = [
    ("12th Gen Intel(R) Core(TM) i7-12650H", 10, 16),
    ("12th Gen Intel(R) Core(TM) i5-12400F", 6, 12),
    ("12th Gen Intel(R) Core(TM) i9-12900K", 16, 24),
    ("13th Gen Intel(R) Core(TM) i7-13700K", 16, 24),
    ("13th Gen Intel(R) Core(TM) i5-13600K", 14, 20),
    ("13th Gen Intel(R) Core(TM) i9-13900K", 24, 32),
    ("Intel(R) Core(TM) Ultra 7 155H", 16, 22),
    ("Intel(R) Core(TM) Ultra 9 285H", 16, 16),
    ("Intel(R) Core(TM) i9-14900K", 24, 32),
    ("Intel(R) Core(TM) i7-14700K", 20, 28),
    ("AMD Ryzen 7 7800X3D", 8, 16),
    ("AMD Ryzen 9 7950X", 16, 32),
    ("AMD Ryzen 5 7600", 6, 12),
    ("AMD Ryzen 9 7900X", 12, 24),
    ("AMD Ryzen 7 5800X3D", 8, 16),
];
const MEMS: [i64; 6] = [8, 16, 24, 32, 48, 64];
const TIMEZONES: [&str; 15] = [
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "America/Toronto",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Moscow",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Asia/Seoul",
    "Asia/Hong_Kong",
    "Australia/Sydney",
    "Pacific/Auckland",
];
const MAC_COUNTS: [i64; 4] = [2, 3, 4, 5];
const OS_USERS: [&str; 6] = ["dev", "user", "admin", "coder", "engineer", "work"];
const MAIL_DOMAINS: [&str; 4] = ["gmail.com", "outlook.com", "qq.com", "163.com"];

fn digest(key: &str, salt: &str, field: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update([0u8]);
    hasher.update(key.as_bytes());
    hasher.update([0u8]);
    hasher.update(field.as_bytes());
    hasher.finalize().into()
}

fn pick(key: &str, salt: &str, field: &str, labels: &[String]) -> usize {
    let mut index = 0;
    let mut best: Option<[u8; 32]> = None;
    for (candidate, label) in labels.iter().enumerate() {
        let score = digest(key, salt, &format!("{field}\0{label}"));
        if best.is_none_or(|best| score > best) {
            index = candidate;
            best = Some(score);
        }
    }
    index
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn signal(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FP_SALT.as_bytes());
    hasher.update([0u8]);
    hasher.update(value.trim().to_lowercase().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn generate(key: &str, salt: &str) -> Value {
    let cpu = CPUS[pick(
        key,
        salt,
        "cpu",
        &CPUS
            .iter()
            .map(|(model, cores, _)| format!("{model}|{cores}"))
            .collect::<Vec<_>>(),
    )];
    let mem = MEMS[pick(
        key,
        salt,
        "mem",
        &MEMS.iter().map(i64::to_string).collect::<Vec<_>>(),
    )];
    let timezone = TIMEZONES[pick(
        key,
        salt,
        "timezone",
        &TIMEZONES
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
    )];
    let mac_count = MAC_COUNTS[pick(
        key,
        salt,
        "macCount",
        &MAC_COUNTS.iter().map(i64::to_string).collect::<Vec<_>>(),
    )];
    let os_user = OS_USERS[pick(
        key,
        salt,
        "osUser",
        &OS_USERS
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
    )];
    let domain = MAIL_DOMAINS[pick(
        key,
        salt,
        "mailDomain",
        &MAIL_DOMAINS
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
    )];
    let field = |name: &str, length: usize| hex(&digest(key, salt, name)[..length]);
    let mid = field("machineId", 16);
    let machine_id = format!(
        "{}-{}-{}-{}-{}",
        &mid[..8],
        &mid[8..12],
        &mid[12..16],
        &mid[16..20],
        &mid[20..]
    );
    let mut macs = (0..mac_count)
        .map(|index| {
            digest(key, salt, &format!("mac{index}"))[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        })
        .collect::<Vec<_>>();
    macs.sort();
    macs.dedup();
    let hostname = format!("DESKTOP-{}", field("hostname", 4).to_uppercase());
    let email = format!("{os_user}.{}@{domain}", field("gitEmail", 3));
    let seed = format!("{machine_id}|{}", macs.join(","));
    let mut hasher = Sha256::new();
    hasher.update(FP_SALT.as_bytes());
    hasher.update(b"\0machine\0");
    hasher.update(seed.as_bytes());
    json!({
        "thumbmark": format!("{:x}", hasher.finalize()),
        "components": {
            "machineIdHash": signal(&machine_id), "osUserHash": signal(os_user),
            "hostnameHash": signal(&hostname), "gitEmailHash": signal(&email),
            "macHashes": macs.iter().map(|mac| signal(mac)).collect::<Vec<_>>(),
            "platform": DEVICE_PLATFORM, "arch": DEVICE_ARCH, "osRelease": DEVICE_OS_RELEASE,
            "cpuModel": cpu.0, "cpuCount": cpu.2, "memGiB": mem,
            "isContainer": false, "timezone": timezone, "runtime": "cli", "collectorVersion": 1
        }
    })
}

pub fn slugify_project_path(path: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in path.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch);
            dash = false;
        } else {
            dash = true;
        }
    }
    if out.is_empty() {
        "root".into()
    } else {
        out
    }
}

fn random_bytes(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("宿主随机数据编码无效"))?;
    if bytes.len() != length {
        return Err(Error::new("宿主随机数据长度无效"));
    }
    Ok(bytes)
}

pub fn random_hex(length: usize) -> Result<String> {
    random_bytes(length).map(|bytes| hex(&bytes))
}

pub fn traceparent() -> Result<String> {
    Ok(format!("00-{}-{}-01", random_hex(16)?, random_hex(8)?))
}

fn jitter() -> Result<i64> {
    let bytes = random_bytes(4)?;
    Ok(
        i64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            % (INIT_JITTER_MS + 1),
    )
}

pub fn ensure_initialized(key: &str, config: &Config) -> Result<()> {
    let record = sdk::account()?;
    if account::api_key(&record["secret"])? != key {
        return Err(
            Error::http(409, "Command Code 账号在初始化前已变化").with_code("account_changed")
        );
    }
    let _guard = sdk::AccountLock::acquire_named_conversation(STATE_SCOPE)?;
    account::unchanged(&record)?;
    let now = sdk::now();
    // 控制面登记用固定账号隔离临时状态，与用户会话及官方持久文件无关。
    let snapshot = state::conversation_get(STATE_SCOPE, STATE_KEY, 1)?;
    let fingerprint = generate(key, &config.fingerprint_salt);
    // 改派生盐或上游地址后必须重新登记；状态只存摘要，不存第二份明文密钥。
    let identity = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{}", config.api_base, fingerprint["thumbmark"]).as_bytes())
    );
    if snapshot.compatible && snapshot.value["identity"] == identity {
        let until = snapshot.value["next_at"].as_i64().unwrap_or(0);
        if until > now {
            return Ok(());
        }
        // 原调用若在持锁期间被取消，租约不表示握手已完成；新调用重做，不能直接生成。
    }
    let lease = random_hex(8)?;
    let lifecycle = json!({ "eventType": "cli_session_exists", "metadata": {
        "sessionId": format!("sess_{}", random_hex(8)?), "cliVersion": config.protocol_version,
        "mode": "interactive", "os": format!("{DEVICE_PLATFORM}-{DEVICE_ARCH}")
    } });
    let delay = INIT_REFRESH_MS.saturating_add(jitter()?);
    if !state::conversation_compare_exchange(
        STATE_SCOPE,
        STATE_KEY,
        1,
        snapshot.revision,
        json!({ "identity": identity, "lease": lease, "lease_until": now + LEASE_MS, "next_at": 0 }),
        STATE_TTL,
    )? {
        return Err(
            Error::http(409, "Command Code 初始化状态已变化，请重新发起")
                .with_code("initialization_conflict"),
        );
    }
    // SDK 同步宿主调用按先后顺序取得完整控制面响应；两条都执行，普通上游失败不阻断生成。
    // 非 2xx 也算完成一次登记尝试；只在运输层失败时保持下次重试，与旧版一致。
    account::unchanged(&record)?;
    let fingerprint_result = http::open_timeout(
        "POST",
        &config.url("/alpha/fingerprint/record"),
        config.headers(key),
        Some(&fingerprint),
        5_000,
    )
    .and_then(|response| response.bytes(1024 * 1024));
    account::unchanged(&record)?;
    let lifecycle_result = http::open_timeout(
        "POST",
        &config.url("/alpha/lifecycle-events"),
        config.headers(key),
        Some(&lifecycle),
        5_000,
    )
    .and_then(|response| response.bytes(1024 * 1024));
    account::unchanged(&record)?;
    let settled = fingerprint_result.is_ok() && lifecycle_result.is_ok();
    let denied = [&fingerprint_result, &lifecycle_result]
        .iter()
        .find_map(|result| match result {
            Err(error)
                if errors::host_failure(error) && error.code.as_deref() != Some("timeout") =>
            {
                Some(error.clone())
            }
            _ => None,
        });
    drop(fingerprint_result);
    drop(lifecycle_result);
    let current = state::conversation_get(STATE_SCOPE, STATE_KEY, 1)?;
    if current.value["lease"] == lease && current.value["identity"] == identity {
        state::conversation_compare_exchange(
            STATE_SCOPE,
            STATE_KEY,
            1,
            current.revision,
            json!({ "identity": identity, "lease_until": 0, "next_at": if settled { sdk::now().saturating_add(delay) } else { 0 } }),
            STATE_TTL,
        )?;
    }
    if let Some(error) = denied {
        return Err(error);
    }
    Ok(())
}
