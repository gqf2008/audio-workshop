//! 参考音频（音色克隆的 `voice_ref`）的时长读取、15 秒上限判据与超长自动裁剪。
//!
//! 为什么需要（2026-09-22 真机复现，随包 v0.8.2-metalbf16）：
//! 192.9s 的参考音频发 audio8-tts 克隆请求 → 引擎按参考时长膨胀图尺寸，向 Metal
//! 申请 14,539 MiB buffer → 分配失败 → `ggml_metal_buffer_is_shared` 空指针
//! **SIGSEGV，整个引擎进程死掉**（端口消失）。同一文件裁到 20s → HTTP 200
//! （11.3s 出音频）。分配规模约 75 MiB/参考秒。
//!
//! 上一批（30s 硬上限）在四个克隆入口**拦死**超长参考音；本批
//! （thread cc-ai-audio-workshop-ref-limit-15s）把上限收紧到 15s，并改为
//! **自动只取前 15 秒**：原文件不动，应用生成一份本地 wav 副本用于克隆
//! （[`trim_reference_first_seconds`]），裁剪失败才退回红色拦截文案
//! （[`reference_too_long`]）。读取与判据都只在这里一份：main 侧只负责在入口处
//! 调用 [`reference_over_limit`] 与 [`trim_reference_first_seconds`]。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// 参考音频硬上限（秒）。
///
/// 依据：UI 引导文案「5–15 秒干净人声」；15s × 约 75 MiB/参考秒 ≈ 1.1 GiB，
/// 在常见机器上是安全规模（192.9s 实测要 14.5 GiB 直接把引擎进程打死）。
/// 放宽/收紧只改这一个常量（判据文案与裁剪目标都由它生成）；UI/静态引导文案由
/// `reference_range_label_is_pinned_to_static_copy` 守卫测试钉住——改常量必须同步那几处字面量。
pub const REFERENCE_MAX_SECONDS: f64 = 15.0;

/// UI 引导用的范围标签（如 `5–15 秒`）：数字由 [`REFERENCE_MAX_SECONDS`] 生成。
pub fn reference_range_label() -> String {
    format!("5–{:.0} 秒", REFERENCE_MAX_SECONDS)
}

/// 读参考音频的时长（秒）。读不出 → `None`（**fail-open**，取舍见下）。
///
/// - wav：`dub::wav_file_duration`（hound 只读 RIFF 头，不读样本）；
/// - 非 wav（mp3/flac…）：symphonia 探测音轨参数（与 `separate::probe_sample_rate`
///   同一条依赖链——上游自己就用它解码，这里复用读头，不另造解码逻辑）。
///
/// **fail-open 的取舍（注释写清）**：这个护栏只拦「时长读得出来且确实超 15s」的
/// 输入。读不出（文件损坏 / 权限 / 不支持的容器如 m4a / VBR mp3 头里没有帧数）时
/// 返回 `None` 放行——最坏结果与没有这个护栏完全一样（引擎自己报错或崩），而
/// fail-closed 会把"时长探测坏了"放大成"克隆功能整体不可用"。探测是纯本地读头，
/// 失败面小；192.9s / 20s 两组对照另有真机验收。
pub fn reference_duration_seconds(path: &Path) -> Option<f64> {
    // wav 优先（配音链路的主力输入）。读不出时交给 symphonia 再试一次（wav 扩展名
    // 错误、或根本是别的格式），两条路都走不通才 fail-open。
    if let Ok(d) = crate::dub::wav_file_duration(path) {
        return Some(d);
    }
    symphonia_duration(path)
}

/// 用 symphonia 探测非 wav 输入的时长：`n_frames × time_base`，拿不到 time_base
/// （或畸形）时退回 `n_frames / sample_rate` 的近似口径。只读容器头/音轨参数，
/// 不解码样本。
fn symphonia_duration(path: &Path) -> Option<f64> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).ok()?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, stream, &Default::default(), &Default::default())
        .ok()?;
    let track = probed.format.default_track()?;
    let frames = track.codec_params.n_frames? as f64;
    match track.codec_params.time_base {
        Some(tb) if tb.numer > 0 && tb.denom > 0 => {
            Some(frames * tb.numer as f64 / tb.denom as f64)
        }
        // 没有 time_base：退回 n_frames / sample_rate（mp3 等常见头里两者等价）。
        _ => {
            let rate = track.codec_params.sample_rate? as f64;
            (rate > 0.0).then_some(frames / rate)
        }
    }
}

