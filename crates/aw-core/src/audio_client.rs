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
        // 委托给唯一的结构化解析器：识别口径只能有一处
        self.insufficient_memory().is_some()
    }
}

/// 服务端结构化内存不足错误的**用户文案投影**。
///
/// `message` 是服务端原文（含模型名、估算内存、余量、可用内存这些数字，但**只作为文本**）。
/// 面向用户的提示一律用这段原文，不拿解析出来的数字重新拼一遍——服务端改文案时应用不该跟着漂移。
/// （数字要用于"降档候选"这类判断时，走 `InsufficientMemory` 的字段，见下。）
///
/// 实现上这里**委托** `parse_insufficient_memory`（唯一解析器），不再自己 parse 一次 JSON。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryShortfall {
    pub message: String,
}

/// 只认结构化字段 `error.type == "insufficient_memory"`。
///
/// 非 JSON、空 body、别的 `error.type` 都返回 `None`；调用方不得把任意 503
/// 都说成内存不足。抽出纯函数是为了让识别与文案只有一处，供所有 Tab 共用。
pub fn memory_shortfall(body: &str) -> Option<MemoryShortfall> {
    // **唯一解析器是 `parse_insufficient_memory`**：这里只做投影，不再自己 parse 一次。
    // 两个并行分支一度各写了一份"只认 error.type"的解析（本仓复核抓过多次同类漂移），
    // 合并时必须收敛到一处，否则识别口径会随改动分叉。
    let parsed = parse_insufficient_memory(body)?;
    Some(MemoryShortfall {
        message: parsed.message,
    })
}

/// 内存不足的唯一用户文案入口。
///
/// 三个动作是产品计划 §2 步骤 4 失败① 的可执行下一步；配音 / BGM / 歌曲 /
/// 人声分离 / 质检所有走到 `ClientError` 的路径都通过 `Display` 使用它，
/// 不再在每个 Tab 各写一套提示。
pub fn memory_shortfall_note(body: &str) -> Option<String> {
    memory_shortfall(body).map(|shortfall| {
        format!(
            "内存不足（OOM）：{}。释放内存后继续：① 释放模型内存（会卸载服务上所有已加载模型，先确认没有其它任务在用）；② 把模型降到 q4_0 量化档；③ 关掉其它占内存的应用。",
            shortfall.message
        )
    })
}

/// 服务 503 拒绝加载模型时给出的**内存不足**信息（数字就在 503 的 body 里）。
///
/// 真实样例（本机 16 GiB，qwen3-asr 装不下）：
/// ```text
/// {"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB
///  headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsufficientMemory {
    /// 服务端原文（保留模型名、估算、余量、可用内存；缺字段时的兜底也在这里）
    pub message: String,
    /// 装不下的那个模型 id（服务用单引号包着）
    pub model: Option<String>,
    /// 模型自身的估算占用（MiB）
    pub estimated_mib: Option<u64>,
    /// 服务要求预留的余量（MiB）
    pub headroom_mib: Option<u64>,
    /// 当时服务的可用主机内存（MiB）
    pub available_mib: Option<u64>,
}

impl InsufficientMemory {
    /// 合计需要多少（估算 + 余量）。缺任一项就是 None —— 不拿单项冒充合计。
    pub fn required_mib(&self) -> Option<u64> {
        self.estimated_mib?.checked_add(self.headroom_mib?)
    }
}

impl ClientError {
    /// 服务因**内存不足**拒绝加载模型时的结构化信息；其它错误（含模型忙碌的 503）为 None。
    ///
    /// 只看 `type` 字段：那是服务给的稳定契约，不靠匹配人读的 message 文案。
    pub fn insufficient_memory(&self) -> Option<InsufficientMemory> {
        match self {
            ClientError::Server(503, body) => parse_insufficient_memory(body),
            _ => None,
        }
    }
}

/// 解析服务的错误体；不是"内存不足"就返回 None。
///
/// 判定只认 `error.type == "insufficient_memory"`（稳定契约）。数字解析失败只让对应字段
/// 为 `None`，**不影响**判定本身——调用方仍能给出"这个模型装不下 + 换个更小的"这条可执行动作。
pub fn parse_insufficient_memory(body: &str) -> Option<InsufficientMemory> {
    let v: Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if err.get("type").and_then(Value::as_str) != Some("insufficient_memory") {
        return None;
    }
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
    let t: Vec<&str> = msg.split_whitespace().collect();
    Some(InsufficientMemory {
        message: if msg.is_empty() {
            "服务端没有提供内存不足的详细说明".to_string()
        } else {
            msg.to_string()
        },
        // `cannot load model 'qwen3-asr': ...`
        model: msg
            .split_once("model '")
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(id, _)| id.to_string())
            .filter(|id| !id.is_empty()),
        // `estimated 3.31 GiB + ...`
        estimated_mib: word_at(&t, "estimated").and_then(|i| size_at(&t, i + 1)),
        // `+ 1024 MiB headroom exceeds ...` —— 数字在 "headroom" 前面
        headroom_mib: word_at(&t, "headroom")
            .and_then(|i| i.checked_sub(2))
            .and_then(|i| size_at(&t, i)),
        // `... available host memory (3.84 GiB)`
        available_mib: word_at(&t, "memory").and_then(|i| size_at(&t, i + 1)),
    })
}

