#![allow(dead_code)] // 各测试目标各自用到其中一部分

//! 测试支撑：进程内 mock audiocpp_server。
//!
//! 有了它，"重试策略/失败计数/默认 instruction"这些**策略**可以确定性验证，
//! 不必依赖真机服务（真机 e2e 只在 `--ignored` 时跑，见 tests/e2e_service.rs）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct Mock {
    pub base: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    /// `script[n]` 是第 n 次请求（从 0 起）的应答；用完后重复最后一条。
    pub fn start(script: Vec<(u16, String)>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (hits_t, bodies_t) = (hits.clone(), bodies.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let n = hits_t.fetch_add(1, Ordering::SeqCst);
                let body = read_request(&mut stream);
                bodies_t.lock().unwrap().push(body);
                let (code, resp) = script
                    .get(n)
                    .or_else(|| script.last())
                    .cloned()
                    .unwrap_or((200, "{}".to_string()));
                let reason = match code {
                    200 => "OK",
                    503 => "Service Unavailable",
                    _ => "Error",
                };
                let head = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        Mock { base, hits, bodies }
    }

    pub fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// 收到的请求体（按到达顺序）
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(&mut *stream);
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return String::new();
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut body = vec![0u8; len];
    if reader.read_exact(&mut body).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&body).to_string()
}

/// 响应体：`{"audio": "<base64 wav>"}`
pub fn audio_response(wav: &[u8]) -> String {
    format!(r#"{{"audio":"{}"}}"#, b64(wav))
}

pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (k, i) in idx.iter().enumerate() {
            if k <= chunk.len() {
                out.push(T[*i as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 一段最小合法 wav（测试用）
pub fn tiny_wav(samples: &[i16]) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for s in samples {
            w.write_sample(*s).unwrap();
        }
        w.finalize().unwrap();
    }
    buf.into_inner()
}
