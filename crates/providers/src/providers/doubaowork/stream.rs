use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use super::api;
use crate::errors::GatewayError;

const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_EVENT: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<(String, String)>, GatewayError> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let separator = [
                self.pending
                    .windows(2)
                    .position(|value| value == b"\n\n")
                    .map(|at| (at, 2)),
                self.pending
                    .windows(4)
                    .position(|value| value == b"\r\n\r\n")
                    .map(|at| (at, 4)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(at, _)| *at);
            let Some((at, length)) = separator else {
                break;
            };
            if at > MAX_EVENT {
                return Err(GatewayError::with_status(502, "豆包工作流式事件过大"));
            }
            let raw = self.pending.drain(..at + length).collect::<Vec<_>>();
            let block = std::str::from_utf8(&raw[..at])
                .map_err(|_| GatewayError::with_status(502, "豆包工作流式事件不是有效 UTF-8"))?;
            let mut event = String::new();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = value.trim_start().to_string();
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if !data.is_empty() {
                frames.push((event, data.join("\n")));
            }
        }
        if self.pending.len() > MAX_EVENT {
            return Err(GatewayError::with_status(502, "豆包工作流式事件过大"));
        }
        Ok(frames)
    }
}

/// 生成结果块的类型。豆包的生图不走独立接口：它就是**对话里的一次任务**
/// （`agent_mode` 的工作任务模式），由 agent 调用 `image_gen` 产出图片，
/// 结果挂在同一条 SSE 流里。实测（2026-11-03，客户端抓包 + 本机复现）两种块：
///   · `10010` `content.gen_image_block.images[]` —— 交付结果，`uri` 给的是
///     **带水印**的压缩预览（`~tplv-…-downsize_watermark…`）；
///   · `10084` `content.rich_media_layout_block.media[].creation.image` —— 画布
///     渲染，这里同时给了 `key`（对象键）与 `image_ori_raw`（**无水印原图**）。
/// 后者总在稍后到达 —— 所以图片**不随块即时发出**：先记下来，流末由
/// [`Decoder::drain_images`] 统一交出去，才交得到无水印那份。
const IMAGE_BLOCK: u64 = 10010;
const MEDIA_BLOCK: u64 = 10084;

/// 流里收到的一张图（同一张图两种块都会报，按对象键合并）。
#[derive(Default)]
struct Image {
    /// 对象键（`tos-cn-i-…/rc_gen_image/x.jpeg`）：两种块都用它标识同一张图。
    key: String,
    /// 无水印原图（`~tplv-…-image_raw.png`）。
    ///
    /// ── 为什么认它、而不去调去水印接口 ──────────────────────────
    /// 实测同一张图：流里这份 `image_raw` 与官方
    /// `/creativity/resource/get_without_watermark` 返回的 `cgen_wm0` **字节完全
    /// 相同**（4624288 字节、同一 md5）。流里既然就有，就不必再发第二次请求。
    raw: Option<String>,
    /// 带水印的预览（`uri` / `thumb_url` / `image_ori`），拿不到原图时的兜底。
    preview: String,
}

/// 图片地址 → 对象键：`https://host/tos-cn-i-…/rc_gen_image/x.jpeg~tplv-…?…`
/// 的对象键是 `~` 之前、去掉协议与 host 的那一段。
fn object_key(url: &str) -> Option<String> {
    let without_query = url.split('?').next().unwrap_or(url);
    let path = without_query.split('~').next().unwrap_or(without_query);
    let key = path
        .strip_prefix("https://")
        .or_else(|| path.strip_prefix("http://"))
        .map(|rest| rest.split_once('/').map(|(_, path)| path).unwrap_or(""))
        .unwrap_or(path);
    (!key.is_empty()).then(|| key.to_string())
}

#[derive(Default)]
pub struct Decoder {
    pub text: String,
    pub reasoning: String,
    reasoning_blocks: BTreeSet<String>,
    blocks: BTreeMap<String, String>,
    /// 生成图片，按到达先后排（一次任务通常一张，流里会重复报同一张）。
    images: Vec<Image>,
    pub done: bool,
    received: usize,
}

#[derive(Clone)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

impl Decoder {
    /// 记下 `10010`（交付结果）里的图片：这一份是**带水印的预览**。
    fn note_generated(&mut self, block: &Value) {
        for image in block["content"]["gen_image_block"]["images"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(url) = ["uri", "thumb_url"].iter().find_map(|key| {
                image[*key]
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
            }) else {
                continue;
            };
            let key = object_key(url).unwrap_or_else(|| url.to_string());
            if let Some(known) = self.images.iter_mut().find(|known| known.key == key) {
                if known.preview.is_empty() {
                    known.preview = url.to_string();
                }
                continue;
            }
            self.images.push(Image {
                key,
                raw: None,
                preview: url.to_string(),
            });
        }
    }

