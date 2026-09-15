//! 配音链路：切句 → 逐句合成 → 拼装（含句子级时间轴）/ 单句重录。
//!
//! 与 Python 侧 tools/audio_dub.py 行为一致：句子级时间轴让「改一句只重录那一句」
//! 成为天然能力，也顺带产出 SRT。
//!
//! 踩过的坑（Python 侧已修，这里补齐）：
//! - 句首逗号不能当断点：`cut <= 0` 是"没有可用断点"，切出来会是只含逗号的垃圾句
//! - 重录必须换 seed，否则拿回同一条不满意的音频
//! - 合成/拼装的失败句必须**报数**，不能静默跳过（成品少一句没人知道）

use crate::audio_client::{Client, ClientError};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const DEFAULT_PUNCTUATION: &str = "。！？；…";
/// Python 侧 cmd_synth 恒发这条 instruction（不是可选装饰：不发音色/语气线索时读法更飘）
pub const DEFAULT_INSTRUCTION: &str = "自然、清晰的叙述语气";
/// 重录时的 seed 步进（Python `cmd_redo`：`s["seed"] += 1000`）
pub const REDO_SEED_STEP: u64 = 1000;

/// 折叠空格与制表符（Python `re.sub(r"[ \t]+", " ", text)`）
fn collapse_blanks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for ch in text.chars() {
        if ch == ' ' || ch == '\t' {
            blank = true;
            continue;
        }
        if blank {
            out.push(' ');
            blank = false;
        }
        out.push(ch);
    }
    out
}

