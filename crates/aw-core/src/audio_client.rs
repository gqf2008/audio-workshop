//! audiocpp_server 客户端（POST /v1/tasks/run）。
//!
//! 行为对齐 Python 侧 tools/audio_dub.py：错误体要透出、503（内存预检/忙碌）要退避重试、
//! 失败必须可见——这几条都是实测踩出来的（见 LESSON：指标噪声、静默失败类）。

use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug)]
pub enum ClientError {
    Http(String),
    Server(String),
    Decode(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(e) => write!(f, "请求失败: {e}"),
            ClientError::Server(e) => write!(f, "服务端拒绝: {e}"),
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
            retries: 6,
            backoff: Duration::from_secs(5),
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
        let audio = resp.get("audio").and_then(Value::as_str).ok_or_else(|| {
            ClientError::Server(format!(
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

    fn post_with_retry(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let mut last = ClientError::Http("未发起请求".into());
        for attempt in 0..=self.retries {
            match self.post_once(path, body) {
                Ok(v) => return Ok(v),
                Err(e @ ClientError::Server(_)) => {
                    // 503 = 内存预检拒绝或模型忙碌：退避重试（与 Python 侧同一策略）
                    if e.to_string().contains("503") && attempt < self.retries {
                        std::thread::sleep(self.backoff);
                        last = e;
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => {
                    last = e;
                    if attempt < self.retries {
                        std::thread::sleep(self.backoff);
                    }
                }
            }
        }
        Err(last)
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
                Err(ClientError::Server(format!(
                    "HTTP {code} {}",
                    truncate(&text)
                )))
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
}