    /// 记下 `10084`（画布渲染）里的图片：这一份带着**无水印原图** [`Image::raw`]。
    ///
    /// 与 `10010` 按对象键合并：先到的是预览、后到的补上原图，两种先后顺序都成立
    /// （实测是 10010 在前，但流里块的先后不由我们定，所以两边都按 key 合并）。
    fn note_media(&mut self, block: &Value) {
        for image in block["content"]["rich_media_layout_block"]["media"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let image = &image["creation"]["image"];
            let key = image["key"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    ["image_ori", "image_preview", "image_thumb"]
                        .iter()
                        .find_map(|field| object_key(image[*field]["url"].as_str()?))
                });
            let Some(key) = key else { continue };
            let raw = image["image_ori_raw"]["url"]
                .as_str()
                .filter(|url| !url.trim().is_empty())
                .map(str::to_string);
            let preview = ["image_ori", "image_preview", "image_thumb"]
                .iter()
                .find_map(|field| image[*field]["url"].as_str())
                .filter(|url| !url.trim().is_empty())
                .map(str::to_string);
            if let Some(known) = self.images.iter_mut().find(|known| known.key == key) {
                if known.raw.is_none() {
                    known.raw = raw;
                }
                if known.preview.is_empty() {
                    known.preview = preview.unwrap_or_default();
                }
                continue;
            }
            self.images.push(Image {
                key,
                raw,
                preview: preview.unwrap_or_default(),
            });
        }
    }

    /// 本次流里有没有出图（`chat::run` 用它决定"只有图片、没有正文"算不算成功）。
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// 把攒下来的图片交出去（流末调用一次）。
    ///
    /// ── 为什么是 markdown，不是图片内容块 ────────────────────────
    /// 网关这条对话流一路上都是**文本增量**（插件只推 `delta.content`，
    /// 下游各家客户端也都按 markdown 渲染）。要塞一个 `image_url` 内容块出去，
    /// 多数客户端会连整条 delta 一起丢掉 —— 图片反而更看不见。渲成
    /// `![](url)` 则所有客户端都能显示，且下游的用量统计/报文记录不用改。
    ///
    /// 一张图只发一次：`10010` 会随进度重复下发同一张，这里按对象键合并过。
    /// 首选无水印原图 [`Image::raw`]，没有才退回带水印的预览（旧账号/旧版本
    /// 可能不下发 `10084`）—— 有图总比没图好，只是带水印。
    pub fn drain_images(&mut self) -> Vec<Delta> {
        let mut output = Vec::new();
        for image in std::mem::take(&mut self.images) {
            let url = match image
                .raw
                .clone()
                .or_else(|| (!image.preview.is_empty()).then(|| image.preview.clone()))
            {
                Some(url) => url,
                None => continue,
            };
            let delta = format!("\n\n![图片]({url})\n\n");
            self.text.push_str(&delta);
            output.push(Delta::Text(delta));
        }
        output
    }

    fn blocks(&mut self, value: &Value, append: bool) -> Result<Vec<Delta>, GatewayError> {
        let mut output = Vec::new();
        for block in value.as_array().into_iter().flatten() {
            let kind = api::number(&block["block_type"]);
            if kind == Some(10040) {
                continue;
            }
            if kind == Some(IMAGE_BLOCK) {
                self.note_generated(block);
                continue;
            }
            if kind == Some(MEDIA_BLOCK) {
                self.note_media(block);
                continue;
            }
            if kind != Some(10000) {
                continue;
            }
            let Some(text) = block["content"]["text_block"]["text"].as_str() else {
                continue;
            };
            let id = api::identifier(&block["block_id"]);
            let icons = ["icon_url", "icon_url_dark"];
            let tagged = icons.iter().any(|key| {
                block["content"]["text_block"][*key]
                    .as_str()
                    .is_some_and(|icon| icon.to_ascii_lowercase().contains("deep_think"))
            });
            if let Some(id) = id.as_ref() {
                if tagged {
                    self.reasoning_blocks.insert(id.clone());
                } else if icons
                    .iter()
                    .any(|key| block["content"]["text_block"].get(*key).is_some())
                {
                    self.reasoning_blocks.remove(id);
                }
            }
            let reasoning = tagged
                || id
                    .as_ref()
                    .is_some_and(|id| self.reasoning_blocks.contains(id));
            let key = format!(
                "{}:{}",
                if reasoning { "reasoning" } else { "text" },
                id.unwrap_or_else(|| "default".into())
            );
            let prior = self.blocks.entry(key).or_default();
            let delta = if append {
                prior.push_str(text);
                text.to_string()
            } else if let Some(suffix) = text.strip_prefix(prior.as_str()) {
                let suffix = suffix.to_string();
                *prior = text.into();
                suffix
            } else if prior.is_empty() {
                *prior = text.into();
                text.to_string()
            } else {
                return Err(GatewayError::with_status(
                    502,
                    "豆包工作替换了已发送的回答，无法安全继续流式响应",
                ));
            };
            if delta.is_empty() {
                continue;
            }
            if reasoning {
                self.reasoning.push_str(&delta);
                output.push(Delta::Reasoning(delta));
            } else {
                self.text.push_str(&delta);
                output.push(Delta::Text(delta));
            }
            if self.text.len() > MAX_OUTPUT || self.reasoning.len() > MAX_OUTPUT {
                return Err(GatewayError::with_status(502, "豆包工作输出超过大小限制"));
            }
        }
        Ok(output)
    }

    fn frame(&mut self, event: &str, raw: &str) -> Result<Vec<Delta>, GatewayError> {
        self.received = self.received.saturating_add(raw.len());
        if self.received > MAX_OUTPUT * 4 {
            return Err(GatewayError::with_status(502, "豆包工作流式响应过大"));
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| GatewayError::with_status(502, "豆包工作流式事件格式无效"))?;
        if event.contains("ERR")
            || event == "gateway-error"
            || event == "error"
            || value
                .get("error")
                .is_some_and(|error| !error.is_null() && error != &Value::Bool(false))
            || api::number(&value["code"]).is_some_and(|code| code != 0)
            || api::number(&value["error_code"]).is_some_and(|code| code != 0)
        {
            return Err(api::error(200, &value));
        }
        if self.done {
            return Ok(Vec::new());
        }
        match event {
            "SSE_REPLY_END" => { self.done = true; Ok(Vec::new()) },
            "STREAM_MSG_NOTIFY" => self.blocks(&value["content"]["content_block"], true),
            "CHUNK_DELTA" if value["text"].is_string() => self.blocks(&json!([{"block_id":"delta","block_type":10000,"content":{"text_block":{"text":value["text"]}}}]), true),
            "FULL_MSG_NOTIFY" if api::number(&value["message"]["user_type"]) == Some(2) => self.blocks(&value["message"]["content_block"], false),
            "STREAM_CHUNK" => {
                let mut deltas = Vec::new();
                for patch in value["patch_op"].as_array().into_iter().flatten() {
                    if api::number(&patch["patch_object"]) != Some(1) { continue; }
                    deltas.extend(self.blocks(&patch["patch_value"]["content_block"], api::number(&patch["patch_type"]) == Some(1))?);
                }
                Ok(deltas)
            }
            _ => Ok(Vec::new()),
        }
    }
}

