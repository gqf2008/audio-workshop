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

    /// 这是不是服务端结构化返回的内存不足（而不是别的 503）。
    ///
    /// 唯一识别入口：只认 JSON 里 `error.type == "insufficient_memory"`，
    /// 不从人类可读 message 里猜数字或关键词。
    pub fn is_insufficient_memory(&self) -> bool {
        matches!(self, ClientError::Server(_, body) if memory_shortfall(body).is_some())
    }
}

/// 服务端结构化内存不足错误的最小投影。
///
/// `message` 是服务端原文，保留模型名、估算内存、余量和当前可用内存，
/// 不在这里重新解析数字（那会让服务端改文案时应用跟着漂移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryShortfall {
    pub message: String,
}

/// 只认结构化字段 `error.type == "insufficient_memory"`。
///
/// 非 JSON、空 body、别的 `error.type` 都返回 `None`；调用方不得把任意 503
/// 都说成内存不足。抽出纯函数是为了让识别与文案只有一处，供所有 Tab 共用。
pub fn memory_shortfall(body: &str) -> Option<MemoryShortfall> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("insufficient_memory") {
        return None;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("服务端没有提供内存不足的详细说明")
        .to_string();
    Some(MemoryShortfall { message })
}

/// 内存不足的唯一用户文案入口。
///
/// 三个动作是产品计划 §2 步骤 4 失败① 的可执行下一步；配音 / BGM / 歌曲 /
/// 人声分离 / 质检所有走到 `ClientError` 的路径都通过 `Display` 使用它，
/// 不再在每个 Tab 各写一套提示。
pub fn memory_shortfall_note(body: &str) -> Option<String> {
    memory_shortfall(body).map(|shortfall| {
        format!(
            "内存不足（OOM）：{}。释放内存后继续：① 卸载空闲模型；② 把模型降到 q4_0 量化档；③ 关掉其它占内存的应用。",
            shortfall.message
        )
    })
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(e) => write!(f, "请求失败: {e}"),
            ClientError::Server(code, body) => {
                if *code == 503 {
                    if let Some(note) = memory_shortfall_note(body) {
                        return write!(f, "{note}");
                    }
                    if body.trim().is_empty() {
                        return write!(f, "服务端拒绝: HTTP 503（响应没有正文，无法判断原因）");
                    }
                }
                write!(f, "服务端拒绝: HTTP {code} {}", truncate(body))
            }
            ClientError::Decode(e) => write!(f, "响应解析失败: {e}"),
            // 本地失败已经带全上下文（路径 / 需要多少空间），不再加前缀把话说两遍
            ClientError::Local(e) => write!(f, "{e}"),
        }
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
    pub fn synth(
        &self,
        model: &str,
        text: &str,
        seed: Option<u64>,
        voice_ref: Option<&str>,
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
        if let Some(v) = voice_ref {
            request["voice_ref"] = json!(v);
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

    /// 手动卸载服务当前加载的全部模型（`/v1/tasks/unload_all_models`）。
    ///
    /// 只在用户显式点击「释放模型内存」时调用；这里不做自动调用，也不猜测服务
    /// 返回。返回的 `unloaded` 名称会如实展示，空数组也会说清楚“没有可卸载的”。
    pub fn unload_all_models(&self) -> Result<String, ClientError> {
        let path = "/v1/tasks/unload_all_models";
        let value = self.post_once(path, &json!({}))?;
        let Some(list) = value.get("unloaded") else {
            return Ok(format!("服务返回成功：{}", truncate(&value.to_string())));
        };
        let Some(items) = list.as_array() else {
            return Ok(format!(
                "服务返回成功，但 unloaded 不是数组：{}",
                truncate(&value.to_string())
            ));
        };
        let names: Vec<String> = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            Ok("服务已成功响应：当前没有可卸载的已加载模型".into())
        } else {
            Ok(format!(
                "已请求卸载 {} 个模型：{}",
                names.len(),
                names.join("、")
            ))
        }
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
                    // 内存不足不是“服务忙”：服务端已经明确拒绝，重试同一请求只会让用户
                    // 白等退避周期；交给配音队列标 error: oom，释放内存后再由用户继续。
                    let retryable = e.status() == Some(503)
                        && !e.is_insufficient_memory()
                        && attempt + 1 < tries;
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
                // 保留完整 body：内存不足的 message 可能超过展示上限，结构化识别和
                // 原文透出都不该先被截断；Display 对流式错误做展示级截断。
                let text = r.into_string().unwrap_or_default();
                Err(ClientError::Server(code, text))
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

    const OOM_BODY: &str = r#"{
        "error": {
            "message": "cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)",
            "type": "insufficient_memory"
        }
    }"#;

    #[test]
    fn structured_oom_is_actionable_and_keeps_server_message() {
        let err = ClientError::Server(503, OOM_BODY.into());
        assert!(err.is_insufficient_memory());
        let note = err.to_string();
        assert!(note.contains("qwen3-asr"), "{note}");
        assert!(note.contains("3.31 GiB"), "{note}");
        assert!(note.contains("3.84 GiB"), "{note}");
        assert!(note.contains("卸载空闲模型"), "{note}");
        assert!(note.contains("q4_0"), "{note}");
        assert!(note.contains("关掉其它占内存的应用"), "{note}");
    }

    #[test]
    fn only_the_structured_error_type_is_treated_as_oom() {
        // 别的错误类型不能沾 OOM 文案。
        let busy = ClientError::Server(
            503,
            r#"{"error":{"message":"model is busy","type":"model_busy"}}"#.into(),
        );
        assert!(!busy.is_insufficient_memory());
        let busy_note = busy.to_string();
        assert!(busy_note.contains("model is busy"), "{busy_note}");
        assert!(!busy_note.contains("卸载空闲模型"), "{busy_note}");

        // 非 JSON body：保留原文，不按关键词猜。
        let plain = ClientError::Server(503, "service unavailable".into());
        assert!(!plain.is_insufficient_memory());
        assert!(plain.to_string().contains("service unavailable"));
        assert!(!plain.to_string().contains("内存不足"));

        // 空 body：明确说没有正文，不能伪装成内存不足。
        let empty = ClientError::Server(503, "".into());
        assert!(!empty.is_insufficient_memory());
        assert!(empty.to_string().contains("响应没有正文"));
    }

    #[test]
    fn oom_message_is_not_truncated_before_the_note_is_built() {
        let tail = "tail-after-two-hundred-chars";
        let message = format!("{}{tail}", "x".repeat(220));
        let body = format!(
            r#"{{"error":{{"message":"{}","type":"insufficient_memory"}}}}"#,
            message
        );
        let note = memory_shortfall_note(&body).expect("结构化 OOM 必须识别");
        assert!(note.contains(tail), "完整 message 不能被展示层吞掉: {note}");
    }
}
