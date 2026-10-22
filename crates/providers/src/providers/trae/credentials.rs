//! Trae 凭据（`auth.Auth` 的 Rust 对应物）。
//!
//! ── 哪些是长期的、哪些是短期的 ────────────────────────────
//! `accessToken` 是 **Cloud-IDE-JWT**，约 14 天到期；`refreshToken` 长期但
//! **每次 ExchangeToken 都轮换**（一次性）。所以"能不能续"取决于 refreshToken
//! 在不在，而"要不要续"取决于 accessToken 的到期时刻 —— 到期时刻优先信
//! JWT 自己的 `exp`，落盘字段只作兜底（参考实现两处都读，见 `expires_at_ms`）。
//!
//! 另外三个字段是**设备指纹**：`machineId` / `deviceId` 与登录时上传的公钥
//! 一起被服务端绑定（`CheckLogin` 会回 `BoundDeviceID` / `DeviceBindStatus`）。
//! 它们必须与凭据同生共死：换掉它们等于换了一台设备，社区明确把
//! "每次随机 deviceId" 标成风控隐患。
//!
//! `variant` 决定 ClientID / 平台码 / 目录血统，缺省按 `solo`
//! （本仓只服务 SOLO 通道，见 `mod.rs` 模块头）。

use serde_json::Value;

pub const DEFAULT_VARIANT: &str = "solo";

/// 一条 Trae 凭据。字段名与落盘形态一致（驼峰），因为账号存储就是按
/// 参考实现的 auth 文件形状读写（嵌套 `{type, provider, auth{…}, account{…}}`
/// 与平铺两种都认，见 `from_payload`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    /// accessToken 的到期时刻（Unix 秒或毫秒都接受，读的时候归一，见 `expires_at_ms`）。
    pub expires_at: i64,
    pub domain: String,
    /// OAuth/积分那条链的 host（`ExchangeToken` 的第一候选）。
    pub api_host: String,
    pub machine_id: String,
    pub device_id: String,
    pub variant: String,
    pub uid: String,
    pub enterprise_id: String,
    pub nickname: String,
    /// 登录时那套设备密钥的**公钥**（SPKI PEM，随 ExchangeToken 上传过的那份）。
    ///
    /// 落盘它不是为了再用它发请求，而是为了让"这台设备绑定过哪个账号"这件事
    /// 在凭据里可读回来 —— 服务端按 DevicePublicKey + DeviceID/MachineID 做绑定，
    /// 出问题时（20405 那族）第一件事就是要能看见当时上传的是哪把公钥。
    pub device_public_key: String,
    /// 登录时那套设备密钥的私钥（PKCS#8 PEM）。
    ///
    /// 参考实现**只写不读**（全目录 grep `devicePrivateKey` 只有写入与
    /// "重建时不许丢"的夹具测试）。这里同样只存不用，但必须存：
    /// 写回凭据时把它抹掉，就等于把这台设备的绑定关系弄断了。
    pub device_private_key: String,
    /// 这张 refreshToken **当初是在哪个 OAuth 应用里 mint 的**（上游在
    /// `ExchangeToken` 响应里回显 `Result.ClientID`，CPA 的 auth 文件里同名
    /// 字段叫 `authClientId`）。
    ///
    /// ── 为什么它必须落盘，而不是按 variant 现推 ──────────────
    /// `variant` 是**转发面**的谱系（本家只服务 SOLO 通道），`ClientID` 是
    /// **凭据的归属**。两者可以不一致：生产实测（2026-10-02，NAS 三条从 CPA
    /// 迁来的账号）库里 `variant=solo`，而 CPA 盘上的 `authClientId` 与当年
    /// 换证响应的 `Result.ClientID` 都是非 solo 的 `ono9krqynydwx5`。按
    /// variant 现推就会拿 solo 的 `en1oxy7wnw8j9n` 去续期，上游回
    /// `400 10101 "refresh token is not matched to the client"` —— 三条账号
    /// 从第一次进入续期窗口起就续不上（假令牌的对照回的是
    /// `401 20101 "refresh token is invalid"`，两种失败不同形，见 `refresh`）。
    /// 空串 = 不知道（老数据 / 手工粘贴没带），那时才回退到按 variant 推。
    pub auth_client_id: String,
}

