use std::collections::{BTreeMap, BTreeSet};

use agent2api_wasm_sdk::{Error, Result, Value};

use crate::{endpoints::ReadEndpoint, errors, restriction, util};

const MAX_EVENT: usize = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<(String, String)>> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let boundary = [
                self.pending
                    .windows(2)
                    .position(|bytes| bytes == b"\n\n")
                    .map(|at| (at, 2)),
                self.pending
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .map(|at| (at, 4)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(at, _)| *at);
            let Some((at, len)) = boundary else {
                break;
            };
            if at > MAX_EVENT {
                return Err(errors::protocol("豆包工作流式事件超过 2 MiB"));
            }
            let bytes = self.pending.drain(..at + len).collect::<Vec<_>>();
            let text = std::str::from_utf8(&bytes[..at])
                .map_err(|_| errors::protocol("豆包工作事件不是有效文本"))?;
            let mut event = String::new();
            let mut data = Vec::new();
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = value.trim().to_string();
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if !data.is_empty() {
                frames.push((event, data.join("\n")));
            }
        }
        if self.pending.len() > MAX_EVENT {
            return Err(errors::protocol("豆包工作事件超过大小限制"));
        }
        Ok(frames)
    }

    pub fn finish(&self) -> Result<()> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(errors::protocol("豆包工作流末事件未完整接收"))
        }
    }
}

pub enum Delta {
    Text(String),
    Reasoning(String),
}
struct Image {
    key: String,
    raw: Option<String>,
    preview: Option<String>,
}

#[derive(Default)]
pub struct Decoder {
    pub text: String,
    pub reasoning: String,
    blocks: BTreeMap<String, String>,
    reasoning_blocks: BTreeSet<String>,
    images: Vec<Image>,
    received: usize,
    chunk_channel: BTreeSet<String>,
    compact_channel: BTreeSet<String>,
    active_block: Option<(String, bool)>,
    restriction: Option<Error>,
}

impl Decoder {
    fn image(&mut self, key: String, raw: Option<String>, preview: Option<String>) -> Result<()> {
        if let Some(image) = self.images.iter_mut().find(|image| image.key == key) {
            if raw.is_some() {
                image.raw = raw;
            }
            if preview.is_some() {
                image.preview = preview;
            }
            return Ok(());
        }
        if self.images.len() >= 16 {
            return Err(errors::protocol("豆包工作返回图片过多"));
        }
        self.images.push(Image { key, raw, preview });
        Ok(())
    }

    fn note_images(&mut self, block: &Value) -> Result<()> {
        if util::unsigned(&block["block_type"]) == Some(10010) {
            for image in block["content"]["gen_image_block"]["images"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let preview = ["uri", "thumb_url"]
                    .iter()
                    .find_map(|key| image[*key].as_str())
                    .and_then(safe_image);
                if let Some(url) = preview {
                    self.image(object_key(&url), None, Some(url))?;
                }
            }
        } else {
            for row in block["content"]["rich_media_layout_block"]["media"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let image = &row["creation"]["image"];
                let raw = image["image_ori_raw"]["url"].as_str().and_then(safe_image);
                let preview = ["image_ori", "image_preview", "image_thumb"]
                    .iter()
                    .find_map(|key| image[*key]["url"].as_str())
                    .and_then(safe_image);
                let key = image["key"]
                    .as_str()
                    .filter(|key| !key.is_empty() && key.len() <= 2048)
                    .map(str::to_string)
                    .or_else(|| raw.as_ref().or(preview.as_ref()).map(|url| object_key(url)));
                if let Some(key) = key {
                    self.image(key, raw, preview)?;
                }
            }
        }
        Ok(())
    }