/// 纯判据：超过 [`REFERENCE_MAX_SECONDS`] 返回可执行文案，否则 `None`（放行）。
///
/// 本批起这条判据的语义是「自动裁剪失败后的红色拦截」：正常路径下超长会先被
/// [`trim_reference_first_seconds`] 自动取前 15 秒（main 侧
/// `prepare_reference_for_clone` 拿裁剪失败的原因组完整文案），只有裁剪失败才
/// 走到这条拦截。文案有意按「实际秒数 → 上限 → 为什么 → 该做什么」排：状态栏是
/// `overflow: elide`，被截断时丢掉的是尾巴，动作句必须靠前
/// （与 docs/robustness.md §2 的文案顺序原则同一条）。
pub fn reference_too_long(seconds: f64) -> Option<String> {
    if seconds > REFERENCE_MAX_SECONDS {
        Some(format!(
            "参考音频 {seconds:.1} 秒，超过 {:.0} 秒上限：引擎按参考时长申请内存（约 75 MiB/秒，193 秒实测申请 14,539 MiB 后引擎崩溃），超长会直接打死引擎进程。请裁到 {:.0} 秒内再合成。",
            REFERENCE_MAX_SECONDS, REFERENCE_MAX_SECONDS
        ))
    } else {
        None
    }
}

/// 读时长 + 判据的组合入口（读不出时长 = fail-open 放行）。
pub fn reference_over_limit(path: &Path) -> Option<String> {
    reference_duration_seconds(path).and_then(reference_too_long)
}

// ── 超长自动裁剪 ──

/// 超长参考音频只取前 `max_seconds` 的裁剪副本（**原文件不动**）。
///
/// - wav：hound 读头 + 逐帧拷贝前 `max_seconds`（保留原采样率/声道/位深；
///   PCM 8/16/24/32 位与 float 都能读）；
/// - 非 wav（mp3/flac…）：symphonia 解码取前 `max_seconds` 的样本 → 写成 wav
///   （s16le，保留源采样率/声道）——与 [`reference_duration_seconds`] 的探测同一套
///   依赖，只有这里才真正解码；
/// - 目标文件名 `<slug>-<hash8(规范路径|size|mtime)>-<max>s.wav`：同一源稳定复用；
///   mtime 进哈希 → 源文件更新后不会复用旧 mtime 的陈旧副本；
/// - **原子写**：同目录临时文件 + fsync + rename（与 `dub` 拼装 final.wav 同一写法），
///   失败不留半截；
/// - 源 ≤ max：直接返回源路径（不复制、不建目录）。
pub fn trim_reference_first_seconds(
    src: &Path,
    dest_dir: &Path,
    max_seconds: f64,
) -> Result<PathBuf, String> {
    let secs = reference_duration_seconds(src)
        .ok_or_else(|| format!("读不出参考音频时长，无法裁剪：{}", src.display()))?;
    if secs <= max_seconds {
        return Ok(src.to_path_buf());
    }
    let dest = trimmed_dest_path(src, dest_dir, max_seconds)?;
    if dest.is_file() {
        // 同名副本已存在（同路径、同 size、同 mtime）：直接复用，不重新裁。
        return Ok(dest);
    }
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("建裁剪副本目录失败（{}）: {e}", dest_dir.display()))?;
    let tmp = temp_sibling_of(&dest);
    let outcome = match hound::WavReader::open(src) {
        Ok(mut reader) => {
            let spec = reader.spec();
            let frames = (max_seconds * spec.sample_rate as f64).round() as usize;
            copy_wav_prefix(&mut reader, spec, frames, &tmp)
        }
        // 不是（或读不动）wav：走 symphonia 真解码。
        Err(_) => decode_first_seconds_to_wav(src, max_seconds, &tmp),
    };
    finish_atomic(&tmp, &dest, outcome)
}

