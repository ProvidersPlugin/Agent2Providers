use agent2api_wasm_sdk::{json, state::Snapshot, Error, Result, Value};

use crate::{account, endpoints, login_state as state, network, util};

pub fn form(value: &Value) -> Result<Value> {
    let phase = value["phase"].as_str().unwrap_or("");
    let step = value["step"]
        .as_str()
        .ok_or_else(|| Error::http(409, "科搭登录步骤无效"))?;
    let texts = match value["locale"].as_str().unwrap_or("zh-CN") {
        "en" => ["Sign in to Codar with WeChat", "Scan the QR code with WeChat and confirm on your phone. Phone binding or new-IP verification appears here when required.", "Bind a phone and sign in to Codar", "Verify the Codar sign-in IP", "Codar SMS sign-in", "11-digit mainland China phone number", "SMS code (4–8 digits)", "Enter the linked phone number ending in ", " and complete the CAPTCHA before sending SMS.", "Complete the CAPTCHA, then continue to send SMS. Nothing is sent automatically or twice.", "SMS was sent to ", ". Enter the code from this attempt."],
        "zh-Hant" => ["微信掃碼登入科搭", "請用微信掃描 QR 碼並在手機上確認。需要綁定手機或驗證新 IP 時會自動顯示下一步。", "綁定手機號並登入科搭", "驗證科搭登入 IP", "科搭手機驗證碼登入", "11 位中國大陸手機號", "簡訊驗證碼（4–8 位數字）", "請填寫帳號綁定的手機號（尾號 ", "），完成人機驗證後傳送簡訊。", "完成人機驗證後按繼續傳送簡訊；不會自動或重複傳送。", "簡訊已傳送至 ", "，請輸入本次驗證碼。"],
        "ja" => ["WeChat で Codar にログイン", "WeChat で QR コードを読み取り、スマートフォンで確認してください。電話番号の登録や新しい IP の確認が必要な場合は次の手順が表示されます。", "電話番号を登録して Codar にログイン", "Codar のログイン IP を確認", "Codar SMS ログイン", "中国本土の11桁の電話番号", "SMS コード（4～8桁）", "末尾が ", " の登録済み電話番号を入力し、CAPTCHA を完了して SMS を送信してください。", "CAPTCHA を完了して続行すると SMS が送信されます。自動送信や重複送信はしません。", "SMS の送信先は ", " です。今回のコードを入力してください。"],
        "ko" => ["WeChat으로 Codar 로그인", "WeChat으로 QR 코드를 스캔하고 휴대전화에서 확인하세요. 전화번호 연결 또는 새 IP 확인이 필요하면 다음 단계가 표시됩니다.", "전화번호 연결 후 Codar 로그인", "Codar 로그인 IP 확인", "Codar SMS 로그인", "중국 본토 11자리 휴대전화 번호", "SMS 코드(4~8자리)", "끝자리가 ", "인 연결된 번호를 입력하고 CAPTCHA를 완료한 뒤 SMS를 보내세요.", "CAPTCHA를 완료하고 계속하면 SMS가 발송됩니다. 자동 또는 중복 발송하지 않습니다.", "SMS 발송 번호: ", ". 이번 인증 코드를 입력하세요."],
        "pt-BR" => ["Entrar no Codar pelo WeChat", "Leia o QR com o WeChat e confirme no celular. A vinculação do telefone ou verificação de novo IP aparece aqui quando necessária.", "Vincular telefone e entrar no Codar", "Verificar IP de login do Codar", "Login do Codar por SMS", "Telefone da China continental com 11 dígitos", "Código SMS (4–8 dígitos)", "Digite o telefone vinculado que termina em ", " e conclua o CAPTCHA antes de enviar o SMS.", "Conclua o CAPTCHA e continue para enviar o SMS. Nada é enviado automaticamente ou em duplicidade.", "SMS enviado para ", ". Digite o código desta tentativa."],
        _ => ["微信扫码登录科搭", "请用微信扫描二维码并在手机上确认。需要绑定手机或验证新 IP 时会自动显示下一步。", "绑定手机号并登录科搭", "验证科搭登录 IP", "科搭手机验证码登录", "11 位中国大陆手机号", "短信验证码（4–8 位数字）", "请填写账号绑定的手机号（尾号 ", "），完成人机验证后发送短信。", "完成人机验证后点击继续发送短信；不会自动发送或重复发送。", "短信已发送至 ", "，请输入本次验证码。"],
    };
    if phase == "qr" {
        return Ok(json!({ "id": step, "title": texts[0], "message": texts[1],
            "fields": [], "captcha": null, "qr": { "url": value["qrUrl"], "expiresAt": value["deadline"] } }));
    }
    let send = phase == "phone";
    let purpose = value["purpose"].as_str().unwrap_or("login");
    let title = match purpose {
        "bind_phone" => texts[2],
        "ip_verify" => texts[3],
        _ => texts[4],
    };
    let mut fields = Vec::new();
    if send {
        fields.push(json!({ "key": "phone", "label": texts[5], "type": "text", "secret": false, "optional": false, "options": [] }));
    } else {
        fields.push(json!({ "key": "code", "label": texts[6], "type": "text", "secret": true, "optional": false, "options": [] }));
    }
    let hint = util::text(value, &["hint"]);
    let tail = util::text(value, &["phoneTail"]);
    let message = if !hint.is_empty() {
        hint
    } else if send && !tail.is_empty() {
        format!("{}{tail}{}", texts[7], texts[8])
    } else if send {
        texts[9].into()
    } else {
        format!(
            "{}{}{}",
            texts[10],
            value["phoneMasked"].as_str().unwrap_or("***"),
            texts[11]
        )
    };
    Ok(
        json!({ "id": step, "title": title, "message": message, "fields": fields,
        "captcha": if send { json!({ "engine": "alicom4", "captchaId": endpoints::CAPTCHA_ID }) } else { Value::Null } }),
    )
}

