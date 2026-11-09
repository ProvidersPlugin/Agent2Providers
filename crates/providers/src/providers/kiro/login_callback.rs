//! 手工回填与 loopback 回调共用的 URL / HTTP 边界。

use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

const MAX_CALLBACK_BYTES: usize = 16 * 1024;

pub(super) struct Request {
    pub url: Url,
    pub query: HashMap<String, String>,
}

impl Request {
    pub fn parse(raw: &str, port: u16) -> Result<Self, String> {
        if raw.is_empty()
            || raw.len() > MAX_CALLBACK_BYTES
            || raw.bytes().any(|ch| ch <= b' ' || ch == 0x7f)
        {
            return Err("请粘贴本次登录的完整回调地址，不要修改地址或参数".into());
        }
        let authority = raw
            .strip_prefix("http://")
            .and_then(|value| value.split('/').next());
        let localhost = format!("localhost:{port}");
        let loopback = format!("127.0.0.1:{port}");
        if !authority.is_some_and(|host| host == localhost || host == loopback) {
            return Err("这个地址不属于本次 Kiro 登录，请复制刚打开的登录页的完整回调地址".into());
        }
        let url = Url::parse(raw).map_err(|_| "回调地址无效，请复制地址栏中的完整地址")?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
            || url.port() != Some(port)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "/oauth/callback" | "/signin/callback")
        {
            return Err("这个地址不属于本次 Kiro 登录，请复制刚打开的登录页的完整回调地址".into());
        }
        let mut query = HashMap::new();
        for (key, value) in url.query_pairs() {
            if query.insert(key.into_owned(), value.into_owned()).is_some() {
                return Err("回调地址包含重复参数，请重新复制完整地址".into());
            }
        }
        Ok(Self { url, query })
    }

    pub fn value(&self, key: &str) -> &str {
        self.query.get(key).map(String::as_str).unwrap_or("")
    }
}

pub(super) async fn read_request(
    stream: &mut tokio::net::TcpStream,
    port: u16,
) -> Result<Request, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|_| "读取登录回调失败")?;
        if count == 0 {
            return Err("登录回调不完整".into());
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > MAX_CALLBACK_BYTES {
            return Err("登录回调过长".into());
        }
        if buffer.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&buffer).map_err(|_| "登录回调格式无效")?;
    let mut lines = text.split("\r\n");
    let first: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
    if first.len() != 3
        || first[0] != "GET"
        || !first[1].starts_with('/')
        || first[1].starts_with("//")
        || !matches!(first[2], "HTTP/1.0" | "HTTP/1.1")
    {
        return Err("登录回调仅接受 GET 请求".into());
    }
    let mut host = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or("登录回调请求头无效")?;
        let value = value.trim();
        if key.eq_ignore_ascii_case("host") {
            if host.replace(value).is_some() {
                return Err("登录回调主机重复".into());
            }
        } else if key.eq_ignore_ascii_case("origin") {
            let origin = Url::parse(value).map_err(|_| "登录回调来源无效")?;
            if origin.scheme() != "http"
                || !matches!(origin.host_str(), Some("localhost" | "127.0.0.1"))
                || origin.port() != Some(port)
                || !origin.username().is_empty()
                || origin.password().is_some()
            {
                return Err("登录回调来源不匹配".into());
            }
        } else if key.eq_ignore_ascii_case("transfer-encoding")
            || key.eq_ignore_ascii_case("upgrade")
            || (key.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return Err("登录回调不能携带请求正文".into());
        }
    }
    let host = host.ok_or("登录回调缺少主机")?;
    if host != format!("localhost:{port}") && host != format!("127.0.0.1:{port}") {
        return Err("登录回调主机不匹配".into());
    }
    Request::parse(&format!("http://{host}{}", first[1]), port)
}

pub(super) struct Response {
    pub status: u16,
    pub body: String,
    pub location: Option<String>,
}

impl Response {
    pub fn page(status: u16, title: &str, text: &str) -> Self {
        let title = escape_html(title);
        let text = escape_html(text);
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
             <style>body{{font:15px system-ui,sans-serif;display:grid;place-items:center;min-height:90vh;margin:0;color:#222;background:#fafafa}}\
             @media (prefers-color-scheme:dark){{body{{color:#eee;background:#161616}}}}\
             main{{text-align:center;max-width:28rem;padding:1rem}}</style>\
             <main><h1>{title}</h1><p>{text}</p></main>"
        );
        Self {
            status,
            body,
            location: None,
        }
    }

    pub fn redirect(location: String) -> Self {
        Self {
            status: 302,
            body: String::new(),
            location: Some(location),
        }
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) async fn write_response(
    stream: &mut tokio::net::TcpStream,
    response: &Response,
) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        _ => "Bad Request",
    };
    let mut header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        response.status, reason, response.body.len()
    );
    if let Some(location) = &response.location {
        header.push_str(&format!("Location: {location}\r\n"));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(response.body.as_bytes()).await?;
    stream.shutdown().await
}
