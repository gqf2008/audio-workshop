//! 歌曲彩蛋链路：yue2 / ace-step 文生歌，以及 sheetsage2 → yue2 翻唱。

use crate::audio_client::{Client, ClientError};
use crate::dub::write_atomic_explained;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongModel {
    Yue2,
    AceStep,
}

impl SongModel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Yue2 => "yue2",
            Self::AceStep => "ace-step",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SongOptions {
    pub model: SongModel,
    pub lyrics: String,
    pub style: String,
    pub seed: u64,
    pub duration_seconds: u32,
    pub steps: u32,
    pub max_tokens: u32,
    pub language: String,
    /// 翻唱：sheetsage2 的 ABC 谱；None 表示文生歌。
    pub abc: Option<String>,
}

impl Default for SongOptions {
    fn default() -> Self {
        Self {
            model: SongModel::Yue2,
            lyrics: String::new(),
            style: String::new(),
            seed: 831001,
            duration_seconds: 120,
            steps: 8,
            max_tokens: 4200,
            language: "zh".into(),
            abc: None,
        }
    }
}

fn validate(options: &SongOptions) -> Result<(), String> {
    if options.lyrics.trim().is_empty() {
        return Err("歌词为空".into());
    }
    if options.style.trim().is_empty() {
        return Err("歌曲风格为空".into());
    }
    if options.duration_seconds == 0 {
        return Err("歌曲时长必须 > 0".into());
    }
    Ok(())
}

fn yue2_request(options: &SongOptions) -> Value {
    let mut request = json!({
        "lyrics": options.lyrics,
        "options": {
            "style": options.style,
            "num_inference_steps": options.steps.to_string(),
            "semantic_max_tokens": options.max_tokens.to_string(),
            "seed": options.seed.to_string(),
        }
    });
    if let Some(abc) = options.abc.as_deref() {
        request["options"]["cot"] = json!("melody");
        request["options"]["abc"] = json!(abc);
    } else {
        request["options"]["cot"] = json!("off");
    }
    request
}

fn ace_step_request(options: &SongOptions) -> Value {
    json!({
        "text": options.style,
        "lyrics": options.lyrics,
        "task_route": "text2music",
        "options": {
            "language": options.language,
            "duration_seconds": options.duration_seconds.to_string(),
            "num_inference_steps": options.steps.to_string(),
            "seed": options.seed.to_string(),
        }
    })
}

fn request_for(options: &SongOptions) -> Value {
    match options.model {
        SongModel::Yue2 => yue2_request(options),
        SongModel::AceStep => ace_step_request(options),
    }
}

/// 生成歌曲 wav；文件按固定临时文件 + fsync + rename 落盘。
pub fn generate_song(
    client: &Client,
    dir: &Path,
    file_name: &str,
    options: &SongOptions,
) -> Result<PathBuf, ClientError> {
    validate(options).map_err(ClientError::Http)?;
    let wav = client.run_audio(options.model.id(), request_for(options))?;
    let path = dir.join(format!("{file_name}.wav"));
    write_atomic_explained(&path, &wav).map_err(|e| ClientError::Local(e.to_string()))?;
    Ok(path)
}

/// 调用 sheetsage2 把音频转成 ABC 谱；兼容服务端 text/abc 两种返回字段。
pub fn transcribe_abc(client: &Client, audio: &Path) -> Result<String, ClientError> {
    let response = client.run_json(
        "sheetsage2",
        json!({ "audio": audio.display().to_string() }),
    )?;
    response
        .get("abc")
        .or_else(|| response.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| ClientError::Decode("sheetsage2 响应缺少 abc/text 字段".into()))
}

/// 翻唱：源音频 → ABC → yue2 cot=melody。
pub fn generate_cover(
    client: &Client,
    dir: &Path,
    source_audio: &Path,
    file_name: &str,
    options: &SongOptions,
) -> Result<PathBuf, ClientError> {
    let abc = transcribe_abc(client, source_audio)?;
    let mut cover = options.clone();
    cover.model = SongModel::Yue2;
    cover.abc = Some(abc);
    generate_song(client, dir, file_name, &cover)
}