fn phone(raw: &str) -> Result<String> {
    let digits = raw.trim();
    if digits.len() == 11
        && digits.starts_with('1')
        && digits
            .as_bytes()
            .get(1)
            .is_some_and(|digit| (b'3'..=b'9').contains(digit))
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(digits.to_string())
    } else {
        Err(Error::http(400, "请填写 11 位中国大陆手机号"))
    }
}

fn captcha(value: &Value) -> Result<Value> {
    let mut output = json!({});
    for key in ["lot_number", "captcha_output", "pass_token", "gen_time"] {
        let value = value[key]
            .as_str()
            .map(str::trim)
            .filter(|value| {
                !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control)
            })
            .ok_or_else(|| Error::http(400, "请先完成本步骤的人机验证"))?;
        output[key] = json!(value);
    }
    Ok(output)
}

pub fn send(old: &Snapshot, params: &Value) -> Result<Value> {
    let phone = phone(params["inputs"]["phone"].as_str().unwrap_or(""))?;
    let tail = util::text(&old.value, &["phoneTail"]);
    if !tail.is_empty() && !phone.ends_with(&tail) {
        return Err(Error::http(400, "手机号与科搭账号绑定尾号不一致"));
    }
    if old.value["resendAt"].as_i64().unwrap_or(0) > util::now()? {
        return Err(Error::http(429, "短信发送过于频繁，请稍后重试"));
    }
    let purpose = old.value["purpose"].as_str().unwrap_or("login");
    if !matches!(purpose, "login" | "bind_phone" | "ip_verify") {
        return Err(Error::http(409, "科搭短信验证用途无效"));
    }
    let mut body = captcha(&params["captcha"])?;
    let submitted = body.to_string();
    if old.value["captchaFingerprint"] == util::digest(&submitted) {
        return Err(Error::http(409, "此人机验证结果已经提交，请重新完成验证"));
    }
    body["phone"] = json!(phone);
    body["name"] = json!("user");
    body["type"] = json!(purpose);
    let reserved = state::reserve(old, "sending")?;
    let reply = network::request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::SMS,
        None,
        Some(&body),
        12_000,
    )
    .and_then(|reply| reply.checked(&[phone.clone()]));
    state::unchanged(&reserved)?;
    let mut next = old.value.clone();
    next["step"] = json!(util::nonce()?);
    next["resendAt"] = json!(util::now()?.saturating_add(60_000));
    next["captchaFingerprint"] = json!(util::digest(&submitted));
    match reply {
        Ok(_) => {
            next["phase"] = json!("code");
            next["phone"] = json!(phone);
            next["phoneMasked"] = json!(format!("{}****{}", &phone[..3], &phone[7..]));
            next["codeUntil"] = json!(util::now()?.saturating_add(300_000));
            next["hint"] = Value::Null;
        }
        Err(error) => {
            next["phase"] = json!("phone");
            let template = match old.value["locale"].as_str().unwrap_or("zh-CN") {
                "en" => "SMS sending was not confirmed: {error}. Wait at least 60 seconds before another manual attempt.",
                "zh-Hant" => "簡訊傳送未確認：{error}。請等待至少 60 秒後再手動嘗試。",
                "ja" => "SMS の送信を確認できませんでした：{error}。手動で再試行する前に60秒以上お待ちください。",
                "ko" => "SMS 발송을 확인하지 못했습니다: {error}. 60초 이상 기다린 뒤 수동으로 다시 시도하세요.",
                "pt-BR" => "O envio do SMS não foi confirmado: {error}. Aguarde pelo menos 60 segundos antes de tentar manualmente.",
                _ => "短信发送未确认：{error}。请等待至少 60 秒后再手动尝试。",
            };
            next["hint"] = json!(template.replace("{error}", &error.message));
        }
    }
    state::save(&reserved, next.clone())?;
    Ok(json!({ "state": "pending", "interaction": form(&next)? }))
}

