//! audiocpp_server 客户端（POST /v1/tasks/run）。
//!
//! 行为对齐 Python 侧 tools/audio_eval.py 的 `post_retry(tries=6, wait=5.0)`：
//! - 错误体要透出（503 的原因"内存预检/忙碌"就在 body 里）
//! - 只有 **503** 才退避重试，且按**状态码**判定
//! - 传输/解码错误不重试：服务没起来时重试只是让每句白等一整个退避周期

use serde_json::{json, Value};
use std::time::Duration;

/// Python `post_retry` 的 `tries=6`：最多 6 次尝试（不是 6 次重试）
pub const DEFAULT_RETRIES: u32 = 6;
/// 质检默认用的 ASR 模型（M0 定标同款；服务里还有 audio8-asr / fun-asr）
pub const DEFAULT_ASR_MODEL: &str = "qwen3-asr";
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ClientError {
    /// 传输层失败（连不上 / 超时 / 连接被重置）：不重试
    Http(String),
    /// 服务端返回的 HTTP 错误：**状态码 + body**（body 必须透出，503 靠它解释原因）
    Server(u16, String),
    /// 响应解析失败
    Decode(String),
    /// **本地**失败（落盘、读文件、编码等）：与网络无关，但走同一条错误上抛通道。
    /// 文案由调用方给全（哪个路径、要多少空间、下一步做什么），见 `dub::write_failure_note`。
    Local(String),
}

impl ClientError {
    /// HTTP 状态码；传输/解析/本地错误没有状态码
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Server(code, _) => Some(*code),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(e) => write!(f, "请求失败: {e}"),
            ClientError::Server(code, body) => write!(f, "服务端拒绝: HTTP {code} {body}"),
            ClientError::Decode(e) => write!(f, "响应解析失败: {e}"),
            // 本地失败已经带全上下文（路径 / 需要多少空间），不再加前缀把话说两遍
            ClientError::Local(e) => write!(f, "{e}"),
        }
    }
}

/// `voice_ref` 给了但参考文本为空时的**前置拦截**文案。
///
/// 服务端原文 `Audio8 TTS prepare with inline reference audio requires reference_text option`
/// 用户看不懂，而且是在**每一句**上重复撞出来的（N 句 = N 条一样的 500）。
/// 这里一次说清"缺什么 / 去哪填 / 怎么免手填"。
/// 调用了克隆但没给参考音频路径时的**前置拦截**文案。
///
/// 正常 UI 走不到（路径为空就是内置音色、不会走 `VoiceClone`），但 `VoiceClone::new`
/// 是公开入口，类型自己的不变式该自己守。
pub const MISSING_REFERENCE_PATH: &str = concat!(
    "调用了参考音频克隆，但没有给参考音频路径。",
    "内置音色不需要参考音；要克隆就先在「参考音频」里填一段干净的 5–30 秒人声。",
);

pub const MISSING_REFERENCE_TEXT: &str = concat!(
    "参考音频已选，但缺少它的文本（reference_text）。",
    "克隆音色时服务端要求音频与文本成对：请在「参考音频的文本」里填这段音频实际念的内容，",
    "或点「自动转写」让 ASR 填好、确认无误后再开始。",
);

/// 参考音频克隆的**成对**输入：`voice_ref`（音频路径）+ `reference_text`
/// （这段音频实际念的内容）。服务端（audio8-tts / index-tts2）要求两者同时给，
/// 只给路径必然失败（真机：HTTP 500
/// `Audio8 TTS prepare with inline reference audio requires reference_text option`）。
///
/// 为什么是结构体而不是两个相邻的 `Option<&str>` 参数：两个同类型参数挨在一起，
/// 传反了编译器不会拦（见 `LESSON_同类型参数批量插入会静默错位`）。
/// 字段私有 + `new()` 校验 ⇒ "只给路径不给文本"在**类型上**不可表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceClone<'a> {
    path: &'a str,
    reference_text: &'a str,
}

impl<'a> VoiceClone<'a> {
    /// 唯一构造入口：路径或文本为空白即 `Err`（不是 `None`，也不是"悄悄发个空串"）。
    ///
    /// 空串发过去服务端照样报错，只是换了个看不懂的说法；在**发起前**拦住，
    /// 用户拿到的是一次可执行的提示，而不是 N 句 `error:`。
    ///
    /// 路径也要校验：只校文本的话，"空路径 + 有文本"会通过，然后发出
    /// `voice_ref: ""` —— 与这个类型自称的"成对"不一致（复核指出；
    /// 当前 UI 走不到，但类型不变式不该依赖 UI 恰好拦得住）。
    pub fn new(path: &'a str, reference_text: &'a str) -> Result<Self, ClientError> {
        if path.trim().is_empty() {
            return Err(ClientError::Local(MISSING_REFERENCE_PATH.into()));
        }
        if reference_text.trim().is_empty() {
            return Err(ClientError::Local(MISSING_REFERENCE_TEXT.into()));
        }
        Ok(Self {
            path,
            reference_text,
        })
    }

    /// 参考音频路径（原样透传，不做归一）。
    pub fn path(self) -> &'a str {
        self.path
    }

    /// 参考音频里实际念的内容（原样透传，**不 trim**：服务端要的是真实文本）。
    pub fn reference_text(self) -> &'a str {
        self.reference_text
    }
}

pub struct Client {
    base: String,
    retries: u32,
    backoff: Duration,
}

