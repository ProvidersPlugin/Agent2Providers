use std::collections::BTreeMap;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::api;
use crate::errors::GatewayError;

const MAX_IMAGE: usize = 20 * 1024 * 1024;

fn unsupported(message: &str) -> GatewayError {
    GatewayError::with_status(400, message).with_code("unsupported_parameter")
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], value: &str) -> Result<Vec<u8>, GatewayError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| GatewayError::with_status(500, "图片上传签名失败"))?;
    mac.update(value.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, GatewayError> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, format!("豆包工作图片上传缺少 {key}")))
}

fn image_sign(
    auth: &Value,
    method: &str,
    host: &str,
    query: &BTreeMap<&str, String>,
    body: &str,
    region: &str,
    time: &str,
) -> Result<(String, String), GatewayError> {
    let access = required(auth, "access_key")?;
    let secret = required(auth, "secret_key")?;
    let token = required(auth, "session_token")?;
    let date = time
        .get(..8)
        .ok_or_else(|| GatewayError::with_status(500, "图片上传时间无效"))?;
    let query = query
        .iter()
        .map(|(key, value)| format!("{}={}", api::encode(key), api::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let headers = format!("host:{host}\nx-amz-date:{time}\nx-amz-security-token:{token}\n");
    let names = "host;x-amz-date;x-amz-security-token";
    let canonical = format!(
        "{method}\n/\n{query}\n{headers}\n{names}\n{}",
        hex(&Sha256::digest(body))
    );
    let scope = format!("{date}/{region}/imagex/aws4_request");
    let string = format!(
        "AWS4-HMAC-SHA256\n{time}\n{scope}\n{}",
        hex(&Sha256::digest(canonical))
    );
    let key = hmac(
        &hmac(
            &hmac(&hmac(format!("AWS4{secret}").as_bytes(), date)?, region)?,
            "imagex",
        )?,
        "aws4_request",
    )?;
    Ok((
        query,
        format!(
            "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={names}, Signature={}",
            hex(&hmac(&key, &string)?)
        ),
    ))
}

fn upload_base(host: &str) -> Result<String, GatewayError> {
    let url = url::Url::parse(&format!("https://{host}/"))
        .map_err(|_| GatewayError::with_status(502, "豆包工作图片上传域名无效"))?;
    if url.host_str() != Some(host)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || ![
            ".bytedanceapi.com",
            ".bytevcloudapi.com",
            ".volcengineapi.com",
            ".byteimg.com",
            ".ibyteimg.com",
            ".ibytedtos.com",
            ".imagex.cn",
            ".douyinpic.com",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return Err(GatewayError::with_status(
            502,
            "豆包工作返回了非官方图片上传域名",
        ));
    }
    Ok(format!("https://{host}"))
}

fn valid_uri(uri: &str) -> bool {
    !uri.is_empty()
        && !uri.starts_with('/')
        && !uri.contains(['?', '#', '\\'])
        && !uri
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
        && !uri.bytes().any(|byte| byte.is_ascii_control())
}

pub async fn image(
    client: &reqwest::Client,
    credentials: &Value,
    data: &str,
) -> Result<Value, GatewayError> {
    let (prefix, encoded) = data
        .split_once(',')
        .ok_or_else(|| unsupported("无效的图片 data URL"))?;
    if !prefix.starts_with("data:image/")
        || !prefix.ends_with(";base64")
        || encoded.len() > MAX_IMAGE * 4 / 3 + 4
    {
        return Err(unsupported("图片 data URL 格式无效或超过 20 MiB"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| unsupported("无效的图片 base64"))?;
    if bytes.len() > MAX_IMAGE {
        return Err(unsupported("输入图片超过 20 MiB"));
    }
    let format = image::guess_format(&bytes).map_err(|_| unsupported("输入不是有效图片"))?;
    let extension = match format {
        image::ImageFormat::Png => ".png",
        image::ImageFormat::Jpeg => ".jpg",
        image::ImageFormat::WebP => ".webp",
        _ => return Err(unsupported("豆包工作图片输入仅支持 PNG、JPEG 和 WebP")),
    };
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| unsupported("输入不是有效图片"))?
        .into_dimensions()
        .map_err(|_| unsupported("无法读取图片尺寸"))?;
    let prepared = api::post(
        client,
        credentials,
        "/alice/resource/prepare_upload",
        &json!({"tenant_id":"5","scene_id":"4","resource_type":2}),
    )
    .await?;
    let prep = &prepared["data"];
    let host = required(prep, "upload_host")?;
    let endpoint = upload_base(host)?;
    let auth = &prep["upload_auth_token"];
    let token = required(auth, "session_token")?;
    let service = required(prep, "service_id")?;
    let region = prep["region"].as_str().unwrap_or("cn-north-1");
    let time = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let query = BTreeMap::from([
        ("Action", "ApplyImageUpload".into()),
        ("Version", "2018-08-01".into()),
        ("ServiceId", service.into()),
        ("FileSize", bytes.len().to_string()),
        ("FileExtension", extension.into()),
    ]);
    let (query, signature) = image_sign(auth, "GET", host, &query, "", region, &time)?;
    let applied = api::json_response(
        client
            .get(format!("{endpoint}/?{query}"))
            .header("x-amz-date", &time)
            .header("x-amz-security-token", token)
            .header("Authorization", signature),
        false,
    )
    .await?;
    let address = &applied["Result"]["UploadAddress"];
    let store = &address["StoreInfos"][0];
    let upload_host = address["UploadHosts"][0]
        .as_str()
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作未返回图片上传地址"))?;
    let uri = required(store, "StoreUri")?;
    if !valid_uri(uri) {
        return Err(GatewayError::with_status(502, "豆包工作图片上传路径无效"));
    }
    let authorization = required(store, "Auth")?;
    let mut crc = flate2::Crc::new();
    crc.update(&bytes);
    let response = client
        .post(format!("{}/upload/v1/{uri}", upload_base(upload_host)?))
        .header("Authorization", authorization)
        .header("Content-CRC32", format!("{:08x}", crc.sum()))
        .header("Content-Type", "application/octet-stream")
        .body(bytes)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let uploaded: Value = response
        .json()
        .await
        .map_err(|_| GatewayError::with_status(502, "豆包工作图片上传响应格式无效"))?;
    if !(200..300).contains(&status) || api::number(&uploaded["code"]) != Some(2000) {
        return Err(api::error(status, &uploaded));
    }
    let session = required(address, "SessionKey")?;
    let body = json!({"SessionKey":session}).to_string();
    let time = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let query = BTreeMap::from([
        ("Action", "CommitImageUpload".into()),
        ("Version", "2018-08-01".into()),
        ("ServiceId", service.into()),
    ]);
    let (query, signature) = image_sign(auth, "POST", host, &query, &body, region, &time)?;
    let committed = api::json_response(
        client
            .post(format!("{endpoint}/?{query}"))
            .header("x-amz-date", &time)
            .header("x-amz-security-token", token)
            .header("Authorization", signature)
            .header("Content-Type", "application/json")
            .body(body),
        false,
    )
    .await?;
    let result = &committed["Result"]["Results"][0];
    if api::number(&result["UriStatus"]) != Some(2000) {
        return Err(GatewayError::with_status(502, "豆包工作未确认图片上传完成"));
    }
    let uri = required(result, "Uri")?;
    Ok(
        json!({"type":1,"identifier":crate::ids::uuid_v4(),"image":{"name":format!("image{extension}"),"uri":uri,"image_ori":{"url":"","width":width,"height":height,"format":"","url_formats":{}}},"parse_state":0,"review_state":1,"upload_status":1,"progress":100,"src":""}),
    )
}