/// 国际版谱系（`intl` / `solo-intl`）。
///
/// 本家**只接了国内 SOLO 那一条通道**：`trae-api-cn.mchost.guru` 上的
/// `llm_utils_chat` + `solo_work_lite`，目录与积分两条链也都是 CN 域名。
/// 国际版走的是另一套两步协议（`core-normal.trae.ai/api/remote/v1/chat_sessions`
/// → `GET …/{id}/events`，`Origin`/`Referer` 必须是 `work.trae.ai`，流还是
/// 累积式要还原增量），没实现之前这类凭据**不能落账号** —— 收下它只会在
/// 第一次转发时收到一句来自 CN host 的 401，用户完全看不出是谱系不对。
/// 判据本身是大小写无关的（`Credential::variant()` 归一后才走到闸门，
/// 但这条函数也可能被别处直接拿去问原始值，所以不依赖调用前先归一）。
///
/// 反向验证：本家两处闸门（落账号与转发）都是拿**归一后**的值问它，
/// 因此"未知值兜到 solo"这条老规则不会被误伤成"Intl 被放行"。
pub fn is_intl_variant(variant: &str) -> bool {
    matches!(variant.trim().to_lowercase().as_str(), "intl" | "solo-intl")
}

/// 拒绝 Intl 谱系时给的那句理由（三处入口共用一份文案，别说三种话）。
pub const INTL_UNSUPPORTED: &str = "Trae 国际版（intl / solo-intl）暂未接入：本家只支持国内 SOLO 通道，国际版是另一套两步协议（chat_sessions → events）。请在 trae.cn 登录后再添加，或等 Intl 通道接入；国内 `cn` 与 `solo` 两个谱系在转发面等价（都发 solo_work_lite），不在此列。";