impl Client {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            retries: DEFAULT_RETRIES,
            backoff: DEFAULT_BACKOFF,
        }
    }

    pub fn with_retry(mut self, retries: u32, backoff: Duration) -> Self {
        self.retries = retries;
        self.backoff = backoff;
        self
    }

    /// 合成一句话，返回 wav 字节。seed 固定可复现（audio8-* 支持）。
    ///
    /// `clone` 为 `Some` 时**成对**发送 `voice_ref` + `reference_text`
    /// ——服务端硬要求两者同时给，见 `VoiceClone`。
    pub fn synth(
        &self,
        model: &str,
        text: &str,
        seed: Option<u64>,
        clone: Option<VoiceClone<'_>>,
        instruction: Option<&str>,
    ) -> Result<Vec<u8>, ClientError> {
        let mut options = serde_json::Map::new();
        if let Some(s) = seed {
            options.insert("seed".into(), json!(s.to_string()));
        }
        if let Some(i) = instruction {
            options.insert("instruction".into(), json!(i));
        }
        let mut request = json!({ "text": text, "options": Value::Object(options) });
        if let Some(c) = clone {
            // 两行必须同进同出：只发 voice_ref 就是那个"每句都 500"的老 bug。
            request["voice_ref"] = json!(c.path());
            request["reference_text"] = json!(c.reference_text());
        }
        self.run_audio(model, request)
    }

    /// 发送任意 audio.cpp 音频任务并取回 wav 字节。TTS 之外的 gen 场景
    /// （BGM/歌曲）需要自己的 `request_defaults`，由调用方组装 request。
    pub fn run_audio(&self, model: &str, request: Value) -> Result<Vec<u8>, ClientError> {
        let resp = self.run_json(model, request)?;
        let audio = resp.get("audio").and_then(Value::as_str).ok_or_else(|| {
            ClientError::Decode(format!(
                "响应缺少 audio 字段: {}",
                truncate(&resp.to_string())
            ))
        })?;
        decode_base64(audio).map_err(ClientError::Decode)
    }

    /// 发送任意 audio.cpp 任务并返回原始 JSON。MIDI/ABC 等非音频产物也走这里。
    /// 语音识别：把本地 wav 交给 ASR 模型，取回文本。
    ///
    /// 请求体与 `tools/audio_eval.py::transcribe` 一致（`{"audio": <path>}`），
    /// 模型默认 `qwen3-asr`（M0 定标用的就是它）。
    pub fn asr(&self, audio: &std::path::Path) -> Result<String, ClientError> {
        self.asr_with(DEFAULT_ASR_MODEL, audio)
    }

    /// 指定 ASR 模型（服务里还有 audio8-asr / fun-asr）。
    pub fn asr_with(&self, model: &str, audio: &std::path::Path) -> Result<String, ClientError> {
        let response = self.run_json(model, json!({ "audio": audio.display().to_string() }))?;
        response
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ClientError::Decode("ASR 响应缺少 text 字段".into()))
    }

    pub fn run_json(&self, model: &str, request: Value) -> Result<Value, ClientError> {
        let body = json!({ "model": model, "request": request });
        self.post_with_retry("/v1/tasks/run", &body)
    }

    /// 是否可用（/health）
    pub fn healthy(&self) -> bool {
        ureq::get(&format!("{}/health", self.base))
            .timeout(Duration::from_secs(5))
            .call()
            .map(|r| r.into_string().unwrap_or_default().contains("\"ok\""))
            .unwrap_or(false)
    }

    /// 服务健康 JSON 里的 backend（metal/cuda/cpu/...），不可用时返回 None。
    pub fn backend_label(&self) -> Option<String> {
        let value = ureq::get(&format!("{}/health", self.base))
            .timeout(Duration::from_secs(5))
            .call()
            .ok()?
            .into_json::<Value>()
            .ok()?;
        value
            .get("backend")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// 503 才退避重试（内存预检拒绝 / 模型忙碌），其余错误立即返回。
    fn post_with_retry(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let tries = self.retries.max(1);
        let mut last: Option<ClientError> = None;
        for attempt in 0..tries {
            match self.post_once(path, body) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // 按状态码判定，不看消息内容：500 且 body 里恰好含 "503"
                    // （比如错误信息里提到端口/编号）不该被当成可重试
                    let retryable = e.status() == Some(503) && attempt + 1 < tries;
                    last = Some(e);
                    if !retryable {
                        break;
                    }
                    std::thread::sleep(self.backoff);
                }
            }
        }
        Err(last.unwrap_or_else(|| ClientError::Http("未发起请求".into())))
    }

    fn post_once(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let url = format!("{}{}", self.base, path);
        match ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(1800))
            .send_string(&body.to_string())
        {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| ClientError::Decode(e.to_string())),
            Err(ureq::Error::Status(code, r)) => {
                // 关键：HTTP 错误也要读 body，否则 503 的原因（内存不足/忙碌）看不见
                let text = r.into_string().unwrap_or_default();
                Err(ClientError::Server(code, truncate(&text)))
            }
            Err(e) => Err(ClientError::Http(e.to_string())),
        }
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

/// 最小 base64 解码（避免为一个函数引入依赖）
pub fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        lut[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = lut[c as usize];
        if v == 255 {
            return Err(format!("非法 base64 字符: {}", c as char));
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_known_input() {
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(decode_base64("AAEC").unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn base64_rejects_invalid() {
        assert!(decode_base64("****").is_err());
    }

    /// 重试判定必须看状态码，不看消息文本
    #[test]
    fn retry_is_decided_by_status_code() {
        let e503 = ClientError::Server(503, "内存预检拒绝：需要 1200MB".into());
        assert_eq!(e503.status(), Some(503));
        // 500 而 body 里恰好含 "503"（如端口/编号）：不得被判成可重试
        let e500 = ClientError::Server(500, "后端 audio8-tts 在 503 号槽位启动失败".into());
        assert_ne!(e500.status(), Some(503));
        assert!(ClientError::Http("连接被拒绝".into()).status().is_none());
        assert!(ClientError::Decode("坏 json".into()).status().is_none());
    }
}