pub struct Turn {
    source: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    pending: VecDeque<(String, String)>,
    pub decoder: Decoder,
    received: usize,
    deltas: VecDeque<Delta>,
    idle: Duration,
}

impl Turn {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            source: Box::pin(response.bytes_stream()),
            parser: SseParser::default(),
            pending: VecDeque::new(),
            decoder: Decoder::default(),
            received: 0,
            deltas: VecDeque::new(),
            idle: Duration::from_millis(crate::timeouts::current().stream_idle_ms()),
        }
    }

    pub async fn next(&mut self) -> Result<Option<Delta>, GatewayError> {
        loop {
            if let Some(delta) = self.deltas.pop_front() {
                return Ok(Some(delta));
            }
            if let Some((event, data)) = self.pending.pop_front() {
                self.deltas.extend(self.decoder.frame(&event, &data)?);
                continue;
            }
            if self.decoder.done {
                return Ok(None);
            }
            let chunk = tokio::time::timeout(self.idle, self.source.next())
                .await
                .map_err(|_| GatewayError::with_status(504, "豆包工作流式响应空闲超时"))?;
            match chunk {
                Some(Ok(bytes)) => {
                    self.received = self.received.saturating_add(bytes.len());
                    if self.received > MAX_OUTPUT * 4 {
                        return Err(GatewayError::with_status(502, "豆包工作流式响应过大"));
                    }
                    self.pending.extend(self.parser.push(&bytes)?);
                }
                Some(Err(error)) => {
                    return Err(GatewayError::with_status(
                        502,
                        crate::http::describe(&error),
                    ))
                }
                None => {
                    return Err(GatewayError::with_status(
                        502,
                        "豆包工作响应在 SSE_REPLY_END 之前中断",
                    ))
                }
            }
        }
    }
}
