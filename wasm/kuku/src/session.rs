//! userreport三件套按账号持久化600秒；新实例不依赖静态HashMap。
//! 业务STOKEN落库须账号锁+CAS，冲突回读；本次三件套与后续Cookie必须同一凭据。
//! BDUSS无续期，不伪造refreshToken。无PTOKEN或换发失败可沿用原Cookie，但验证不能跳过。

use crate::{account, credentials::Credentials, endpoints, engine, errors, network, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "kuku.triple";
const TTL: i64 = 600_000;
#[derive(Clone)]
pub struct Triple {
    pub bdstoken: String,
    pub uinfo: String,
    pub uk: String,
}
pub struct Session {
    pub credentials: Credentials,
    pub triple: Triple,
}
fn triple(value: &Value) -> Result<Triple> {
    let bdstoken = util::text(value, &["bdstoken"]);
    let uinfo = util::text(value, &["uinfo"]);
    let uk = value["uk"]
        .as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| value["uk"].as_u64().map(|n| n.to_string()))
        .unwrap_or_default();
    if !util::identifier(&bdstoken, 8192)
        || !util::identifier(&uinfo, 32 * 1024)
        || uk.len() > 480
        || uk.chars().any(char::is_control)
    {
        return Err(Error::http(502, "库库会话三件套格式无效"));
    }
    Ok(Triple {
        bdstoken,
        uinfo,
        uk,
    })
}
impl Triple {
    pub fn query(&self) -> String {
        format!(
            "&bdstoken={}&uinfo={}&uk={}",
            util::query(&self.bdstoken),
            util::query(&self.uinfo),
            util::query(&self.uk)
        )
    }
    fn value(&self) -> Value {
        json!({ "bdstoken": self.bdstoken, "uinfo": self.uinfo, "uk": self.uk })
    }
}
pub fn read() -> Result<(Value, Credentials)> {
    let record = sdk::account()?;
    let secret = record["secret"].clone();
    let c = Credentials::from_secret(&secret)?;
    Ok((secret, c))
}
pub fn unchanged(expected: &Credentials) -> Result<()> {
    let (_, current) = read()?;
    if current.cookie() != expected.cookie() || current.uid != expected.uid {
        return Err(
            Error::http(409, "库库账号在操作期间已更换凭据，未使用旧账号结果")
                .with_code("kuku_account_changed"),
        );
    }
    Ok(())
}

fn report(c: &Credentials) -> Result<Triple> {
    let value = network::get(
        &format!(
            "{}{}?{}",
            endpoints::BASE,
            endpoints::USER_REPORT,
            endpoints::WEB_QUERY
        ),
        network::headers(c),
        &c.secrets(),
    )?;
    errors::errno(&value, &c.secrets())?;
    triple(&value["data"])
}
fn matched(c: &Credentials, triple: &Triple) -> Result<()> {
    if !c.uid.is_empty() && !triple.uk.is_empty() && c.uid != triple.uk {
        return Err(
            Error::http(409, "库库Cookie已对应另一个账号，未替换原身份，请重新添加")
                .with_code("kuku_identity_changed"),
        );
    }
    if !triple.uk.is_empty()
        && c.secrets()
            .iter()
            .any(|secret| !secret.is_empty() && triple.uk.contains(*secret))
    {
        return Err(Error::http(502, "库库返回的账号标识包含凭据信息"));
    }
    Ok(())
}
/// 手工导入即时验证，无已落库账号，不申请账号锁或账号状态。
pub fn verify(mut c: Credentials) -> Result<Credentials> {
    if !c.ptoken().is_empty() {
        match engine::exchange(&c) {
            Ok(stoken) => c.set_stoken(&stoken)?,
            Err(error) if errors::host_stop(&error) => return Err(error),
            Err(_) => {}
        }
    }
    let triple = match report(&c) {
        Ok(triple) => triple,
        Err(original) if original.status == Some(401) && !c.ptoken().is_empty() => {
            let stoken = match engine::exchange(&c) {
                Ok(stoken) => stoken,
                Err(error) if errors::host_stop(&error) => return Err(error),
                Err(_) => return Err(original),
            };
            c.set_stoken(&stoken)?;
            report(&c)?
        }
        Err(error) => return Err(error),
    };
    matched(&c, &triple)?;
    c.uid = triple.uk;
    Ok(c)
}
fn save(before: &Value, c: &Credentials) -> Result<(Value, Credentials)> {
    if before["accessToken"] == c.cookie() {
        return Ok((before.clone(), c.clone()));
    }
    let mut next = before.clone();
    next["accessToken"] = json!(c.cookie());
    if sdk::host(
        "account.update",
        json!({ "expect": before, "secret": next, "public": account::view(&next)? }),
    )?["updated"]
        == true
    {
        return Ok((next, c.clone()));
    }
    let (latest, current) = read()?;
    if current.get("BDUSS") != c.get("BDUSS") || current.uid != c.uid {
        return Err(Error::http(409, "库库账号在换发期间发生变化，请重试"));
    }
    Ok((latest, current))
}
/// 调用方已经持有账号锁，避免签到/目录路径嵌套申请。
pub fn ensure_locked(force: bool) -> Result<Session> {
    let (secret, mut c) = read()?;
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    let now = util::now()?;
    if !force && snapshot.compatible && snapshot.value["binding"] == c.binding() {
        if let Some(at) = snapshot.value["at"].as_i64() {
            if (0..TTL).contains(&now.saturating_sub(at)) {
                let triple = triple(&snapshot.value["triple"])?;
                matched(&c, &triple)?;
                return Ok(Session {
                    credentials: c,
                    triple,
                });
            }
        }
    }
    let triple = match report(&c) {
        Ok(triple) => triple,
        Err(original) if original.status == Some(401) && !c.ptoken().is_empty() => {
            let stoken = match engine::exchange(&c) {
                Ok(stoken) => stoken,
                Err(error) if errors::host_stop(&error) => return Err(error),
                Err(_) => return Err(original),
            };
            c.set_stoken(&stoken)?;
            c = save(&secret, &c)?.1;
            report(&c)?
        }
        Err(error) => return Err(error),
    };
    matched(&c, &triple)?;
    unchanged(&c)?;
    if !state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        snapshot.revision,
        json!({ "binding": c.binding(), "at": util::now()?, "triple": triple.value() }),
        0,
    )? {
        return Err(Error::http(409, "库库会话状态被其他操作更新，请重试"));
    }
    Ok(Session {
        credentials: c,
        triple,
    })
}
pub fn ready(force: bool) -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    ensure_locked(force)
}
/// 旧freepoint直接使用业务STOKEN；不用三件套，但换发仍通过同一CAS边界。
pub fn business_locked() -> Result<Credentials> {
    let (secret, mut c) = read()?;
    if c.ptoken().is_empty() {
        return Ok(c);
    }
    match engine::exchange(&c) {
        Ok(stoken) => {
            c.set_stoken(&stoken)?;
            Ok(save(&secret, &c)?.1)
        }
        Err(error) if errors::host_stop(&error) => Err(error),
        Err(_) => Ok(c),
    }
}
