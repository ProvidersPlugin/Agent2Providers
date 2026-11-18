//! 跨 SSE 分片的思考标签拆解；普通不支持思考的模型不会调用此转换。
//! 对应原生 ThinkingParser 的正文/思考两态，不生成、改写或执行工具调用。
const TAGS: [(&str, &str); 4] = [("<thinking>", "</thinking>"), ("<think>", "</think>"),
    ("<reasoning>", "</reasoning>"), ("<thought>", "</thought>")];

#[derive(Default)]
pub struct Parser { pending: String, close: Option<&'static str>, finished: bool }
pub struct Piece { pub text: String, pub reasoning: bool }
impl Parser {
    pub fn push(&mut self, text: &str) -> Vec<Piece> { self.pending.push_str(text); self.drain(false) }
    pub fn finish(&mut self) -> Vec<Piece> {
        if self.finished { return Vec::new(); }
        self.finished = true;
        self.drain(true)
    }
    fn drain(&mut self, final_chunk: bool) -> Vec<Piece> {
        let mut output = Vec::new();
        while !self.pending.is_empty() {
            if let Some(close) = self.close {
                if let Some(at) = self.pending.find(close) {
                    if at > 0 { output.push(Piece { text: self.pending[..at].into(), reasoning: true }); }
                    self.pending = trim_newline(&self.pending[at + close.len()..]).into(); self.close = None;
                    continue;
                }
            } else {
                let next = TAGS.iter().flat_map(|(open, close)| [(*open, Some(*close)), (*close, None)])
                    .filter_map(|(tag, close)| self.pending.find(tag).map(|at| (at, tag, close))).min_by_key(|(at, _, _)| *at);
                if let Some((at, tag, close)) = next {
                    if at > 0 { output.push(Piece { text: self.pending[..at].into(), reasoning: false }); }
                    let rest = &self.pending[at + tag.len()..];
                    self.pending = if close.is_none() { trim_newline(rest).into() } else { rest.into() };
                    self.close = close;
                    continue;
                }
            }
            let keep = if final_chunk { 0 } else { trailing(&self.pending) };
            let safe = self.pending.len() - keep;
            if safe > 0 {
                output.push(Piece { text: self.pending[..safe].into(), reasoning: self.close.is_some() });
                self.pending = self.pending[safe..].into();
            }
            break;
        }
        output
    }
}
fn trailing(text: &str) -> usize {
    TAGS.iter().flat_map(|(open, close)| [*open, *close]).filter_map(|tag| {
        (1..tag.len().min(text.len() + 1)).rev().find(|length| text.is_char_boundary(text.len() - *length) && text.ends_with(&tag[..*length]))
    }).max().unwrap_or(0)
}
fn trim_newline(text: &str) -> &str { text.strip_prefix("\n\n").or_else(|| text.strip_prefix("\r\n")).or_else(|| text.strip_prefix('\n')).unwrap_or(text) }
pub fn strip(text: &str) -> String {
    let mut output = text.to_string();
    for (open, close) in TAGS { output = output.replace(open, "").replace(close, ""); }
    output
}