/// 目标文件名：`<slug>-<hash8(规范路径|size|mtime)>-<max>s.wav`。
fn trimmed_dest_path(src: &Path, dest_dir: &Path, max_seconds: f64) -> Result<PathBuf, String> {
    let meta =
        std::fs::metadata(src).map_err(|e| format!("参考音频读不了（{}）: {e}", src.display()))?;
    // 规范路径消掉 `./..` 等别名差异；canonicalize 失败（少见）退回原路径。
    let canonical = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(b"|");
    hasher.update(meta.len().to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(mtime_nanos(&meta).to_string().as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    Ok(dest_dir.join(format!(
        "{}-{}-{}s.wav",
        slug_of(src),
        &hex[..8],
        secs_label(max_seconds)
    )))
}

/// 文件名里的秒数：整秒不带小数点（`15`），非整秒保留原样（`0.19`）——
/// 不能用 `{:.0}`，那会把 0.1 与 0.19 都压成 `0`、两个不同目标的副本撞名互用。
fn secs_label(secs: f64) -> String {
    if secs.fract() == 0.0 && secs.is_finite() {
        format!("{secs:.0}")
    } else {
        format!("{secs}")
    }
}

/// 文件的修改时间（纳秒）；文件系统没给 mtime 时用 0（只影响副本复用命中）。
fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 文件名 slug：只留字母数字与 `._-`，其余换成 `_`（路径/特殊字符不进文件名）。
fn slug_of(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".into());
    let mut out = String::with_capacity(stem.len());
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("audio");
    }
    out
}

/// 裁剪用的同目录临时文件路径。后缀每次调用都不同（与 `dub::temp_sibling` 同一
/// 理由：两个裁剪并发写同一个目标名时会抢同一个 `.tmp`）。
fn temp_sibling_of(dest: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(
        ".{}.tmp{}-{n}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("trim"),
        std::process::id()
    ))
}

/// 临时文件写完 → fsync → rename；任何一步失败都清临时文件（不留半截）。
///
/// 与 `dub` 的 final.wav 同一套写法：关文件后补一次 fsync 再 rename
/// （writer 关闭不落盘到点，掉电可能留下空成品）。
fn finish_atomic(tmp: &Path, dest: &Path, outcome: Result<(), String>) -> Result<PathBuf, String> {
    let result = outcome
        .and_then(|()| {
            crate::dub::sync_file(tmp)
                .map_err(|e| format!("裁剪副本落盘失败（{}）: {e}", dest.display()))
        })
        .and_then(|()| {
            std::fs::rename(tmp, dest)
                .map_err(|e| format!("裁剪副本落盘失败（{}）: {e}", dest.display()))
        });
    if result.is_err() {
        // 三步都算在结果里：只有 rename 成功才算落地。任何一步失败都清临时文件——
        // 只在拷贝/解码失败时清会漏掉"临时文件写完但 rename 失败"的残渣
        // （与 dub::copy_atomic 同一教训）。
        let _ = std::fs::remove_file(tmp);
        return Err(result.expect_err("刚判过 is_err"));
    }
    Ok(dest.to_path_buf())
}

/// wav 裁剪：读头（规格照抄）→ 逐帧拷贝前 `frames` 帧。样本类型按
/// （sample_format, bits_per_sample）分派：PCM 8/16/24/32 位与 float 都能读
/// （hound 的 `samples::<T>()` 只接受与文件位深匹配的 T，必须分派）。
fn copy_wav_prefix(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    spec: hound::WavSpec,
    frames: usize,
    dest: &Path,
) -> Result<(), String> {
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 8) => copy_typed::<i8>(reader, spec, frames, dest),
        (hound::SampleFormat::Int, 16) => copy_typed::<i16>(reader, spec, frames, dest),
        (hound::SampleFormat::Int, 24) | (hound::SampleFormat::Int, 32) => {
            copy_typed::<i32>(reader, spec, frames, dest)
        }
        (hound::SampleFormat::Float, 32) => copy_typed::<f32>(reader, spec, frames, dest),
        other => Err(format!(
            "不支持的 wav 样本格式（{other:?}）：请换 8/16/24/32 位 PCM 或 32 位 float"
        )),
    }
}

/// 按类型逐帧拷贝：源比声明短（头与数据不符的损坏文件）会在这里读报错，
/// 由 [`finish_atomic`] 收尾清理。
fn copy_typed<T: hound::Sample>(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    spec: hound::WavSpec,
    frames: usize,
    dest: &Path,
) -> Result<(), String> {
    let mut writer = hound::WavWriter::create(dest, spec)
        .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))?;
    let limit = frames.saturating_mul(spec.channels as usize);
    let mut written = 0usize;
    for sample in reader.samples::<T>() {
        let v = sample.map_err(|e| format!("参考音频数据读不了（文件可能损坏或被截断）：{e}"))?;
        writer
            .write_sample(v)
            .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))?;
        written += 1;
        if written >= limit {
            break;
        }
    }
    writer
        .finalize()
        .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))
}