    fn text(
        &mut self,
        key: String,
        text: &str,
        append: bool,
        reasoning: bool,
    ) -> Result<Option<Delta>> {
        let prior = self.blocks.entry(key).or_default();
        let delta = if append {
            prior.push_str(text);
            text.to_string()
        } else if let Some(suffix) = text.strip_prefix(prior.as_str()) {
            let delta = suffix.to_string();
            *prior = text.to_string();
            delta
        } else {
            return Err(errors::protocol(
                "豆包工作替换了已下发正文，无法安全继续输出",
            ));
        };
        if delta.is_empty() {
            return Ok(None);
        }
        if reasoning {
            self.reasoning.push_str(&delta);
        } else {
            self.text.push_str(&delta);
        }
        if self.text.len() > MAX_OUTPUT || self.reasoning.len() > MAX_OUTPUT {
            return Err(errors::protocol("豆包工作回答超过大小限制"));
        }
        Ok(Some(if reasoning {
            Delta::Reasoning(delta)
        } else {
            Delta::Text(delta)
        }))
    }

    fn blocks(&mut self, value: &Value, append: bool, source: &str) -> Result<Vec<Delta>> {
        let mut deltas = Vec::new();
        for block in value.as_array().into_iter().flatten() {
            let kind = util::unsigned(&block["block_type"]);
            if matches!(kind, Some(10010 | 10084)) {
                self.note_images(block)?;
                continue;
            }
            if !matches!(kind, Some(10000 | 10040)) {
                continue;
            }
            let id = util::identifier(&block["block_id"]).unwrap_or_else(|| "default".into());
            let reasoning = if kind == Some(10040) {
                self.reasoning_blocks.insert(id.clone());
                true
            } else {
                let content = &block["content"]["text_block"];
                let tagged = ["icon_url", "icon_url_dark"].iter().any(|key| {
                    content[*key]
                        .as_str()
                        .is_some_and(|icon| icon.to_ascii_lowercase().contains("deep_think"))
                });
                if tagged {
                    self.reasoning_blocks.insert(id.clone());
                } else if ["icon_url", "icon_url_dark"]
                    .iter()
                    .any(|key| content.get(*key).is_some())
                {
                    self.reasoning_blocks.remove(&id);
                }
                self.reasoning_blocks.contains(&id)
            };
            if source != "snapshot" {
                self.active_block = Some((id.clone(), reasoning));
            }
            let content = if kind == Some(10040) {
                &block["content"]["thinking_block"]
            } else {
                &block["content"]["text_block"]
            };
            let text = content["text"]
                .as_str()
                .or_else(|| content["content"].as_str());
            let Some(text) = text else {
                continue;
            };
            let key = format!("{}:{id}", if reasoning { "reasoning" } else { "text" });
            if append && source == "chunk" && self.compact_channel.contains(&key) {
                continue;
            }
            if append
                && source == "notify"
                && (self.chunk_channel.contains(&key) || self.compact_channel.contains(&key))
            {
                continue;
            }
            if source == "chunk" && !text.is_empty() {
                self.chunk_channel.insert(key.clone());
            }
            if let Some(delta) = self.text(key, text, append, reasoning)? {
                deltas.push(delta);
            }
        }
        Ok(deltas)
    }

    fn compact(&mut self, value: &Value) -> Result<Vec<Delta>> {
        let Some(text) = value["text"].as_str() else {
            return Ok(Vec::new());
        };
        let explicit = util::identifier(&value["block_id"]);
        let (id, reasoning) = match explicit {
            Some(id) => {
                let reasoning = self.reasoning_blocks.contains(&id);
                (id, reasoning)
            }
            None => self
                .active_block
                .clone()
                .ok_or_else(|| errors::protocol("豆包工作紧凑文本增量缺少已确认的所属内容块"))?,
        };
        let key = format!("{}:{id}", if reasoning { "reasoning" } else { "text" });
        if self.chunk_channel.contains(&key) {
            return Ok(Vec::new());
        }
        self.compact_channel.insert(key.clone());
        Ok(self.text(key, text, true, reasoning)?.into_iter().collect())
    }

