//! 配音链路：切句 → 逐句合成 → 拼装（含句子级时间轴）/ 单句重录。
//!
//! 与 Python 侧 tools/audio_dub.py 行为一致：句子级时间轴让「改一句只重录那一句」
//! 成为天然能力，也顺带产出 SRT。

use crate::audio_client::{Client, ClientError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PUNCTUATION: &str = "。！？；…";

/// 按句末标点切句；超长句子再按逗号断（避免单次请求过长）
pub fn split_sentences(text: &str, punctuation: &str, max_chars: usize) -> Vec<String> {
    let punct: Vec<char> = punctuation.chars().collect();
    let mut sentences = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if punct.contains(&ch) {
            let s = cur.trim();
            if !s.is_empty() {
                sentences.push(s.to_string());
            }
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        sentences.push(cur.trim().to_string());
    }

    // 超长再按逗号/顿号断
    let mut out = Vec::new();
    for s in sentences {
        let mut rest = s;
        while rest.chars().count() > max_chars {
            let chars: Vec<char> = rest.chars().collect();
            let window: String = chars[..max_chars.min(chars.len())].iter().collect();
            // rfind 给的是**字节**下标；中文标点是多字节，必须加上该字符本身的长度，
            // 否则 ..=i 会切在字符中间（byte index is not a char boundary）。
            let cut = window
                .rfind(['，', ',', '、'])
                .map(|i| i + window[i..].chars().next().map_or(0, |c| c.len_utf8()))
                .unwrap_or(0);
            if cut == 0 {
                break; // 没有可断点，保留整句
            }
            out.push(rest[..cut].trim().to_string());
            rest = rest[cut..].trim().to_string();
        }
        if !rest.is_empty() {
            out.push(rest);
        }
    }
    out
}

/// 句子级时间轴 → SRT 时间戳（HH:MM:SS,mmm）
pub fn srt_timestamp(secs: f64) -> String {
    let total_ms = (secs * 1000.0).round() as u64;
    let (h, m, s, ms) = (
        total_ms / 3_600_000,
        (total_ms / 60_000) % 60,
        (total_ms / 1000) % 60,
        total_ms % 1000,
    );
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Sentence {
    pub index: usize,
    pub text: String,
    pub spoken: String,
    pub seed: u64,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub status: String, // pending | done | error:...
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub model: String,
    #[serde(default)]
    pub voice_ref: Option<String>,
    pub gap_ms: u64,
    pub base_seed: u64,
    pub sentences: Vec<Sentence>,
}

impl Project {
    // 构造参数确实多（脚本/模型/间隔/种子/参考音/标点/长度/规范化闭包），
    // 但每个都是调用方必须显式给出的决策；改为配置结构体会让调用点更啰嗦。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        script: &str,
        model: impl Into<String>,
        gap_ms: u64,
        base_seed: u64,
        voice_ref: Option<String>,
        punctuation: &str,
        max_chars: usize,
        normalize: impl Fn(&str) -> String,
    ) -> Self {
        let sentences = split_sentences(script, punctuation, max_chars)
            .into_iter()
            .enumerate()
            .map(|(i, text)| Sentence {
                index: i,
                spoken: normalize(&text),
                text,
                seed: base_seed + i as u64,
                duration: None,
                start: None,
                status: "pending".into(),
            })
            .collect();
        Self {
            model: model.into(),
            voice_ref,
            gap_ms,
            base_seed,
            sentences,
        }
    }

    /// 合成未完成的句子（或指定序号）
    pub fn synthesize(
        &mut self,
        client: &Client,
        dir: &Path,
        only: Option<&[usize]>,
        instruction: Option<&str>,
        mut on_progress: impl FnMut(usize, &str),
    ) -> Result<(), ClientError> {
        std::fs::create_dir_all(dir.join("sentences")).ok();
        for s in self.sentences.iter_mut() {
            if let Some(list) = only {
                if !list.contains(&s.index) {
                    continue;
                }
            } else if s.status == "done" {
                continue;
            }
            on_progress(s.index, &s.spoken);
            match client.synth(
                &self.model,
                &s.spoken,
                Some(s.seed),
                self.voice_ref.as_deref(),
                instruction,
            ) {
                Ok(wav) => {
                    let path = dir.join(format!("sentences/{:03}.wav", s.index));
                    std::fs::write(&path, &wav).map_err(|e| ClientError::Http(e.to_string()))?;
                    let d = wav_duration(&wav)?;
                    s.duration = Some(d);
                    s.status = "done".into();
                    on_progress(s.index, &format!("done {d:.2}s"));
                }
                Err(e) => {
                    s.status = format!("error: {e}");
                    on_progress(s.index, &format!("error {e}"));
                }
            }
        }
        Ok(())
    }

    /// 拼装成品 + SRT（句子级时间轴）
    pub fn assemble(&mut self, dir: &Path) -> Result<(f64, PathBuf, PathBuf), String> {
        let done: Vec<&Sentence> = self
            .sentences
            .iter()
            .filter(|s| s.status == "done")
            .collect();
        if done.is_empty() {
            return Err("还没有已合成的句子".into());
        }
        let reader =
            hound::WavReader::open(dir.join(format!("sentences/{:03}.wav", done[0].index)))
                .map_err(|e| e.to_string())?;
        let spec = reader.spec();
        drop(reader);

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
        let final_wav = out_dir.join("final.wav");
        let mut writer = hound::WavWriter::create(&final_wav, spec).map_err(|e| e.to_string())?;

        let gap_frames = (spec.sample_rate as u64 * self.gap_ms / 1000) as usize;
        let mut cursor_frames: u64 = 0;
        let mut srt = String::new();
        let mut srt_index = 0u32;
        let last = self.sentences.len() - 1;

        for (k, s) in self.sentences.iter_mut().enumerate() {
            if s.status != "done" {
                s.start = None;
                continue;
            }
            let path = dir.join(format!("sentences/{:03}.wav", s.index));
            let mut r = hound::WavReader::open(&path).map_err(|e| e.to_string())?;
            let samples: Vec<i16> = r
                .samples::<i16>()
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            for v in &samples {
                writer.write_sample(*v).map_err(|e| e.to_string())?;
            }
            let frames = samples.len() / spec.channels as usize;
            let start = cursor_frames as f64 / spec.sample_rate as f64;
            s.start = Some(start);
            cursor_frames += frames as u64;
            if k != last {
                for _ in 0..(gap_frames * spec.channels as usize) {
                    writer.write_sample(0i16).map_err(|e| e.to_string())?;
                }
                cursor_frames += gap_frames as u64;
            }
            let dur = s
                .duration
                .unwrap_or(frames as f64 / spec.sample_rate as f64);
            srt_index += 1;
            srt.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                srt_index,
                srt_timestamp(start),
                srt_timestamp(start + dur),
                s.text
            ));
        }
        writer.finalize().map_err(|e| e.to_string())?;
        let srt_path = out_dir.join("final.srt");
        std::fs::write(&srt_path, srt).map_err(|e| e.to_string())?;
        Ok((
            cursor_frames as f64 / spec.sample_rate as f64,
            final_wav,
            srt_path,
        ))
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::write(
            dir.join("project.json"),
            serde_json::to_string_pretty(self).unwrap(),
        )
    }

    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(dir.join("project.json"))?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// 从 wav 字节读时长（秒）