impl Credential {
    pub fn valid(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    /// 上游 `function` 与 ClientID 都按 variant 选，未知值兜到 solo。
    pub fn variant(&self) -> &str {
        match self.variant.as_str() {
            "cn" | "solo" | "intl" | "solo-intl" => &self.variant,
            _ => DEFAULT_VARIANT,
        }
    }

    /// 到期时刻统一成**毫秒**。
    ///
    /// 上游与各家客户端在秒与毫秒之间来回漂过（参考实现为此留了
    /// `normalizeExpiresAt`），所以按数量级判：小于 1e11 的当秒用
    /// （1e11 秒 ≈ 公元 5138 年，真毫秒值不可能小于它）。
    pub fn expires_at_ms(&self) -> i64 {
        if self.expires_at <= 0 {
            return 0;
        }
        if self.expires_at < 100_000_000_000 {
            self.expires_at * 1000
        } else {
            self.expires_at
        }
    }

    /// JWT 里的到期时刻（毫秒），解不出来返回 None。
    ///
    /// 只看第二段、不验签：这不是安全判定，只是"还有多久要续"的读数来源。
    /// 验签要公钥，而我们对上游的令牌格式没有信任需求 —— 判错过一次
    /// 也只是早续或晚续一次。
    ///
    /// ── 为什么除 `exp` 之外还认 `TokenExpireAt` ────────────────
    /// 中国版的令牌把到期写在**自定义声明** `TokenExpireAt` 里而不是标准 `exp`
    /// （参考实现 Trae2api-cn 从 JWT payload 读的也是它，见 issue #263）；
    /// 旧解析只认 `exp`，中国版的令牌解出来是 None ⇒ 有效期落到落盘字段，
    /// 而落盘那份在换证时同样是坏的（见 `refresh::parse_refresh_response` ——
    /// 中国版的 `TokenExpireAt` 是**字符串**，旧解析读成 0）。两层都没有的账号
    /// 面板上的有效期就不准，续期节奏也跟着错。
    ///
    /// 声明值可能是数字也可能是字符串，秒与毫秒两种量级都在（`"1900000000000"`
    /// 是毫秒、`1780000000` 是秒），按 [`crate::server::core::providers::trae::refresh::normalize_expires_at`]
    /// 同一判据归一。
    pub fn jwt_expires_at_ms(&self) -> Option<i64> {
        let from_claim = |claim: &str| {
            jwt_claim(&self.access_token, claim).map(|raw| {
                if raw < 100_000_000_000 {
                    raw.saturating_mul(1000)
                } else {
                    raw
                }
            })
        };
        from_claim("exp")
            .or_else(|| from_claim("TokenExpireAt"))
            .or_else(|| from_claim("tokenExpireAt"))
    }

    /// JWT 里的 `iat`（秒）。参考实现额外用它在签发龄超 15 天时强制轮换
    /// （服务端有吊销旧凭据的风险），所以这个读数不是装饰。
    pub fn jwt_issued_at_seconds(&self) -> Option<i64> {
        jwt_claim(&self.access_token, "iat")
    }

    /// 有效到期时刻：JWT 声明优先（`exp`，缺席时退中国版自定义的 `TokenExpireAt`），
    /// 落盘字段兜底。
    pub fn effective_expiry_ms(&self) -> i64 {
        self.jwt_expires_at_ms().unwrap_or_else(|| self.expires_at_ms())
    }

    /// 是否需要续期：临期（`lead` 之内）**或**签发龄超过 `max_age`。
    ///
    /// 两个条件都要：只看临期的话，一个刚签发但服务端已吊销的令牌会被
    /// 当成"还早"，等到真用它时才 401（那时 refreshToken 可能也已经换过一轮）。
    pub fn needs_refresh(&self, lead_ms: i64, max_issue_age_ms: i64, now_ms: i64) -> bool {
        let expiry = self.effective_expiry_ms();
        if expiry > 0 && expiry - now_ms <= lead_ms {
            return true;
        }
        if max_issue_age_ms <= 0 {
            return false;
        }
        self.jwt_issued_at_seconds()
            .map(|issued| now_ms.saturating_sub(issued * 1000) >= max_issue_age_ms)
            .unwrap_or(false)
    }

    /// 从账号记录里读凭据：嵌套（CPA auth 文件形状）与平铺都认。
    ///
    /// 只认驼峰键（与参考实现落盘一致）。看到 `access_key_id` 那一类
    /// 蛇形键**直接报错**而不是当空凭据收下 —— 那是别家的形状，
    /// 静默接受会让一个明显有数据的账号显示成"缺凭据"。
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let auth = payload.get("auth").filter(|value| value.is_object()).unwrap_or(payload);
        let account = payload.get("account").filter(|value| value.is_object());
        let text = text_of;
        let number = |object: &Value, key: &str| -> i64 {
            object
                .get(key)
                .and_then(Value::as_f64)
                .map(|value| value as i64)
                .unwrap_or(0)
        };
        // OAuth 归属（哪把 ClientID mint 的这张 refreshToken）。四种写法都认：
        // 本家与 CPA 落盘是 `authClientId`，上游响应回显是 `ClientID`，
        // 手工粘贴可能给小写 `clientId`；嵌套 `auth{}` 与平铺 payload 两个位置都找。
        let auth_client_id = [
            text(auth, "authClientId"),
            text(auth, "ClientID"),
            text(auth, "clientId"),
            text(payload, "authClientId"),
        ]
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default();
        let credential = Self {
            access_token: text(auth, "accessToken"),
            refresh_token: text(auth, "refreshToken"),
            expires_at: number(auth, "expiresAt"),
            domain: text(auth, "domain"),
            api_host: text(auth, "apiHost"),
            machine_id: text(auth, "machineId"),
            device_id: text(auth, "deviceId"),
            variant: text(auth, "variant"),
            uid: text_either(account, auth, "uid"),
            enterprise_id: text_either(account, auth, "enterpriseId"),
            nickname: text_either(account, auth, "nickname"),
            device_public_key: text(auth, "devicePublicKey"),
            device_private_key: text(auth, "devicePrivateKey"),
            auth_client_id,
        };
        if credential.access_token.trim().is_empty() && credential.refresh_token.trim().is_empty() {
            return Err("Trae 凭据里既没有 accessToken 也没有 refreshToken".to_string());
        }
        Ok(credential)
    }