    fn observe_restriction(&mut self, value: &Value) {
        for ext in [
            value.get("ext"),
            value.pointer("/message/ext"),
            value.pointer("/content/ext"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(error) = restriction::error(ext) {
                self.restriction.get_or_insert(error);
            }
        }
        for patch in value["patch_op"].as_array().into_iter().flatten() {
            if util::integer(&patch["patch_object"]) == Some(50) {
                if let Some(error) = restriction::error(&patch["patch_value"]["ext"]) {
                    self.restriction.get_or_insert(error);
                }
            }
        }
    }

    pub fn restriction(&self) -> Option<&Error> {
        self.restriction.as_ref()
    }

    pub fn begin_stream(&mut self) {
        // 后续异步任务有独立内容块编号空间，正文/图片历史仍保留。
        self.blocks.clear();
        self.reasoning_blocks.clear();
        self.chunk_channel.clear();
        self.compact_channel.clear();
        self.active_block = None;
    }

    pub fn frame(&mut self, event: &str, raw: &str) -> Result<(Vec<Delta>, bool)> {
        self.received = self.received.saturating_add(raw.len());
        if self.received > 64 * 1024 * 1024 {
            return Err(errors::protocol("豆包工作流式响应过大"));
        }
        let value: Value =
            serde_json::from_str(raw).map_err(|_| errors::protocol("豆包工作事件 JSON 无效"))?;
        if event.contains("ERR")
            || matches!(event, "error" | "gateway-error")
            || errors::business(&value).is_some_and(|code| code != 0)
            || value
                .get("error")
                .is_some_and(|error| !error.is_null() && error != false)
            || errors::verification(&value)
        {
            return Err(errors::upstream(200, &value, ReadEndpoint::Chat));
        }
        self.observe_restriction(&value);
        let done = event == "SSE_REPLY_END" && util::integer(&value["end_type"]) == Some(3);
        let deltas = match event {
            "STREAM_MSG_NOTIFY" => {
                self.blocks(&value["content"]["content_block"], true, "notify")?
            }
            "FULL_MSG_NOTIFY" if util::integer(&value["message"]["user_type"]) == Some(2) => {
                self.blocks(&value["message"]["content_block"], false, "snapshot")?
            }
            "CHUNK_DELTA" => self.compact(&value)?,
            "STREAM_CHUNK" => {
                let mut deltas = Vec::new();
                for patch in value["patch_op"].as_array().into_iter().flatten() {
                    if util::integer(&patch["patch_object"]) != Some(1) {
                        continue;
                    }
                    deltas.extend(self.blocks(
                        &patch["patch_value"]["content_block"],
                        util::integer(&patch["patch_type"]) == Some(1),
                        "chunk",
                    )?);
                }
                deltas
            }
            "ASYNC_CHUNK_SNAPSHOT" => {
                self.blocks(&value["message"]["content_block"], false, "snapshot")?
            }
            _ => Vec::new(),
        };
        Ok((deltas, done))
    }

    pub fn image_urls(&self) -> Vec<String> {
        self.images
            .iter()
            .filter_map(|image| image.raw.clone().or_else(|| image.preview.clone()))
            .collect()
    }
    pub fn image_markdown(&self) -> String {
        self.image_urls()
            .into_iter()
            .map(|url| format!("\n\n![图片]({url})\n\n"))
            .collect()
    }
}

fn object_key(url: &str) -> String {
    url.split('?')
        .next()
        .unwrap_or(url)
        .split('~')
        .next()
        .unwrap_or(url)
        .splitn(4, '/')
        .nth(3)
        .unwrap_or(url)
        .to_string()
}
fn safe_image(raw: &str) -> Option<String> {
    if raw.len() > 8192 || raw.chars().any(char::is_control) {
        return None;
    }
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    if ![
        ".byteimg.com",
        ".ibyteimg.com",
        ".ibytedtos.com",
        ".douyinpic.com",
        ".imagex.cn",
        ".bytevcloudapi.com",
    ]
    .iter()
    .any(|suffix| url.host_str().is_some_and(|host| host.ends_with(suffix)))
    {
        return None;
    }
    Some(raw.replace(')', "%29").replace('(', "%28"))
}