/// 非 wav（mp3/flac…）：symphonia 解码 → 取前 `max_seconds` 的样本 → s16le wav
/// （保留源采样率/声道）。**只解码到 `max_seconds` 就停**——超长文件不必整体解码。
fn decode_first_seconds_to_wav(src: &Path, max_seconds: f64, dest: &Path) -> Result<(), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(src)
        .map_err(|e| format!("参考音频读不了（{}）: {e}", src.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("不支持或损坏的音频格式：{e}"))?;
    let track_id = probed
        .format
        .default_track()
        .map(|t| t.id)
        .ok_or_else(|| "音频里没有可解码的音轨".to_string())?;
    // 先复制一份参数再借 format：`default_track` 的引用会一直占着 probed.format 的
    // 不可变借用，与下面循环里的 `next_packet`（可变借用）打架。
    let params = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.id == track_id)
        .map(|t| t.codec_params.clone())
        .ok_or_else(|| "默认音轨不见了".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("该格式没有可用的解码器：{e}"))?;

    let mut out: Vec<i16> = Vec::new();
    let mut wav_spec: Option<hound::WavSpec> = None;
    let mut remaining: Option<u64> = None;
    loop {
        if matches!(remaining, Some(0)) {
            break;
        }
        let packet = match probed.format.next_packet() {
            Ok(p) => p,
            // 正常文件尾与"文件损坏"分开处理：前者直接收尾，后者报错。
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            // 部分流式容器（mp3）结尾以复位信号收尾，同样算正常结束。
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("读音频包失败：{e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // 个别包解不出来（流式 mp3 常见）跳过，不让整段裁剪失败。
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("解码失败：{e}")),
        };
        let signal = *decoded.spec();
        if signal.rate == 0 {
            continue;
        }
        let rate = signal.rate as u64;
        let channels = signal.channels.count();
        let limit = *remaining.get_or_insert((max_seconds * rate as f64).round() as u64);
        let mut sbuf = SampleBuffer::<f32>::new(decoded.capacity() as u64, signal);
        sbuf.copy_interleaved_ref(decoded);
        let samples = sbuf.samples();
        // 截到帧边界：半帧会让 wav 数据长度不对齐，时长也说不清。
        let mut take = (limit as usize).saturating_mul(channels).min(samples.len());
        take -= take % channels;
        if take == 0 {
            break;
        }
        let spec = wav_spec.get_or_insert(hound::WavSpec {
            channels: channels as u16,
            sample_rate: signal.rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        });
        if spec.channels != channels as u16 || spec.sample_rate != signal.rate {
            return Err("音频流中途变了采样率/声道数，无法裁剪".to_string());
        }
        for &s in &samples[..take] {
            out.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
        }
        remaining = Some(limit - take as u64 / channels as u64);
    }
    let Some(wav_spec) = wav_spec else {
        return Err("没能从参考音频解码出任何样本".to_string());
    };
    // 尾部对齐：最后一包可能带出半帧，按帧边界截齐。
    let frames = out.len() / wav_spec.channels as usize;
    out.truncate(frames * wav_spec.channels as usize);
    let mut writer = hound::WavWriter::create(dest, wav_spec)
        .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))?;
    for v in &out {
        writer
            .write_sample(*v)
            .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))?;
    }
    writer
        .finalize()
        .map_err(|e| crate::dub::hound_error_note(dest, 0, &e))
}

// ── 裁剪副本的「参考文本」旁车 ──

/// 旁车文件后缀：`<trimmed>.wav.reftext.txt`。只对**裁剪副本**写，原文件永远没有。
pub const REFERENCE_TEXT_SIDECAR_SUFFIX: &str = ".reftext.txt";

/// 裁剪副本对应的参考文本旁车路径。
pub fn reference_text_sidecar_path(trimmed: &Path) -> PathBuf {
    let mut s = trimmed.as_os_str().to_owned();
    s.push(REFERENCE_TEXT_SIDECAR_SUFFIX);
    PathBuf::from(s)
}