pub fn require_phone(old: &Snapshot, reply: &Value) -> Result<Option<Value>> {
    let data = network::data(reply);
    let status = util::text(data, &["status"]);
    let status = if status.is_empty() {
        util::text(reply, &["status"])
    } else {
        status
    }
    .to_ascii_lowercase();
    let purpose = if status == "bind_phone_required" {
        "bind_phone"
    } else if status == "ip_verify_required" || util::number(&reply["code"]) == Some(201.0) {
        "ip_verify"
    } else {
        return Ok(None);
    };
    if state::load()?.revision != old.revision {
        return Err(Error::http(409, "科搭登录步骤已变化，请重新读取"));
    }
    let temp = util::text(data, &["token"]);
    let temp = if temp.is_empty() {
        util::text(reply, &["token"])
    } else {
        temp
    };
    let username = util::text(data, &["username"]);
    let username = if username.is_empty() {
        util::text(reply, &["username"])
    } else {
        username
    };
    if temp.is_empty() || username.is_empty() {
        return Err(Error::http(502, "科搭安全验证缺少临时令牌或账号名"));
    }
    crate::credentials::token(&temp)?;
    let mut next = old.value.clone();
    if let Some(fields) = next.as_object_mut() {
        // 新 IP 验证是新的短信步骤，不能沿用上一步的手机号、验证码期限或发送节流。
        for key in [
            "phone",
            "phoneMasked",
            "codeUntil",
            "resendAt",
            "captchaFingerprint",
            "inFlightUntil",
            "marker",
        ] {
            fields.remove(key);
        }
    }
    next["phase"] = json!("phone");
    next["purpose"] = json!(purpose);
    next["step"] = json!(util::nonce()?);
    next["tempToken"] = json!(temp);
    next["username"] = json!(username);
    next["phoneTail"] = if purpose == "ip_verify" {
        let raw = util::text(data, &["phone"]);
        let raw = if raw.is_empty() {
            util::text(reply, &["phone"])
        } else {
            raw
        };
        let digits = raw
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        if !digits.is_empty()
            && !old.value["phone"].as_str().unwrap_or("").ends_with(&digits)
            && old.value["methodIndex"] == 1
        {
            return Err(Error::http(
                409,
                "科搭短信登录账号与原手机号不一致，未继续验证",
            ));
        }
        json!(digits)
    } else {
        json!("")
    };
    next["hint"] = Value::Null;
    state::save(old, next.clone())?;
    Ok(Some(
        json!({ "state": "pending", "interaction": form(&next)? }),
    ))
}