/// 按句末标点切句；超长句子再按逗号断（避免单次请求过长）
///
/// 与 Python `split_sentences` 同算法，断点判定一律用**字符**下标
/// （字节下标在中文标点上会切出半个字符）。
pub fn split_sentences(text: &str, punctuation: &str, max_chars: usize) -> Vec<String> {
    let punct: Vec<char> = punctuation.chars().collect();
    let push = |out: &mut Vec<String>, piece: &str| {
        let s = piece.trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    };

    // ① 按句末标点切（标点留在句尾）
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in collapse_blanks(text).chars() {
        cur.push(ch);
        if punct.contains(&ch) {
            push(&mut sentences, &cur);
            cur.clear();
        }
    }
    push(&mut sentences, &cur);

    // ② 超长再按逗号/顿号断
    let mut out = Vec::new();
    for s in sentences {
        let mut rest = s;
        while rest.chars().count() > max_chars {
            let chars: Vec<char> = rest.chars().collect();
            let cut = ['，', ',', '、']
                .iter()
                .filter_map(|c| chars[..max_chars].iter().rposition(|x| x == c))
                .max();
            // cut 为 None（窗口内没有断点）或 0（句首就是逗号）都要放弃：
            // 0 当哨兵却按字节加长度的话，会切出一个只含"，"的垃圾句
            match cut {
                Some(c) if c > 0 => {
                    push(&mut out, &chars[..=c].iter().collect::<String>());
                    rest = chars[c + 1..].iter().collect::<String>().trim().to_string();
                }
                _ => break,
            }
        }
        push(&mut out, &rest);
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
    /// 参考音频内容哈希。只比路径无法识别“同路径文件被替换”；旧工程缺此字段时
    /// 下一次会保守重录并按新内容写入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_ref_hash: Option<String>,
    pub gap_ms: u64,
    pub base_seed: u64,
    pub sentences: Vec<Sentence>,
}

/// 拼装结果。`skipped` 是**必须报出来的数**：失败句此前被静默跳过，
/// 成品听起来"少了一句"却没有任何信号。
#[derive(Debug, Clone)]
pub struct Assembled {
    pub duration: f64,
    pub wav: PathBuf,
    pub srt: PathBuf,
    pub done: usize,
    pub skipped: usize,
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
            voice_ref_hash: None,
            gap_ms,
            base_seed,
            sentences,
        }
    }

    /// 合成未完成的句子（或指定序号）。返回**失败的句数**：
    /// 全句失败时返回 `Ok(n>0)` 而不是 `Ok(0)` 那样的"成功"假象，
    /// 调用方据此决定是否继续拼装/退出码。
    pub fn synthesize(
        &mut self,
        client: &Client,
        dir: &Path,
        only: Option<&[usize]>,
        instruction: Option<&str>,
        on_progress: impl FnMut(usize, &str),
    ) -> Result<usize, ClientError> {
        self.synthesize_stoppable(client, dir, only, instruction, None, on_progress)
    }

    /// 带协作取消的合成：UI 的「停止合成」置 `stop` 位，句间检查（正在合成的那句
    /// 会跑完——不中断单个 HTTP 请求，避免服务端半状态）。取消时已完成的句保留。
    pub fn synthesize_stoppable(
        &mut self,
        client: &Client,
        dir: &Path,
        only: Option<&[usize]>,
        instruction: Option<&str>,
        stop: Option<&std::sync::atomic::AtomicBool>,
        mut on_progress: impl FnMut(usize, &str),
    ) -> Result<usize, ClientError> {
        std::fs::create_dir_all(dir.join("sentences")).ok();
        let instruction = instruction.unwrap_or(DEFAULT_INSTRUCTION);
        let mut failed = 0usize;
        // 按下标迭代而不是 iter_mut：循环体里要 self.save(dir)（逐句落盘），
        // iter_mut 会把 self.sentences 的可变借用一直占着，与 save 的 &self 冲突。
        for i in 0..self.sentences.len() {
            if stop
                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
            {
                on_progress(usize::MAX, "stopped");
                break;
            }
            let (index, spoken, seed) = {
                let s = &self.sentences[i];
                if let Some(list) = only {
                    if !list.contains(&s.index) {
                        continue;
                    }
                } else if s.status == "done" {
                    continue;
                }
                (s.index, s.spoken.clone(), s.seed)
            };
            on_progress(index, &spoken);
            let outcome = match client.synth(
                &self.model,
                &spoken,
                Some(seed),
                self.voice_ref.as_deref(),
                Some(instruction),
            ) {
                Ok(wav) => {
                    let path = dir.join(format!("sentences/{index:03}.wav"));
                    write_atomic(&path, &wav).map_err(|e| ClientError::Http(e.to_string()))?;
                    let d = wav_duration(&wav)?;
                    let s = &mut self.sentences[i];
                    s.duration = Some(d);
                    s.status = "done".into();
                    format!("done {d:.2}s")
                }
                Err(e) => {
                    failed += 1;
                    self.sentences[i].status = format!("error: {e}");
                    format!("error {e}")
                }
            };
            // 逐句落盘（Python `cmd_synth` 同款）：中途被杀/断电，已完成句与状态不丢。
            // 崩溃恢复 = 重跑 synthesize，done 句自动跳过。
            // 顺序必须是「先落盘、再回调」：回调里（UI 刷新/测试断言）读到的状态
            // 必须是磁盘上已持久化的状态。
            if let Err(e) = self.save(dir) {
                on_progress(index, &format!("工程落盘失败（续作仍可重跑）: {e}"));
            }
            on_progress(index, &outcome);
        }
        Ok(failed)
    }

    /// 单句重录（对应 Python `cmd_redo`）：换 seed → 可选改文本 → 重合成该句。
    ///
    /// **必须换 seed**：不换的话"重录"回来的是同一条不满意的音频。
    /// 重录后要刷新成品，调用方接着调 [`Project::assemble`]（Python 的 cmd_redo 也顺手重拼）。
    /// 返回值同 [`Project::synthesize`]：失败的句数。
    // 参数多与 `Project::new` 同理：client/dir/序号/新文本/规范化/instruction/回调
    // 都是调用方必须显式给出的决策，包成配置结构体只会让调用点更啰嗦
    #[allow(clippy::too_many_arguments)]
    pub fn redo(
        &mut self,
        client: &Client,
        dir: &Path,
        index: usize,
        new_text: Option<&str>,
        normalize: impl Fn(&str) -> String,
        instruction: Option<&str>,
        on_progress: impl FnMut(usize, &str),
    ) -> Result<usize, ClientError> {
        if let Some(t) = new_text {
            let s = self
                .sentences
                .iter_mut()
                .find(|s| s.index == index)
                .ok_or_else(|| ClientError::Http(format!("没有第 {index} 句")))?;
            s.text = t.to_string();
            s.spoken = normalize(t);
        }
        {
            let s = self
                .sentences
                .iter_mut()
                .find(|s| s.index == index)
                .ok_or_else(|| ClientError::Http(format!("没有第 {index} 句")))?;
            s.seed += REDO_SEED_STEP;
        }
        // 先落盘再合成（Python cmd_redo 修过的坑：synthesize 从磁盘重载工程时，
        // 未保存的文本/seed 修改会被静默丢弃，"重录"回来还是旧文本）。
        self.save(dir)
            .map_err(|e| ClientError::Http(format!("重录前落盘失败: {e}")))?;
        self.synthesize(client, dir, Some(&[index]), instruction, on_progress)
    }

    /// 拼装成品 + SRT（句子级时间轴）。返回值带**跳过的句数**，不再静默丢句。
    pub fn assemble(&mut self, dir: &Path) -> Result<Assembled, String> {
        let done: Vec<usize> = self
            .sentences
            .iter()
            .filter(|s| s.status == "done")
            .map(|s| s.index)
            .collect();
        if done.is_empty() {
            return Err("还没有已合成的句子".into());
        }
        let skipped = self.sentences.len() - done.len();

        // ── 拼装前逐句校验（Python cmd_assemble 同款）：任何一句有问题都中止并列出全部，
        // 不在拼到一半时才发现第 N 句是噪声。截断文件的证据不在 WAV 头里
        // （头仍合法、头里的帧数也不被截断改写），唯一可信判据是**实际字节数**。
        let spec = hound::WavReader::open(dir.join(format!("sentences/{:03}.wav", done[0])))
            .map_err(|e| e.to_string())?
            .spec();
        let mut bad: Vec<String> = Vec::new();
        for &idx in &done {
            let path = dir.join(format!("sentences/{idx:03}.wav"));
            let r = match hound::WavReader::open(&path) {
                Ok(r) => r,
                Err(e) => {
                    bad.push(format!("[{idx}] 不可读: {e}"));
                    continue;
                }
            };
            let got = r.spec();
            if (got.sample_rate, got.channels, got.bits_per_sample)
                != (spec.sample_rate, spec.channels, spec.bits_per_sample)
            {
                bad.push(format!(
                    "[{idx}] 参数不符 ({}Hz/{}ch/{}bit，首句 {}Hz/{}ch/{}bit)",
                    got.sample_rate,
                    got.channels,
                    got.bits_per_sample,
                    spec.sample_rate,
                    spec.channels,
                    spec.bits_per_sample
                ));
                continue;
            }
            let bytes_per_sample = got.bits_per_sample as u64 / 8;
            let need = 44 + r.duration() as u64 * got.channels as u64 * bytes_per_sample;
            let actual = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if actual < need {
                bad.push(format!(
                    "[{idx}] 文件被截断 (实际 {actual} 字节，头声明需要 {need} 字节)"
                ));
            }
        }
        if !bad.is_empty() {
            return Err(format!(
                "拼装中止，以下句子有问题：\n    {}",
                bad.join("\n    ")
            ));
        }

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
        let final_wav = out_dir.join("final.wav");
        // 成品走"临时文件 + fsync + 原子替换"：旧成品在写完前不受影响，
        // 掉电不会留下 rename 到位的空文件（Python cmd_assemble 同款）。
        let final_tmp = out_dir.join(format!("final.wav.tmp{}", std::process::id()));
        let mut writer = hound::WavWriter::create(&final_tmp, spec).map_err(|e| e.to_string())?;

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
            // 参数/截断校验已在拼装前统一做过；这里只读数据
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
        // 关文件后补一次 fsync 再 rename（wave 关闭不落盘到点，掉电可能留下空成品）
        std::fs::File::open(&final_tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&final_tmp, &final_wav).map_err(|e| e.to_string())?;
        let srt_path = out_dir.join("final.srt");
        write_atomic(&srt_path, srt.as_bytes()).map_err(|e| e.to_string())?;
        // 时间轴落进 project.json：下次打开工程/重录单句都从这里续
        self.save(dir).map_err(|e| e.to_string())?;
        Ok(Assembled {
            duration: cursor_frames as f64 / spec.sample_rate as f64,
            wav: final_wav,
            srt: srt_path,
            done: done.len(),
            skipped,
        })
    }

    /// 工程落盘（原子写：临时文件 + fsync + rename）。
    /// 崩溃/磁盘满时不会留下写了一半的 project.json。
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        write_atomic(
            &dir.join("project.json"),
            serde_json::to_string_pretty(self).unwrap().as_bytes(),
        )
    }

    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(dir.join("project.json"))?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// 原子写：同目录临时文件 + fsync + rename（Python `write_atomic` 同款）。