/// 读旁车里的参考文本；没有或为空 → `None`（调用方回退到用户文本）。
pub fn paired_reference_text(trimmed: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(reference_text_sidecar_path(trimmed)).ok()?;
    let text = raw.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 原子写旁车（临时文件 + fsync + rename，与裁剪副本同一套写法）。
pub fn write_reference_text_sidecar(trimmed: &Path, text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        // 空文本不落盘：旁车语义是「这段音频念的内容」，空等于没有。
        return Ok(());
    }
    let dest = reference_text_sidecar_path(trimmed);
    let tmp = temp_sibling_of(&dest);
    let outcome = std::fs::write(&tmp, text)
        .map_err(|e| format!("参考文本旁车落盘失败（{}）: {e}", dest.display()));
    finish_atomic(&tmp, &dest, outcome).map(|_| ())
}

/// 估算语速上限（字/秒）：ASR 不可用时兼底截断用。中文旁白常见 4–6 字/秒，
/// 取下界 5.0 宁短勿长（略短于实际只会丢掉最后几个字，强于把全文塞给引擎）。
pub const ESTIMATED_CHARS_PER_SECOND: f64 = 5.0;

/// 按字数上限截断参考文本（尽量停在句末标点；无标点时按字数硬截）。
pub fn truncate_reference_text_to_chars(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.is_empty() || max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // 句末标点之后的位置（字节下标）；循环结束后是预算内最后一个句末标点。
    let mut cut: Option<usize> = None;
    for (idx, ch) in text.char_indices().take(max_chars) {
        if matches!(ch, '。' | '！' | '？' | '!' | '?' | '…' | '；' | ';' | '\n') {
            cut = Some(idx + ch.len_utf8());
        }
    }
    let end = cut.unwrap_or_else(|| {
        text.char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(text.len())
    });
    text[..end].trim().to_string()
}

/// 比例截断：按「保留秒数 / 原时长」把用户文本截到近似长度。纯函数（零依赖兜底）。
///
/// 只用于 ASR 不可用时的兜底：同一段录音里语速大致均匀，按比例取前缀是最近似的匹配。
pub fn proportional_reference_text(text: &str, kept_seconds: f64, full_seconds: f64) -> String {
    let text = text.trim();
    if text.is_empty() || !kept_seconds.is_finite() || !full_seconds.is_finite() {
        return String::new();
    }
    if full_seconds <= 0.0 || kept_seconds <= 0.0 {
        return String::new();
    }
    if kept_seconds >= full_seconds {
        return text.to_string();
    }
    let budget = (text.chars().count() as f64 * (kept_seconds / full_seconds)).ceil() as usize;
    truncate_reference_text_to_chars(text, budget.max(1))
}