pub fn verify(old: &Snapshot, params: &Value) -> Result<Value> {
    let code = params["inputs"]["code"].as_str().unwrap_or("").trim();
    if !(4..=8).contains(&code.len()) || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::http(400, "请填写 4–8 位数字短信验证码"));
    }
    if old.value["codeUntil"].as_i64().unwrap_or(0) <= util::now()? {
        return Err(Error::http(409, "短信验证码已过期，请重新发起登录"));
    }
    let phone = phone(old.value["phone"].as_str().unwrap_or(""))?;
    let purpose = old.value["purpose"].as_str().unwrap_or("login");
    let reserved = state::reserve(old, "verifying")?;
    let temp = old.value["tempToken"].as_str().unwrap_or("");
    let username = old.value["username"].as_str().unwrap_or("");
    let (path, token, body) = match purpose {
        "login" => (
            endpoints::PHONE,
            None,
            json!({ "username": phone, "password": code }),
        ),
        "ip_verify" => (
            endpoints::VERIFY_IP,
            Some(temp),
            json!({ "username": username, "phone": phone, "code": code }),
        ),
        "bind_phone" => (
            endpoints::BIND_PHONE,
            None,
            json!({ "username": username, "phone": phone, "code": code }),
        ),
        _ => return Err(Error::http(409, "科搭验证用途无效")),
    };
    let reply = match network::request("POST", endpoints::ACCOUNT, path, token, Some(&body), 12_000)
    {
        Ok(reply) => reply,
        Err(error) => {
            return failure(
                &reserved,
                &format!("本次验证结果未确认，未自动重复提交：{}", error.message),
            )
        }
    };
    state::unchanged(&reserved)?;
    if let Some(next) = require_phone(&reserved, &reply.body)? {
        return Ok(next);
    }
    let reply = match reply.checked(&[phone, code.to_string(), temp.to_string()]) {
        Ok(reply) => reply,
        Err(error) => return failure(&reserved, &error.message),
    };
    let raw = util::text(network::data(&reply), &["token"]);
    let raw = if raw.is_empty() {
        util::text(&reply, &["token"])
    } else {
        raw
    };
    let raw = if raw.is_empty() && purpose == "bind_phone" {
        temp.to_string()
    } else {
        raw
    };
    let origin = if old.value["methodIndex"] == 1 {
        "sms"
    } else {
        "wechat"
    };
    let normalized = match account::from_token(&raw, old.value["name"].as_str(), origin) {
        Ok(normalized) => normalized,
        Err(error) => return failure(&reserved, &error.message),
    };
    let expected_user = util::text(&old.value, &["username"]);
    if !expected_user.is_empty() && normalized["secret"]["username"] != expected_user {
        return failure(&reserved, "科搭验证结果与原扫码或短信账号不一致，未保存");
    }
    state::unchanged(&reserved)?;
    state::save(
        &reserved,
        json!({ "phase": "done", "deadline": old.value["deadline"], "account": normalized }),
    )?;
    Ok(agent2api_wasm_sdk::login::done(normalized))
}

pub fn failure(old: &Snapshot, message: &str) -> Result<Value> {
    let protected = [old.value["tempToken"].as_str().unwrap_or("").to_string()];
    let message = util::safe_text(message, &protected, 600);
    state::save(
        old,
        json!({ "phase": "failed", "deadline": old.value["deadline"], "message": message }),
    )?;
    Ok(json!({ "state": "failed", "message": message }))
}