/// 词在分词里的下标（要求整词相等，避免 "memory" 命中别的词）。
fn word_at(tokens: &[&str], word: &str) -> Option<usize> {
    tokens
        .iter()
        .position(|t| t.trim_matches(|c: char| !c.is_alphanumeric()) == word)
}

/// 读 `tokens[i]`（数字，允许带 `(` 之类的前缀）与 `tokens[i+1]`（单位）为 MiB。
fn size_at(tokens: &[&str], i: usize) -> Option<u64> {
    let raw = tokens
        .get(i)?
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
    let value: f64 = raw.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    // 单位可能被标点包着（如 `(3.84 GiB)` 里的 `GiB)`）
    let unit = tokens
        .get(i + 1)?
        .trim_matches(|c: char| !c.is_ascii_alphabetic());
    let k = match unit {
        "GiB" => 1024.0,
        "MiB" => 1.0,
        "KiB" => 1.0 / 1024.0,
        _ => return None,
    };
    Some((value * k).round() as u64)
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

/// `synth` 请求体 `options` 里**允许**出现的键（白名单）。
///
/// 依据是服务端 spec 的 `options.request`（`~/.local/opt/audio.cpp/model_specs/*.json`）：
/// 本 App 选得到的两个 TTS 引擎（`audio8_tts` / `index_tts2`）都**没有** `instruction`
/// —— 声明了它的只有 breeze_tts / cosyvoice3 / dots_tts / firered_audio / fireredtts3 /
/// irodori_tts，本 App 一个都选不到。后端不认的键不报错、只是被忽略，所以"发了但没接"
/// 只能靠这条白名单挡住。真机对照（同 seed、同文本，只改 instruction）两个输出的
/// sha256 完全相同：`214f41f3…`。
///
/// 加键之前先核对服务端 spec 的 `options.request`，否则用户会以为那个旋钮生效。
pub const SYNTH_REQUEST_OPTIONS: &[&str] = &["seed", "reference_text"];

/// 白名单的唯一判据。
fn synth_option_allowed(key: &str) -> bool {
    SYNTH_REQUEST_OPTIONS.contains(&key)
}

/// 把 `(键, 值)` 按白名单放进 `options`；白名单外的键**丢弃**。
///
/// 丢弃而不是报错：调用方无从知道远端服务支持什么，报错只会把"应用自己写错"变成
/// 用户可见的失败；这里要保证的是"发出去的每个键都站得住"。
fn insert_whitelisted(options: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    if synth_option_allowed(key) {
        options.insert(key.to_string(), value);
    }
}

/// 组装 `/v1/tasks/run` 的 TTS 请求体：**唯一**组装点，白名单在这里生效。
///
/// `voice_ref` 是顶层字段（服务端 spec 里属 capabilities，不在 `options` 里）。
fn build_synth_request(text: &str, seed: Option<u64>, clone: Option<VoiceClone<'_>>) -> Value {
    let mut options = serde_json::Map::new();
    if let Some(s) = seed {
        insert_whitelisted(&mut options, "seed", json!(s.to_string()));
    }
    let mut request = json!({ "text": text, "options": Value::Object(options) });
    if let Some(c) = clone {
        // 两行必须同进同出：只发 voice_ref 就是那个"每句都 500"的老 bug（见 VoiceClone）。
        request["voice_ref"] = json!(c.path());
        request["reference_text"] = json!(c.reference_text());
    }
    request
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
    /// 请求体只由 [`build_synth_request`] 组装——那里的白名单是**唯一**决定
    /// "哪个键能发给后端"的地方。本函数不再收 `instruction` 这类自由旋钮：后端 spec
    /// 没声明的键发过去**不报错、直接忽略**，从调用点看不出来（见 `SYNTH_REQUEST_OPTIONS`）。
    ///
    /// `clone` 为 `Some` 时**成对**发送 `voice_ref` + `reference_text`
    /// ——服务端硬要求两者同时给，见 `VoiceClone`。
    pub fn synth(
        &self,
        model: &str,
        text: &str,
        seed: Option<u64>,
        clone: Option<VoiceClone<'_>>,
    ) -> Result<Vec<u8>, ClientError> {
        self.run_audio(model, build_synth_request(text, seed, clone))
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
            return Err(ClientError::Decode(format!(
                "unload 响应缺少 unloaded 数组: {}",
                truncate(&value.to_string())
            )));
        };
        let Some(items) = list.as_array() else {
            return Err(ClientError::Decode(format!(
                "unload 响应的 unloaded 不是数组: {}",
                truncate(&value.to_string())
            )));
        };
        if let Some(bad) = items.iter().position(|item| !item.is_string()) {
            return Err(ClientError::Decode(format!(
                "unload 响应 unloaded[{bad}] 不是字符串: {}",
                truncate(&value.to_string())
            )));
        }
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

    /// 白名单**有牙**：后端 spec 没声明的旋钮进不了请求体。
    /// 把 `insert_whitelisted` 的 `if` 去掉（或把 instruction 加回白名单）→ 本用例红。
    #[test]
    fn synth_option_whitelist_drops_knobs_the_backend_never_declared() {
        let mut options = serde_json::Map::new();
        insert_whitelisted(&mut options, "seed", json!("7"));
        insert_whitelisted(&mut options, "instruction", json!("悲伤、缓慢、低沉"));
        insert_whitelisted(&mut options, "style", json!("念白"));
        assert!(
            options.contains_key("seed"),
            "seed 在 spec 白名单里，应放行"
        );
        assert!(
            !options.contains_key("instruction"),
            "audio8_tts / index_tts2 的 spec 都没有 instruction，不该发（实测输出字节相同）"
        );
        assert!(!options.contains_key("style"), "越白名单的键一律丢弃");
    }

    /// 组装出来的请求体逐键核对白名单——**不是**拿单个 helper 自证。
    /// 真机曾经的请求体：`{"text":"…","options":{"seed":"7","instruction":"自然、清晰的叙述语气"}}`。
    #[test]
    fn synth_request_body_only_carries_whitelisted_options() {
        let body = build_synth_request("你好。", Some(7), None);
        let options = body["options"].as_object().expect("options 应是对象");
        for key in options.keys() {
            assert!(
                SYNTH_REQUEST_OPTIONS.contains(&key.as_str()),
                "请求体出现白名单外的键 `{key}`（后端会静默忽略它）"
            );
        }
        assert_eq!(options.get("seed").and_then(Value::as_str), Some("7"));
        assert!(
            !body.to_string().contains("instruction"),
            "请求体不该再出现 instruction: {body}"
        );
        // voice_ref 走顶层，不进 options（服务端 spec 的 capabilities 在 options 之外）
        let clone = VoiceClone::new("/tmp/ref.wav", "参考音念的内容").unwrap();
        let with_ref = build_synth_request("你好。", None, Some(clone));
        assert_eq!(with_ref["voice_ref"], json!("/tmp/ref.wav"));
        assert!(with_ref["options"].as_object().unwrap().is_empty());
    }

    /// 真机 503 body（本机 16 GiB / qwen3-asr 装不下）必须能解析出模型名与三个数字。
    #[test]
    fn parses_real_insufficient_memory_body() {
        let body = r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
        let m = parse_insufficient_memory(body).expect("应识别为内存不足");
        assert_eq!(m.model.as_deref(), Some("qwen3-asr"));
        assert_eq!(m.estimated_mib, Some(3389)); // 3.31 GiB
        assert_eq!(m.headroom_mib, Some(1024));
        assert_eq!(m.available_mib, Some(3932)); // 3.84 GiB
        assert_eq!(m.required_mib(), Some(4413));
        // ClientError 侧同一个入口
        let e = ClientError::Server(503, body.to_string());
        assert_eq!(
            e.insufficient_memory().unwrap().model.as_deref(),
            Some("qwen3-asr")
        );
    }

    /// 模型忙碌也是 503 —— 不能因为它是 503 就当成"装不下"。
    #[test]
    fn busy_503_is_not_reported_as_insufficient_memory() {
        let busy = r#"{"error":{"message":"model 'qwen3-asr' is busy","type":"model_busy"}}"#;
        assert!(parse_insufficient_memory(busy).is_none());
        assert!(ClientError::Server(503, busy.to_string())
            .insufficient_memory()
            .is_none());
        // 500（非 503）即使 body 自称内存不足也不算：状态码是唯一判据
        assert!(ClientError::Server(
            500,
            r#"{"error":{"type":"insufficient_memory"}}"#.to_string()
        )
        .insufficient_memory()
        .is_none());
    }

    /// 判定靠 type，不靠数字：message 换了措辞/单位不认识时，模型名与能认出的项仍然要出来。
    #[test]
    fn type_decides_while_broken_numbers_stay_none() {
        let odd = r#"{"error":{"message":"cannot load model 'audio8-asr': not enough room","type":"insufficient_memory"}}"#;
        let m = parse_insufficient_memory(odd).expect("type 决定判定");
        assert_eq!(m.model.as_deref(), Some("audio8-asr"));
        assert_eq!(m.estimated_mib, None);
        assert_eq!(m.required_mib(), None, "缺项时不得把单项当合计");
        assert!(parse_insufficient_memory("not json").is_none());
        assert!(parse_insufficient_memory(r#"{"error":{"message":"x"}}"#).is_none());
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
        assert!(
            note.contains("释放模型内存") && note.contains("所有已加载模型"),
            "{note}"
        );
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
        assert!(!busy_note.contains("释放模型内存"), "{busy_note}");

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