pub fn wav_duration(wav: &[u8]) -> Result<f64, ClientError> {
    let r = hound::WavReader::new(std::io::Cursor::new(wav))
        .map_err(|e| ClientError::Decode(e.to_string()))?;
    let spec = r.spec();
    let frames = r.duration() as f64 / spec.channels as f64;
    Ok(frames / spec.sample_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_sentence_punctuation() {
        let s = split_sentences("第一句。第二句！第三句？", DEFAULT_PUNCTUATION, 80);
        assert_eq!(s, vec!["第一句。", "第二句！", "第三句？"]);
    }

    #[test]
    fn splits_long_sentence_on_comma() {
        let text = "这是一个很长的句子，需要按逗号断开，才能保证每次请求不会太长。";
        let s = split_sentences(text, DEFAULT_PUNCTUATION, 12);
        assert!(s.len() >= 2, "超长句应被断开: {s:?}");
        assert!(
            s.iter().all(|x| x.chars().count() <= 14 + 1),
            "断点应贴近上限: {s:?}"
        );
    }

    #[test]
    fn srt_timestamp_formats_millis() {
        assert_eq!(srt_timestamp(0.0), "00:00:00,000");
        assert_eq!(srt_timestamp(2.479), "00:00:02,479");
        assert_eq!(srt_timestamp(61.5), "00:01:01,500");
        assert_eq!(srt_timestamp(3661.25), "01:01:01,250");
    }
}