    /// 写回账号记录用的那部分字段（**只并这些键**，不重建整条记录）。
    ///
    /// 参考实现里出过事故的那条：面板签到前的预刷新走了"重建"路径，
    /// refreshToken 保住了但设备密钥与一堆对拍扩展字段被抹掉。
    /// 所以这里给的是"字段补丁"，落盘侧只做覆盖写。
    pub fn patch_fields(&self) -> Vec<(&'static str, Value)> {
        let mut fields = vec![
            ("accessToken", json_or_empty(&self.access_token)),
            ("refreshToken", json_or_empty(&self.refresh_token)),
            // 落盘**统一成毫秒**。本家收到的值在秒与毫秒之间漂过：CPA 的 auth
            // 文件与手工粘贴是秒（`expiresAt: 1791009732`），上游刷新响应是毫秒。
            // 存进去是什么单位，决定了面板怎么读它 —— 有效期那一列与别家共用
            // 同一套按毫秒的读法，存秒的结果是"1970 年到期"，账号一进面板就红着
            // 显示「已过期」。读取侧本来就有 `expires_at_ms()` 归一（两种都认），
            // 所以这里写毫秒不会把单位读反，只是让**落盘形状与别家一致**。
            ("expiresAt", Value::from(self.expires_at_ms())),
            ("domain", json_or_empty(&self.domain)),
            ("apiHost", json_or_empty(&self.api_host)),
            ("machineId", json_or_empty(&self.machine_id)),
            ("deviceId", json_or_empty(&self.device_id)),
            ("variant", json_or_empty(self.variant())),
        ];
        if !self.device_public_key.is_empty() {
            fields.push(("devicePublicKey", Value::String(self.device_public_key.clone())));
        }
        if !self.device_private_key.is_empty() {
            fields.push(("devicePrivateKey", Value::String(self.device_private_key.clone())));
        }
        // 只在知道的时候写：空串落盘会把 CPA 那边迁来的 `authClientId` 抹掉，
        // 而那个值一旦没了，续期就又只能按 variant 猜（猜错就是今天的 10101）
        if !self.auth_client_id.is_empty() {
            fields.push(("authClientId", Value::String(self.auth_client_id.clone())));
        }
        fields
    }
}

fn json_or_empty(value: &str) -> Value {
    Value::String(value.to_string())
}

/// `account` 段优先，取不到（或为空）再回落到 `auth` 段。
///
/// 参考实现把身份放在 `account{uid,nickname,enterpriseId}`，而手工粘贴的
/// 凭据常常是平铺的 —— 两个位置都读一次，才不会让"明明填了昵称却显示空"。
fn text_either(account: Option<&Value>, auth: &Value, key: &str) -> String {
    let from_account = account.map(|object| text_of(object, key)).unwrap_or_default();
    if from_account.is_empty() { text_of(auth, key) } else { from_account }
}

fn text_of(object: &Value, key: &str) -> String {
    object
        .get(key)
        .map(|value| match value {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// 取 JWT 的一个数字声明（不验签）。
fn jwt_claim(token: &str, claim: &str) -> Option<i64> {
    let mut parts = token.trim().split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload.trim_end_matches('='),
    )
    .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    // 声明值数字与**字符串数字**都认：中国版把 `TokenExpireAt` 放进 payload 时
    // 是字符串形态（参考实现的夹具原样，见 `jwt_expires_at_ms` 的函数头）。
    match value.get(claim)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .map(|number| number as i64)
}