/// 原时长不可知时（已迁移成裁剪副本的旧工程）按估算语速截断。
pub fn estimated_reference_text(text: &str, seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return String::new();
    }
    let cap = (seconds * ESTIMATED_CHARS_PER_SECOND).ceil() as usize;
    truncate_reference_text_to_chars(text, cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个真实 wav（时长 × 采样率，单声道 16bit 静音）。用 8kHz：
    /// 192.9s 也只有约 3MB，测试写得起；时长读取出自 RIFF 头，与内容无关。
    fn write_wav(path: &Path, seconds: f64, rate: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = (seconds * rate as f64).round() as usize;
        for _ in 0..frames {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 非静音的 wav：样本值 = 帧号取模（能验证副本内容是**前 N 帧**而不是别的一段）。
    fn write_pattern_wav(path: &Path, seconds: f64, rate: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = (seconds * rate as f64).round() as usize;
        for i in 0..frames {
            w.write_sample((i % 30_000) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aw-reflimit-{tag}-{}.wav", std::process::id()))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-reflimit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 夹具定位与 separate.rs 的 fixture_path 同一策略：先查运行期 cwd（跨
    /// worktree 复用 target 时编译期 CARGO_MANIFEST_DIR 可能指向已删的旧路径），
    /// 再回落编译期清单根。
    fn fixture(rel: &str) -> PathBuf {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(rel));
        }
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel));
        candidates
            .iter()
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("找不到夹具 {rel}"))
            .clone()
    }

    /// 10s 放行 / 15.0 在上限上放行 / 15.01 拦 / 192.9s 拦，且文案含实际秒数 +
    /// 「裁到 15 秒内再合成」+ 为什么。
    #[test]
    fn criterion_allows_10s_and_15s_and_blocks_15_01s_with_actionable_message() {
        assert!(reference_too_long(10.0).is_none(), "10s 放行");
        assert!(
            reference_too_long(15.0).is_none(),
            "正好 15.0s 在上限上，放行"
        );
        assert!(reference_too_long(15.01).is_some(), "15.01s 必须拦");
        let note = reference_too_long(192.9).expect("192.9s 必须拦");
        assert!(note.contains("192.9"), "文案要带实际秒数：{note}");
        assert!(note.contains("15"), "文案要带上限：{note}");
        assert!(note.contains("裁到 15 秒内再合成"), "文案要带动作：{note}");
        assert!(note.contains("75 MiB"), "文案要说清为什么：{note}");
    }

    /// 真文件端到端：10s wav 放行、15.01s wav 拦截（读取出自 RIFF 头，口径与
    /// 判据同一条链路）。
    #[test]
    fn real_10s_wav_passes_and_real_15_01s_wav_is_blocked() {
        let p10 = temp_file("ok");
        write_wav(&p10, 10.0, 8000);
        let d = reference_duration_seconds(&p10).expect("能读的 wav 必须有时长");
        assert!((d - 10.0).abs() < 0.1, "时长应约 10s：{d}");
        assert!(reference_over_limit(&p10).is_none(), "10s 放行");
        let _ = std::fs::remove_file(&p10);

        let p1501 = temp_file("long");
        write_wav(&p1501, 15.01, 8000);
        let note = reference_over_limit(&p1501).expect("15.01s 必须拦");
        assert!(note.contains("15.0"), "{note}");
        assert!(note.contains("裁到 15 秒内再合成"), "{note}");
        let _ = std::fs::remove_file(&p1501);
    }

    /// 读不出时长必须 fail-open：不存在的路径与垃圾字节都返回 None（放行）。
    #[test]
    fn unreadable_input_fails_open() {
        let missing =
            std::env::temp_dir().join(format!("aw-reflimit-missing-{}.wav", std::process::id()));
        assert!(reference_duration_seconds(&missing).is_none());
        assert!(
            reference_over_limit(&missing).is_none(),
            "读不出时长必须放行（fail-open）"
        );

        let garbage = temp_file("garbage");
        std::fs::write(&garbage, b"this is not audio at all, just bytes").unwrap();
        assert!(reference_duration_seconds(&garbage).is_none());
        assert!(
            reference_over_limit(&garbage).is_none(),
            "损坏文件同样 fail-open"
        );
        let _ = std::fs::remove_file(&garbage);
    }

    /// 非 wav 走 symphonia 探测：flac 夹具（0.2s）应读出时长且远低于上限。
    #[test]
    fn symphonia_probe_reads_non_wav_duration() {
        let path = fixture("tests/fixtures/probe-0.2s.flac");
        let d = reference_duration_seconds(&path).expect("flac 夹具应读得出时长");
        assert!((d - 0.2).abs() < 0.15, "probe-0.2s.flac 应约 0.2s：{d}");
        assert!(reference_over_limit(&path).is_none());
    }

    /// wav 20s → 副本时长 ≈15s、规格不变、内容是**前 15 秒**；wav 10s → 返回原路径。
    #[test]
    fn trim_wav_copies_first_15s_preserving_spec_and_short_returns_source() {
        let dir = temp_dir("trim-wav");
        let src = dir.join("ref-20s.wav");
        write_pattern_wav(&src, 20.0, 8000);

        let out = trim_reference_first_seconds(&src, &dir, 15.0).expect("20s 应能裁剪");
        assert_ne!(out, src, "超长必须产副本，不能返回原路径");
        let d = reference_duration_seconds(&out).expect("副本应是合法 wav");
        assert!((d - 15.0).abs() < 0.01, "副本时长应约 15s：{d}");

        let mut r_in = hound::WavReader::open(&src).unwrap();
        let mut r_out = hound::WavReader::open(&out).unwrap();
        assert_eq!(r_out.spec(), r_in.spec(), "采样率/声道/位深必须原样保留");
        let prefix: Vec<i16> = r_in
            .samples::<i16>()
            .take(15 * 8000)
            .collect::<Result<_, _>>()
            .unwrap();
        let copied: Vec<i16> = r_out.samples::<i16>().collect::<Result<_, _>>().unwrap();
        assert_eq!(copied, prefix, "副本必须是原文件的前 15 秒");
        assert_eq!(copied.len(), 15 * 8000, "不能多拷 15 秒之后的样本");

        // 10s → 返回原路径，不复制
        let short = dir.join("ref-10s.wav");
        write_pattern_wav(&short, 10.0, 8000);
        assert_eq!(
            trim_reference_first_seconds(&short, &dir, 15.0).expect("10s 直接放行"),
            short,
            "10s 应原样返回源路径"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 wav 路径真能解码裁剪：flac 夹具裁到 0.1s 出 wav，内容非静音、
    /// 且与裁到 0.19s 的副本共享前缀（证明取的是**开头**而不是任意一段）。
    #[test]
    fn trim_non_wav_decodes_flac_prefix_to_wav() {
        let flac = fixture("tests/fixtures/probe-0.2s.flac");
        let dir = temp_dir("trim-flac");

        let out01 = trim_reference_first_seconds(&flac, &dir, 0.1).expect("flac 应能解码裁剪");
        let d01 = reference_duration_seconds(&out01).expect("输出应是合法 wav");
        assert!((d01 - 0.1).abs() < 0.05, "裁 0.1s 的副本应约 0.1s：{d01}");
        let mut r01 = hound::WavReader::open(&out01).unwrap();
        assert_eq!(
            r01.spec().sample_format,
            hound::SampleFormat::Int,
            "非 wav 解码输出必须 s16le"
        );
        assert_eq!(r01.spec().bits_per_sample, 16);
        let samples01: Vec<i16> = r01.samples::<i16>().collect::<Result<_, _>>().unwrap();
        assert!(
            samples01.iter().any(|&s| s != 0),
            "内容必须非静音——否则就是假解码（阳性对照）"
        );

        // 对照组：裁到 0.19s 的副本更长，且 0.1s 副本是它的前缀
        let out19 = trim_reference_first_seconds(&flac, &dir, 0.19).expect("0.19s 也应能裁");
        let d19 = reference_duration_seconds(&out19).expect("输出应是合法 wav");
        assert!(d19 > d01, "裁更多秒必须得到更长的副本：{d19} vs {d01}");
        let mut r19 = hound::WavReader::open(&out19).unwrap();
        let samples19: Vec<i16> = r19.samples::<i16>().collect::<Result<_, _>>().unwrap();
        assert_eq!(
            &samples19[..samples01.len()],
            samples01.as_slice(),
            "短副本必须是长副本的前缀（取前 N 秒，不是尾巴）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件/不可解码 → Err 且不留 `.tmp`；头与数据不符的损坏 wav 同样干净失败。
    #[test]
    fn trim_bad_input_fails_without_temp_leftovers() {
        let dir = temp_dir("trim-bad");

        // 垃圾字节：时长读不出 → Err（不建目录、不留任何东西）
        let garbage = dir.join("garbage.mp3");
        std::fs::write(&garbage, b"definitely not audio").unwrap();
        assert!(trim_reference_first_seconds(&garbage, &dir, 15.0).is_err());

        // 头声明 20s、数据只有 1s 的损坏 wav：拷贝中段读报错 → Err，且不留 .tmp
        let truncated = dir.join("truncated.wav");
        {
            let rate = 8000u32;
            let declared = (20.0 * rate as f64) as u32 * 2;
            let mut out = Vec::new();
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&(36 + declared).to_le_bytes());
            out.extend_from_slice(b"WAVEfmt ");
            out.extend_from_slice(&16u32.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&rate.to_le_bytes());
            out.extend_from_slice(&(rate * 2).to_le_bytes());
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&16u16.to_le_bytes());
            out.extend_from_slice(b"data");
            out.extend_from_slice(&declared.to_le_bytes());
            out.resize(44 + rate as usize * 2, 0); // 只写 1s 的数据
            std::fs::write(&truncated, &out).unwrap();
        }
        assert!(
            trim_reference_first_seconds(&truncated, &dir, 15.0).is_err(),
            "头声明 20s 但数据截断：必须失败而不是产出半截副本"
        );

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "失败后不能留 .tmp：{leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 边界：15.0 放行（不裁剪、返回源路径），15.01 裁剪到 15。
    #[test]
    fn trim_boundary_15s_passes_and_15_01s_trims_to_15() {
        let dir = temp_dir("trim-boundary");
        let exact = dir.join("exact-15s.wav");
        write_wav(&exact, 15.0, 8000);
        assert_eq!(
            trim_reference_first_seconds(&exact, &dir, 15.0).expect("15.0s 放行"),
            exact,
            "15.0s 应返回原路径（不裁剪）"
        );

        let over = dir.join("over-15.01s.wav");
        write_wav(&over, 15.01, 8000);
        let out = trim_reference_first_seconds(&over, &dir, 15.0).expect("15.01s 应裁剪");
        assert_ne!(out, over);
        let d = reference_duration_seconds(&out).expect("副本应是合法 wav");
        assert!((d - 15.0).abs() < 0.001, "裁出的副本应正好 15s：{d}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一源稳定复用：不改源时两次裁剪返回同一个副本路径；源变了（size 进哈希）
    /// 就不复用旧副本。
    #[test]
    fn trim_reuses_copy_for_unchanged_source_and_refreshes_when_source_changes() {
        let dir = temp_dir("trim-reuse");
        let src = dir.join("ref.wav");
        write_pattern_wav(&src, 20.0, 8000);
        let first = trim_reference_first_seconds(&src, &dir, 15.0).unwrap();
        let again = trim_reference_first_seconds(&src, &dir, 15.0).unwrap();
        assert_eq!(first, again, "同一源（路径|size|mtime 不变）应复用同一副本");

        // 源换内容（size 变）→ 哈希变 → 新副本名，不命中旧 mtime/旧内容的陈旧副本
        write_pattern_wav(&src, 22.0, 8000);
        let refreshed = trim_reference_first_seconds(&src, &dir, 15.0).unwrap();
        assert_ne!(refreshed, first, "源变了必须产新副本，不能复用陈旧副本");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旁车：写入→读回（trim 两端空白）；空白不覆盖已有内容；没写时是 None。
    #[test]
    fn reference_text_sidecar_roundtrip_and_empty_ignored() {
        let dir = temp_dir("reftext-sidecar");
        let trimmed = dir.join("a-15s.wav");
        std::fs::write(&trimmed, b"RIFF").unwrap();
        assert_eq!(
            paired_reference_text(&trimmed),
            None,
            "没写旁车时必须是 None"
        );
        write_reference_text_sidecar(&trimmed, "  老板，您好。  ").unwrap();
        assert_eq!(
            paired_reference_text(&trimmed).as_deref(),
            Some("老板，您好。"),
            "旁车读回要 trim 两端空白"
        );
        write_reference_text_sidecar(&trimmed, "   ").unwrap();
        assert_eq!(
            paired_reference_text(&trimmed).as_deref(),
            Some("老板，您好。"),
            "空白文本不应该写盘/覆盖已有旁车"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 比例截断：能停在句末标点；无标点时按字数硬截且不劈多字节字符；
    /// 保留时长 ≥ 全时长原样返回；非法/零时长返回空。
    #[test]
    fn proportional_reference_text_truncates_by_ratio() {
        let text = "第一句话。第二句话。第三句话。第四句话。";
        // 预算 = ceil(20 × 15/60) = 5 字 → 落在第一句句末
        let got = proportional_reference_text(text, 15.0, 60.0);
        assert!(text.starts_with(&got), "必须是原文前缀：{got}");
        assert!(got.ends_with('。'), "应停在句末标点：{got}");
        assert!(got.chars().count() < text.chars().count());

        // 无标点：按字数硬截，且不劈多字节字符
        let no_punct = "一二三四五六七八九十";
        assert_eq!(
            proportional_reference_text(no_punct, 5.0, 10.0),
            "一二三四五"
        );
        // 保留时长 ≥ 全时长 → 原样
        assert_eq!(proportional_reference_text(text, 200.0, 192.9), text);
        // 非法输入 → 空
        assert_eq!(proportional_reference_text(text, f64::NAN, 192.9), "");
        assert_eq!(proportional_reference_text(text, 15.0, 0.0), "");
    }

    /// 估算兜底（原时长不可知）：按 5 字/秒 上限截断；短文本不动。
    #[test]
    fn estimated_reference_text_caps_by_speech_rate() {
        let long = "一二三四五六七八九十".repeat(20); // 200 字
        let got = estimated_reference_text(&long, 15.0); // 15 × 5 = 75 字
        assert_eq!(got.chars().count(), 75);
        let short = "老板，您好。";
        assert_eq!(
            estimated_reference_text(short, 15.0),
            short,
            "短文本不该被动"
        );
        assert_eq!(estimated_reference_text(long.as_str(), 0.0), "");
    }
}
