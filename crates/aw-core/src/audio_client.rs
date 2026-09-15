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
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ClientError {
    /// 传输层失败（连不上 / 超时 / 连接被重置）：不重试
    Http(String),
    /// 服务端返回的 HTTP 错误：**状态码 + body**（body 必须透出，503 靠它解释原因）
    Server(u16, String),
    /// 响应解析失败
    Decode(String),
}

impl ClientError {
    /// HTTP 状态码；传输/解析错误没有状态码
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
        let body = json!({ "model": model, "request": request });

        let resp = self.post_with_retry("/v1/tasks/run", &body)?;
        // 响应体合法但没有 audio 字段：属于"响应不可用"，不是 HTTP 错误，也不可重试
        let audio = resp.get("audio").and_then(Value::as_str).ok_or_else(|| {
            ClientError::Decode(format!(
                "响应缺少 audio 字段: {}",
                truncate(&resp.to_string())
            ))
        })?;
        decode_base64(audio).map_err(ClientError::Decode)
    }

    /// 是否可用（/health）
    pub fn healthy(&self) -> bool {
        ureq::get(&format!("{}/health", self.base))
            .timeout(Duration::from_secs(5))
            .call()
            .map(|r| r.into_string().unwrap_or_default().contains("\"ok\""))
            .unwrap_or(false)
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
