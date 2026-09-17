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
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// 最近一次质检的可懂度（0..=100）。`None` = 没测过 / 已失效。
    ///
    /// 失效规则（UI 侧维护内存副本，两边要一致）：
    /// · 这句被重新合成（合成/重录都走 `synthesize_stoppable`）→ 清空；
    /// · 稿件改了 → `load_resumable` 判定文本不一致会重建工程，旧分数自然丢；
    /// · 只改工程名、续跑时跳过已完成的句子 → 保留（音频没变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_percent: Option<f64>,
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
    /// 文本兜底（数字/年份规范化）开关。**要持久化**：它是"这句该念什么"的一部分，
    /// 续作时开关变了而句子文本没变的话，不复用旧音频就会把旧读法留在成品里。
    /// 旧工程没有这个字段时按"开"处理（与历史行为一致）。
    #[serde(default = "default_auto_normalize")]
    pub auto_normalize: bool,
    /// 发音词典指纹（空 = 没启用词典）。词典改的是 spoken 文本，所以它变了旧音频不能复用
    /// ——与 `auto_normalize` 同类。旧工程没有这个字段时按"没启用词典"处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dict_hash: Option<String>,
    pub base_seed: u64,
    pub sentences: Vec<Sentence>,
}

fn default_auto_normalize() -> bool {
    true
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
        // 兜底开关由调用方在构造后按同一份输入设置（与 voice_ref_hash 同一模式）；
        // 这里给"开"是与历史行为一致的默认（旧工程/老调用点不受影响）。
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
                eval_percent: None,
            })
            .collect();
        Self {
            model: model.into(),
            voice_ref,
            voice_ref_hash: None,
            gap_ms,
            auto_normalize: true,
            dict_hash: None,
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
            // 这一句要重做：先把旧质检分数作废并**立刻落盘**，再做（可能失败的）重合成。
            //
            // 顺序是这条不变式的全部：先清后写 ⇒ 磁盘上永远不会出现"新音频 + 旧分数"。
            // 若反过来（先写 wav 再清分），进程在换 wav 与逐句 save 之间退出就会留下那个组合，
            // 重启后旧分会被贴到新音频上（复核指出）。代价是失败后这句没有分数——丢一个分数
            // 比显示一个错的分数好，重跑一次质检即可补回来。
            if self.sentences[i].eval_percent.take().is_some() {
                self.save(dir)
                    .map_err(|e| ClientError::Local(e.to_string()))?;
            }
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
                    write_atomic_explained(&path, &wav)
                        .map_err(|e| ClientError::Local(e.to_string()))?;
                    let d = wav_duration(&wav)?;
                    let s = &mut self.sentences[i];
                    s.duration = Some(d);
                    s.status = "done".into();
                    format!("done {d:.2}s")
                }
                Err(e) => {
                    failed += 1;
                    // `error: oom` 是队列可识别的失败标记；后面的文案仍由
                    // `ClientError` 的唯一 Display 入口生成，五条链路共用。
                    let status = if e.is_insufficient_memory() {
                        format!("error: oom: {e}")
                    } else {
                        format!("error: {e}")
                    };
                    self.sentences[i].status = status.clone();
                    status
                }
            };
            // 逐句落盘（Python `cmd_synth` 同款）：中途被杀/断电，已完成句与状态不丢。
            // 崩溃恢复 = 重跑 synthesize，done 句自动跳过。
            // 顺序必须是「先落盘、再回调」：回调里（UI 刷新/测试断言）读到的状态
            // 必须是磁盘上已持久化的状态。
            if let Err(e) = self.save(dir) {
                // 这里**不能**只回调一句就继续：那句会被报成 done，但 project.json 没有
                // 持久化，重跑时它又会重合成——"逐句落盘、重跑跳过已完成句"的不变式破了。
                // 直接中止，让调用方拿到同一条可执行文案。
                return Err(ClientError::Local(e.to_string()));
            }
            on_progress(index, &outcome);
        }
        Ok(failed)
    }

    /// 本轮失败（`error:` 前缀，含 OOM）的工程句子下标。
    ///
    /// 用户点「继续合成」时只喂这些下标，不再重跑已经 done 的句子；整轮
    /// 失败数也由同一份状态口径决定，不会把 done 误报成全失败。
    pub fn failed_sentence_indices(&self) -> Vec<usize> {
        self.sentences
            .iter()
            .filter(|s| s.status.starts_with("error"))
            .map(|s| s.index)
            .collect()
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
            .map_err(|e| ClientError::Local(format!("重录前落盘失败: {e}")))?;
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
        let first_path = dir.join(format!("sentences/{:03}.wav", done[0]));
        let spec = hound::WavReader::open(&first_path)
            .map_err(|e| sentence_read_note(&first_path, &e))?
            .spec();
        let mut bad: Vec<String> = Vec::new();
        for &idx in &done {
            let path = dir.join(format!("sentences/{idx:03}.wav"));
            let r = match hound::WavReader::open(&path) {
                Ok(r) => r,
                Err(e) => {
                    // 走同一套文案：说清是哪一句的哪个文件、该做什么（裸 os error 2 定位不到句）
                    bad.push(sentence_read_note(&path, &e));
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
        let mut writer = hound::WavWriter::create(&final_tmp, spec)
            .map_err(|e| hound_error_note(&final_wav, 0, &e))?;

        let gap_frames = (spec.sample_rate as u64 * self.gap_ms / 1000) as usize;
        let mut cursor_frames: u64 = 0;
        let mut srt = String::new();
        let mut srt_index = 0u32;
        // 句间静音只加在**成功句之间**：最后一句失败时，前面那个 done 句后面不该再补 gap
        // （否则成品尾部多一段静音；gap=2000 时就是多 2 秒）。
        let last_done = self
            .sentences
            .iter()
            .filter(|s| s.status == "done")
            .map(|s| s.index)
            .next_back();

        for s in self.sentences.iter_mut() {
            if s.status != "done" {
                s.start = None;
                continue;
            }
            let path = dir.join(format!("sentences/{:03}.wav", s.index));
            let mut r = hound::WavReader::open(&path).map_err(|e| sentence_read_note(&path, &e))?;
            // 参数/截断校验已在拼装前统一做过；这里只读数据
            let samples: Vec<i16> = r
                .samples::<i16>()
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            for v in &samples {
                writer
                    .write_sample(*v)
                    .map_err(|e| hound_error_note(&final_wav, 0, &e))?;
            }
            let frames = samples.len() / spec.channels as usize;
            let start = cursor_frames as f64 / spec.sample_rate as f64;
            s.start = Some(start);
            cursor_frames += frames as u64;
            if Some(s.index) != last_done {
                for _ in 0..(gap_frames * spec.channels as usize) {
                    writer
                        .write_sample(0i16)
                        .map_err(|e| hound_error_note(&final_wav, 0, &e))?;
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
        writer
            .finalize()
            .map_err(|e| hound_error_note(&final_wav, 0, &e))?;
        // 关文件后补一次 fsync 再 rename（wave 关闭不落盘到点，掉电可能留下空成品）
        std::fs::File::open(&final_tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| write_failure_note(&final_wav, 0, &e))?;
        std::fs::rename(&final_tmp, &final_wav)
            .map_err(|e| write_failure_note(&final_wav, 0, &e))?;
        let srt_path = out_dir.join("final.srt");
        write_atomic_explained(&srt_path, srt.as_bytes()).map_err(|e| e.to_string())?;
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
        write_atomic_explained(
            &dir.join("project.json"),
            serde_json::to_string_pretty(self).unwrap().as_bytes(),
        )
    }

    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(dir.join("project.json"))?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// 读取工程，**把「没有工程」与「工程读不了」分开**。
    ///
    /// - `Ok(None)`：`project.json` 不存在 → 全新工程，照旧从零建。
    /// - `Err(msg)`：文件在但读不了（截断 / 半截 JSON / 权限）→ 明确报错 + 路径 + 处置建议。
    ///
    /// 为什么不能像以前那样 `.ok()` 一丢了事：那等于把"工程损坏"当成"没有工程"，
    /// 用户已合成的句子会全部显示成待合成（且没有一句解释），随后第一次落盘还会把
    /// 损坏的 `project.json` 覆盖掉——唯一可人工恢复的现场就没了。
    pub fn load_if_present(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join("project.json");
        match Self::load(dir) {
            Ok(p) => Ok(Some(p)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // 与落盘文案同一条规矩：动作在前（界面会 elide），完整路径在后，解析细节最后
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(format!(
                "工程文件损坏：没有自动重建，也没有覆盖它——请把 project.json 改名或移走后重开\
                 （那样会按当前稿件从零合成），或先修好它。完整路径：{}。解析错误：{e}",
                path.display()
            )),
            Err(e) => Err(format!(
                "工程文件读不了：检查文件权限后重试；修不好就把它改名或移走再重开。\
                 完整路径：{}。原因：{e}",
                path.display()
            )),
        }
    }
}

/// 落盘失败的可执行文案：说清**哪个路径、要多少空间、下一步做什么**。
///
/// 原先的 `e.to_string()` 只会给出 `No space left on device (os error 28)`——
/// 用户既不知道是哪个目录（模型目录？导出目录？工程目录？），也不知道要释放多少。
pub fn write_failure_note(path: &Path, bytes: usize, err: &std::io::Error) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let need = if bytes == 0 {
        String::new()
    } else {
        format!("（需要 {mb:.1} MB）")
    };
    match err.kind() {
        // POSIX ENOSPC=28 / Windows ERROR_DISK_FULL=112：Rust 都归到 StorageFull。
        // 这条错误发生在临时文件阶段（write_atomic），目标文件与旧内容都还在。
        // 文案顺序是有意的：界面（状态栏 / 任务中心）都是 overflow: elide，
        // 先说"发生了什么 + 该做什么"，完整路径放最后——被截断时丢的是路径而不是动作。
        // 注意别在这里写 dub 专属的承诺（"已完成的句子会自动跳过"）：这个 helper 也被
        // BGM 分段 / 歌曲 / 分离复用，那些场景的续作语义各不相同。
        std::io::ErrorKind::StorageFull => format!(
            "磁盘空间不足{need}：请释放空间后重跑（已写好的文件不会被破坏）。路径：{}",
            path.display()
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "没有写入权限：检查该目录权限，或把工程/导出目录换到有权限的位置。路径：{}",
            path.display()
        ),
        std::io::ErrorKind::NotFound => format!(
            "路径不存在（父目录可能被删除或移动）：重建目录后再重跑。路径：{}",
            path.display()
        ),
        _ => format!(
            "写入失败：{err}。请检查磁盘与目录权限后重跑。路径：{}",
            path.display()
        ),
    }
}

/// 原子复制：同目录临时文件 + fsync + rename。
///
/// 导出以前用 `std::fs::copy`，失败时会留下**写了一半的目标文件**——所以
/// `write_failure_note` 里那句"已写好的文件不会被破坏"对导出并不成立（复核指出）。
/// 导出是用户交付物，同样值得原子化：要么旧文件不变，要么新文件完整。
pub fn copy_atomic(src: &Path, dst: &Path) -> std::io::Result<()> {
    let tmp = temp_sibling(dst);
    // 三步都算在结果里：只有 rename 成功才算落地。任何一步失败都清临时文件——
    // 只在 copy/sync 失败时清会漏掉"临时文件写完但 rename 失败（例如目标被目录占着）"，
    // 那种情况会在目录里留下 .tmp 残渣（复核抓到）。
    let result = std::fs::copy(src, &tmp)
        .and_then(|_| std::fs::File::open(&tmp).and_then(|f| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, dst));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 复制用的同目录临时文件路径。
///
/// 后缀必须**每次调用都不同**：只带进程 id 的话，同一进程里两个导出（单篇导出与批量
/// 导出同时写同一个目标名）会抢同一个 `.tmp`——两边各自 create/写/rename，轻则
/// `rename` 找不到文件，重则把对方写了一半的内容 rename 成"成品"。
fn temp_sibling(dst: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dst.with_file_name(format!(
        "{}.tmp{}-{n}",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ))
}

/// hound（wav 读写）的错误 → 可执行文案：`IoError` 能按 io 分类的就分类，
/// 其余原样带路径透出。拼装的流式写拿不到确切字节数，`bytes` 传 0，文案里就不提"需要多少"。
pub fn hound_error_note(path: &Path, bytes: usize, err: &hound::Error) -> String {
    match err {
        hound::Error::IoError(io) => write_failure_note(path, bytes, io),
        other => format!(
            "音频写入失败：{other}。请检查磁盘与目录权限后重跑。完整路径：{}",
            path.display()
        ),
    }
}

/// 路径的"最后两级"（`sentences/007.wav`）：被 elide 截断时，这比完整绝对路径更有用。
fn file_label(path: &Path) -> String {
    let mut parts: Vec<String> = path
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return path.display().to_string();
    }
    parts.reverse();
    parts.join("/")
}

/// 读句子 wav 失败（文件丢失 / 损坏）时的可执行文案。
///
/// 这条只在工程里该句状态是 `done` 时才会走到——也就是"状态说已合成，文件却不在"，
/// 必须说清是哪一句的哪个文件，否则用户只看到 `No such file or directory (os error 2)`。
fn sentence_read_note(path: &Path, err: &hound::Error) -> String {
    match err {
        // 同样按 elide 排序：先文件名（用户据此知道是哪一句），动作第二，完整路径最后
        hound::Error::IoError(io) if io.kind() == std::io::ErrorKind::NotFound => format!(
            "句子音频丢失：{}（工程里这句状态是「已合成」）。请重录该句，或把工程目录恢复回来。完整路径：{}",
            file_label(path),
            path.display()
        ),
        other => format!(
            "句子音频读不了：{}（{other}）。请重录该句。完整路径：{}",
            file_label(path),
            path.display()
        ),
    }
}

/// 原子写 + 失败时给出可执行文案。
///
/// 所有落盘都走它：`write_atomic` 的原始 io::Error 直接 `to_string()` 对用户没有可执行信息。
pub fn write_atomic_explained(path: &Path, data: &[u8]) -> std::io::Result<()> {
    write_atomic(path, data)
        .map_err(|e| std::io::Error::new(e.kind(), write_failure_note(path, data.len(), &e)))
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
    // `hound::WavReader::duration()` 返回的**已经是每声道帧数**（内部就是
    // `num_samples / channels`），这里不能再除一次 channels：多除一次会让所有
    // 立体声 wav 的时长正好少一半。配音链路一直是单声道（除不除都一样），
    // 歌曲成品 / 分离两轨是立体声——真机分离 e2e 才把这个错照出来。
    let frames = r.duration() as f64;
    Ok(frames / spec.sample_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 main.rs 同一个文本规范化入口（测试里只需要一个能跑的实例）。
    fn aw_crate_normalize(t: &str) -> String {
        crate::normalize(t, &Default::default())
    }

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

    /// 落盘失败的文案必须可执行：说清哪个路径、要多少空间、下一步做什么。
    /// 尤其 ENOSPC——原来的 `e.to_string()` 只有 `No space left on device (os error 28)`。
    #[test]
    fn write_failure_note_is_actionable_per_error_kind() {
        let path = Path::new("/tmp/音频作坊/projects/demo/sentences/012.wav");

        // POSIX ENOSPC=28（Rust 归到 StorageFull）；这条要先钉住错误映射本身，
        // 否则换工具链后 kind() 变了，测试会在别处莫名其妙地挂
        let enospc = std::io::Error::from_raw_os_error(28);
        assert_eq!(enospc.kind(), std::io::ErrorKind::StorageFull);
        let note = write_failure_note(path, 600 * 1024, &enospc);
        assert!(note.contains("磁盘空间不足"), "实得 {note}");
        assert!(note.contains("sentences/012.wav"), "要说清路径：{note}");
        assert!(note.contains("0.6 MB"), "要说清需要多少：{note}");
        assert!(
            note.contains("请释放空间后重跑") && note.contains("不会被破坏"),
            "要给出下一步与数据安全结论：{note}"
        );
        // 动作必须排在完整路径前面：状态栏/任务中心都是 elide，截断时丢的是尾巴
        let action_at = note.find("请释放空间").expect("要有动作");
        let path_at = note.find("/tmp/音频作坊").expect("要有完整路径");
        assert!(action_at < path_at, "动作不能排在长路径后面：{note}");

        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let note = write_failure_note(path, 1024, &denied);
        assert!(
            note.contains("没有写入权限") && note.contains("012.wav"),
            "{note}"
        );

        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let note = write_failure_note(path, 1024, &missing);
        assert!(
            note.contains("路径不存在") && note.contains("012.wav"),
            "{note}"
        );

        // 其他错误原样带上（不吞细节），但仍要说清路径
        let other = std::io::Error::other("未知的 IO 故障");
        let note = write_failure_note(path, 1024, &other);
        assert!(
            note.contains("写入失败") && note.contains("未知的 IO 故障"),
            "{note}"
        );
    }

    /// 原子写失败时要把分类文案一起带出来（调用方 to_string 后就有可执行信息）。
    #[test]
    fn write_atomic_explained_carries_the_actionable_note() {
        let dir = std::env::temp_dir().join(format!("aw-robust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 故意不建父目录：写临时文件必然失败
        let target = dir.join("nope/sentences/001.wav");
        let err = write_atomic_explained(&target, b"data").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("路径不存在"), "实得 {msg}");
        assert!(msg.contains("001.wav"), "实得 {msg}");
    }

    /// 「没有工程」与「工程坏了」必须分开：前者是从零开始，后者必须报错（不能静默重建，
    /// 否则已合成句全变待合成，而且下一次落盘会覆盖掉损坏文件这份现场）。
    #[test]
    fn load_if_present_separates_missing_from_corrupt_project() {
        let dir = std::env::temp_dir().join(format!("aw-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 没有 project.json：全新工程
        assert!(Project::load_if_present(&dir).unwrap().is_none());

        // 截断的 JSON：必须报错，且带上路径与处置建议
        let tool = |t: &str| aw_crate_normalize(t);
        let project = Project::new(
            "第一句。第二句。",
            "audio8-tts",
            250,
            831001,
            None,
            DEFAULT_PUNCTUATION,
            80,
            tool,
        );
        project.save(&dir).unwrap();
        let good = Project::load_if_present(&dir).unwrap().unwrap();
        assert_eq!(good.sentences.len(), 2, "正常工程照旧能读");

        std::fs::write(dir.join("project.json"), b"{\"sentences\": [{\"index\": 1,").unwrap();
        let err = Project::load_if_present(&dir).unwrap_err();
        assert!(err.contains("工程文件损坏"), "实得 {err}");
        assert!(err.contains("project.json"), "要说清哪个文件：{err}");
        assert!(err.contains("没有自动重建"), "要说清不会替他做决定：{err}");
        let action_at = err.find("请把 project.json").expect("要有动作");
        let path_at = err.find(&dir.display().to_string()).expect("要有完整路径");
        assert!(
            action_at < path_at,
            "动作要排在完整路径前（界面会 elide）：{err}"
        );

        // 读不了的场景里，损坏文件必须原样留着（本函数只读，不写）
        let raw = std::fs::read(dir.join("project.json")).unwrap();
        assert!(raw.starts_with(b"{\"sentences\""), "损坏文件不该被改写");
    }

    /// bytes=0（拼装是流式写，拿不到确切字节数）时不能写"需要 0.0 MB"；
    /// hound 的 IoError 也要能分类到磁盘满。
    #[test]
    fn zero_byte_write_note_omits_size_and_hound_errors_are_classified() {
        let path = Path::new("/tmp/音频作坊/projects/demo/out/final.wav");
        let enospc = std::io::Error::from_raw_os_error(28);
        let note = write_failure_note(path, 0, &enospc);
        assert!(note.contains("磁盘空间不足"), "{note}");
        assert!(!note.contains("MB"), "拿不到字节数就别提大小：{note}");
        assert!(note.contains("final.wav"), "{note}");

        let hound_err = hound::Error::IoError(std::io::Error::from_raw_os_error(28));
        let note = hound_error_note(path, 0, &hound_err);
        assert!(
            note.contains("磁盘空间不足") && note.contains("final.wav"),
            "{note}"
        );
    }

    /// 状态是 done 但句子 wav 不在：必须说清是哪一句的哪个文件（原来是裸的 os error 2）。
    #[test]
    fn missing_sentence_wav_says_which_file_is_gone() {
        let path = Path::new("/tmp/音频作坊/projects/demo/sentences/007.wav");
        let missing = hound::Error::IoError(std::io::Error::from(std::io::ErrorKind::NotFound));
        let note = sentence_read_note(path, &missing);
        assert!(note.contains("句子音频丢失"), "{note}");
        assert!(note.contains("sentences/007.wav"), "先给文件名：{note}");
        assert!(note.contains("已合成"), "要说清状态与文件不一致：{note}");
        let name_at = note.find("sentences/007.wav").unwrap();
        let full_at = note.find("/tmp/音频作坊").unwrap();
        assert!(name_at < full_at, "短标签要排在完整路径前：{note}");

        // 非 NotFound 也要带路径（损坏的 wav 同样要能定位）
        let broken = hound::Error::FormatError("bad header");
        let note = sentence_read_note(path, &broken);
        assert!(
            note.contains("句子音频读不了") && note.contains("007.wav"),
            "{note}"
        );
    }

    /// 复核抓到的漏洞：`assemble` 的**逐句预校验**会先打开句子文件，缺文件时旧实现只给
    /// 裸 `No such file or directory (os error 2)`，下面那条带文案的读取分支根本走不到。
    /// 这条走**真实 assemble 路径**（不直接调 helper），钉住"用户看到的到底是哪条文案"。
    #[test]
    fn assemble_reports_which_sentence_wav_is_missing() {
        let dir = std::env::temp_dir().join(format!("aw-assemble-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut project = Project::new(
            "第一句。第二句。",
            "audio8-tts",
            250,
            831001,
            None,
            DEFAULT_PUNCTUATION,
            80,
            |t| crate::normalize(t, &Default::default()),
        );
        for (i, s) in project.sentences.iter_mut().enumerate() {
            write_valid_sentence_wav(&dir, i, 2400);
            s.status = "done".into();
            s.duration = Some(0.1);
        }
        // 第一句的 wav 被外部删掉（工程里状态仍是 done）
        std::fs::remove_file(dir.join("sentences/000.wav")).unwrap();

        let err = project.assemble(&dir).unwrap_err();
        assert!(err.contains("句子音频丢失"), "不能是裸 os error：{err}");
        assert!(err.contains("sentences/000.wav"), "要指名哪一句：{err}");
        assert!(err.contains("请重录该句"), "要给出动作：{err}");
        assert!(
            !err.contains("os error 2"),
            "不该再把裸 errno 摆在用户面前：{err}"
        );
    }

    /// 写一个能被 assemble 预校验接受的句子 wav（24kHz/单声道/16bit，字节数与头声明一致）。
    fn write_valid_sentence_wav(dir: &Path, index: usize, frames: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = dir.join(format!("sentences/{index:03}.wav"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..frames {
            w.write_sample((i % 97) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 导出改原子复制后：失败不能留下半截目标文件，旧目标也不能被动过
    /// （复核指出 `std::fs::copy` 失败会截断目标，而文案声称"已写好的文件不会被破坏"）。
    #[test]
    fn copy_atomic_keeps_old_target_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("aw-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("src.wav");
        std::fs::write(&src, b"new-bytes").unwrap();
        let dst = dir.join("dst.wav");
        std::fs::write(&dst, b"old").unwrap();

        copy_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new-bytes");

        // 源不存在 → 失败，但**旧目标仍是完整内容**，且没有 .tmp 残渣
        let missing = dir.join("nope.wav");
        let err = copy_atomic(&missing, &dst).unwrap_err();
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            b"new-bytes",
            "失败的复制不该动到已有目标"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件：{leftovers:?}");
        // 源不存在 → 走 NotFound 分支：文案要是"重建目录后再重跑"这类动作，而不是裸 errno
        let note = write_failure_note(&dst, 0, &err);
        assert!(
            note.contains("重建目录后再重跑"),
            "复制失败也要给动作：{note}"
        );
    }

    /// rename 失败（目标被目录占着）也必须清掉临时文件——复核补抓到的那条分支：
    /// 只在 copy/sync 失败时清理会漏掉它，目录里会留下 `dst.wav.tmp<pid>`。
    #[test]
    fn copy_atomic_cleans_temp_when_rename_fails() {
        let dir = std::env::temp_dir().join(format!("aw-copy-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("src.wav");
        std::fs::write(&src, b"new-bytes").unwrap();
        // 目标位置是个目录：rename(file → dir) 必然失败
        let dst = dir.join("dst.wav");
        std::fs::create_dir_all(&dst).unwrap();

        let err = copy_atomic(&src, &dst).unwrap_err();
        assert!(
            std::fs::read_dir(&dst).unwrap().next().is_none(),
            "失败不该把内容塞进目标目录"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "rename 失败也要清临时文件：{leftovers:?}"
        );
        assert!(err.raw_os_error().is_some(), "应是真实 io 错误：{err}");
    }

    /// 复制用的临时文件名必须**每次调用都不同**：同一进程里单篇导出与批量导出可能同时
    /// 写同一个目标名，只带 pid 的后缀会让它们抢同一个 `.tmp`。
    #[test]
    fn copy_temp_names_are_unique_per_call() {
        let dst = PathBuf::from("/tmp/工程.wav");
        let a = temp_sibling(&dst);
        let b = temp_sibling(&dst);
        assert_ne!(a, b, "两次调用不能拿到同一个临时文件");
        assert_eq!(
            a.parent(),
            dst.parent(),
            "临时文件必须在目标同目录（rename 才原子）"
        );
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("工程.wav.tmp"),
            "临时名要能看出是哪个目标的：{}",
            a.display()
        );
    }

    /// 句间停顿真的进了成品：同样两句（各 0.1s），gap 0 与 200ms 的成品时长差 0.2s。
    /// 这条是"停顿设置真的生效"的可执行证据（不是只看字段被赋值）。
    #[test]
    fn assemble_gap_changes_product_duration() {
        let dir = std::env::temp_dir().join(format!("aw-assemble-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = Project::new(
            "第一句。第二句。",
            "audio8-tts",
            0,
            831001,
            None,
            DEFAULT_PUNCTUATION,
            80,
            |t| crate::normalize(t, &Default::default()),
        );
        for (i, s) in project.sentences.iter_mut().enumerate() {
            write_valid_sentence_wav(&dir, i, 2400); // 2400 帧 @24k = 0.1s
            s.status = "done".into();
            s.duration = Some(0.1);
        }

        project.gap_ms = 0;
        let tight = project.assemble(&dir).unwrap();
        project.gap_ms = 200;
        let loose = project.assemble(&dir).unwrap();

        assert!(
            (tight.duration - 0.2).abs() < 0.02,
            "gap=0 时成品应≈0.2s，实得 {}",
            tight.duration
        );
        assert!(
            (loose.duration - 0.4).abs() < 0.02,
            "gap=200ms 时成品应≈0.4s，实得 {}",
            loose.duration
        );
    }

    /// 末尾句失败时不该在成品尾部留一段句间静音（复核抓到：`k != last` 用的是句数组的
    /// 最后一项，而那一项是失败句）。三句、只有前两句 done：成品 = 两句音频 + 1 个 gap。
    #[test]
    fn assemble_skips_trailing_gap_when_last_sentence_failed() {
        let dir = std::env::temp_dir().join(format!("aw-assemble-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = Project::new(
            "第一句。第二句。第三句。",
            "audio8-tts",
            200,
            831001,
            None,
            DEFAULT_PUNCTUATION,
            80,
            |t| crate::normalize(t, &Default::default()),
        );
        let mut done = 0usize;
        for (i, s) in project.sentences.iter_mut().enumerate() {
            if i < 2 {
                write_valid_sentence_wav(&dir, i, 2400); // 0.1s each
                s.status = "done".into();
                s.duration = Some(0.1);
                done += 1;
            } else {
                // 第三句失败：不写 wav、状态不是 done
                s.status = "error: 服务端失败".into();
            }
        }
        let a = project.assemble(&dir).unwrap();
        assert_eq!(a.done, done);
        assert_eq!(a.skipped, 1);
        // 0.1 + 0.2 + 0.1 = 0.4（末尾没有 gap）
        assert!(
            (a.duration - 0.4).abs() < 0.02,
            "末尾不该留静音：实得 {}",
            a.duration
        );
    }

    /// `wav_duration` 的声道口径：`hound` 的 `duration()` 返回的**已经是每声道帧数**
    /// （内部就是 `num_samples / channels`），所以不能再除一次 channels。
    /// 配音链路一直是单声道（除不除都一样），立体声的歌曲成品 / 分离两轨才把它露出来：
    /// 真机分离 e2e 里 3.000s 的立体声轨被算成 1.500s。
    #[test]
    fn wav_duration_counts_frames_not_samples_for_stereo() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
            for _ in 0..(48_000 * 2) {
                // 1 秒 × 两声道
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        let secs = wav_duration(buf.get_ref()).unwrap();
        assert!(
            (secs - 1.0).abs() < 1e-9,
            "立体声 1 秒应读成 1.0s，实得 {secs}"
        );

        // 单声道不能被这次修改带歪（配音链路走的就是它）
        let mono = hound::WavSpec {
            channels: 1,
            ..spec
        };
        let mut mbuf = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut mbuf, mono).unwrap();
            for _ in 0..48_000 {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        let mono_secs = wav_duration(mbuf.get_ref()).unwrap();
        assert!(
            (mono_secs - 1.0).abs() < 1e-9,
            "单声道 1 秒应读成 1.0s，实得 {mono_secs}"
        );
    }
}