/// 崩溃/掉电/磁盘满时，目标路径要么还是旧内容，要么是新内容，不会写一半。
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
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

    /// 句首逗号不是断点：`cut == 0` 时若按字节当断点，会切出一个只含"，"的垃圾句
    #[test]
    fn leading_comma_is_not_a_break_point() {
        let text = "，开头就是逗号而且很长很长很长很长很长的句子。";
        assert_eq!(
            split_sentences(text, DEFAULT_PUNCTUATION, 12),
            vec![text.to_string()]
        );
    }

    #[test]
    fn collapses_blanks_and_keeps_no_empty_pieces() {
        assert_eq!(
            split_sentences("含  \t 多余空白。第二句。", DEFAULT_PUNCTUATION, 80),
            vec!["含 多余空白。", "第二句。"]
        );
        assert!(split_sentences("   \t ", DEFAULT_PUNCTUATION, 80).is_empty());
    }

    #[test]
    fn srt_timestamp_formats_millis() {
        assert_eq!(srt_timestamp(0.0), "00:00:00,000");
        assert_eq!(srt_timestamp(2.479), "00:00:02,479");
        assert_eq!(srt_timestamp(61.5), "00:01:01,500");
        assert_eq!(srt_timestamp(3661.25), "01:01:01,250");
    }
}
