//! BGM 链路：文本生成分段 → 拼接/循环到配音时长 → 按句时间轴 duck → 三轨导出。

use crate::audio_client::{Client, ClientError};
use crate::dub::{hound_error_note, write_atomic_explained, write_failure_note, Project};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BgmOptions {
    pub model: String,
    pub prompt: String,
    pub segment_seconds: f64,
    pub target_seconds: f64,
    pub base_seed: u64,
    /// 人声段 BGM 的压低系数：0.2 ≈ -14dB。
    pub duck_gain: f32,
    /// duck 过渡时长，避免句首句尾爆音。
    pub fade_ms: u64,
}

impl Default for BgmOptions {
    fn default() -> Self {
        Self {
            model: "stable-audio-small-music".into(),
            prompt: String::new(),
            segment_seconds: 30.0,
            target_seconds: 0.0,
            base_seed: 831001,
            duck_gain: 0.22,
            fade_ms: 200,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BgmArtifacts {
    /// 人声轨（把配音成品拷过来）；**独立生成 BGM 时没有这一轨** → None
    pub voice: Option<PathBuf>,
    /// BGM 轨：永远是有的（不管是不是跟配音同框）
    pub bgm: PathBuf,
    /// 混音轨：只有拿到配音成品时才生成 → None
    pub mixed: Option<PathBuf>,
    /// 字幕：来自配音工程；独立生成时没有 → None
    pub srt: Option<PathBuf>,
    pub duration: f64,
    pub segments: usize,
}

/// 独立生成（没有配音成品）时的产物：只有 BGM 一轨，时长按用户选的来。
pub fn bgm_only_artifacts(dir: &Path, options: &BgmOptions) -> Result<BgmArtifacts, String> {
    let bgm = dir.join("bgm/bgm.wav");
    if !bgm.is_file() {
        return Err("缺少 bgm/bgm.wav".into());
    }
    // 时长以**实际写出的 wav** 为准（assemble_bgm 会按目标帧数截断，两者通常一致；
    // 但读回来更稳：万一将来对齐逻辑变了，界面显示的仍是真实产物时长）。
    let seconds = hound::WavReader::open(&bgm)
        .ok()
        .and_then(|r| {
            let spec = r.spec();
            // duration() 已是每声道帧数，别再除 channels（立体声会少算一半）
            (spec.sample_rate > 0 && spec.channels > 0)
                .then(|| r.duration() as f64 / spec.sample_rate as f64)
        })
        .filter(|d| *d > 0.0)
        .unwrap_or_else(|| options.target_seconds.max(0.1));
    Ok(BgmArtifacts {
        voice: None,
        bgm,
        mixed: None,
        srt: None,
        duration: seconds,
        segments: segment_count(options)?,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BgmManifest {
    version: u32,
    model: String,
    prompt: String,
    segment_seconds: f64,
    target_seconds: f64,
    base_seed: u64,
}

impl BgmManifest {
    fn from_options(options: &BgmOptions) -> Self {
        Self {
            version: 1,
            model: options.model.clone(),
            prompt: options.prompt.clone(),
            segment_seconds: options.segment_seconds,
            target_seconds: options.target_seconds,
            base_seed: options.base_seed,
        }
    }

    fn pending(options: &BgmOptions) -> Self {
        Self {
            version: 0,
            ..Self::from_options(options)
        }
    }
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("bgm/manifest.json")
}

fn manifest_matches(dir: &Path, options: &BgmOptions) -> bool {
    std::fs::read_to_string(manifest_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<BgmManifest>(&raw).ok())
        .map(|saved| saved == BgmManifest::from_options(options))
        .unwrap_or(false)
}

fn segment_count(options: &BgmOptions) -> Result<usize, String> {
    if options.prompt.trim().is_empty() {
        return Err("BGM 描述为空".into());
    }
    if !options.segment_seconds.is_finite() || options.segment_seconds <= 0.0 {
        return Err("BGM 分段时长必须 > 0".into());
    }
    if !options.target_seconds.is_finite() || options.target_seconds <= 0.0 {
        return Err("BGM 目标时长必须 > 0".into());
    }
    Ok((options.target_seconds / options.segment_seconds)
        .ceil()
        .max(1.0) as usize)
}

fn segment_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("bgm/segments/{index:03}.wav"))
}

fn wav_duration_seconds(path: &Path) -> Option<f64> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    // duration() 已是每声道帧数；多除一次 channels 会让立体声分段看起来只有一半长，
    // 续跑时判成"这段没生成"→ 每次都重新合成
    Some(reader.duration() as f64 / spec.sample_rate as f64)
}

/// 逐段生成 BGM；已存在且时长足够的段自动跳过，支持中断续跑。
/// `on_progress(done, total, note)`。
/// 生成结果：跑完 or 用户停止（停止发生在**段与段之间**——单段的合成请求不可中断）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgmRun {
    Done(usize),
    Stopped(usize),
}

/// 可停止的生成：`should_stop` 在每段开始前检查一次。
///
/// 为什么只能段间停：单段是发给 audiocpp_server 的一次请求，客户端没有取消接口；
/// 但 BGM 往往是 5~10 段，段间停能把"等全部跑完"缩短到一个段的时长。
pub fn generate_segments_stoppable(
    client: &Client,
    dir: &Path,
    options: &BgmOptions,
    mut on_progress: impl FnMut(usize, usize, &str),
    should_stop: impl Fn() -> bool,
) -> Result<BgmRun, ClientError> {
    let total = segment_count(options).map_err(ClientError::Http)?;
    std::fs::create_dir_all(dir.join("bgm/segments")).ok();
    let reuse_cache = manifest_matches(dir, options);
    if !reuse_cache {
        let pending = serde_json::to_vec(&BgmManifest::pending(options))
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        write_atomic_explained(&manifest_path(dir), &pending)
            .map_err(|e| ClientError::Local(e.to_string()))?;
    }
    for i in 0..total {
        if should_stop() {
            return Ok(BgmRun::Stopped(i));
        }
        let path = segment_path(dir, i);
        if reuse_cache
            && wav_duration_seconds(&path)
                .map(|d| d > 0.0 && d + 0.05 >= options.segment_seconds)
                .unwrap_or(false)
        {
            on_progress(i + 1, total, "cached");
            continue;
        }
        generate_one_segment(client, options, i, &path)?;
        on_progress(i + 1, total, "done");
    }
    // 全部段都在，才把 manifest 落成"有效"（中途停止不写，下次仍视为未完成）
    let manifest = serde_json::to_vec(&BgmManifest::from_options(options))
        .map_err(|e| ClientError::Decode(e.to_string()))?;
    write_atomic_explained(&manifest_path(dir), &manifest)
        .map_err(|e| ClientError::Local(e.to_string()))?;
    Ok(BgmRun::Done(total))
}

pub fn generate_segments(
    client: &Client,
    dir: &Path,
    options: &BgmOptions,
    on_progress: impl FnMut(usize, usize, &str),
) -> Result<usize, ClientError> {
    // 老调用点不需要停止：转发给可停止版本，谓词恒 false。
    match generate_segments_stoppable(client, dir, options, on_progress, || false)? {
        BgmRun::Done(n) => Ok(n),
        // 谓词恒 false，理论上到不了这里；真到了也别谎报成功。
        BgmRun::Stopped(n) => Err(ClientError::Decode(format!(
            "内部状态异常：不可停止的生成被报告为已停止（完成 {n} 段）"
        ))),
    }
}

/// 生成并落盘单段（请求 → 校验 WAV → 原子写）。
fn generate_one_segment(
    client: &Client,
    options: &BgmOptions,
    i: usize,
    path: &Path,
) -> Result<(), ClientError> {
    let request = json!({
        "text": options.prompt,
        "options": {
            "duration_seconds": options.segment_seconds.to_string(),
            "seed": (options.base_seed + i as u64).to_string(),
        }
    });
    let wav = client.run_audio(&options.model, request)?;
    // 服务端返回的段必须是可由 hound 完整读取的 WAV，否则不能落盘冒充成功。
    let reader = hound::WavReader::new(std::io::Cursor::new(&wav))
        .map_err(|e| ClientError::Decode(e.to_string()))?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 || spec.channels == 0 || spec.sample_rate == 0 {
        return Err(ClientError::Decode("BGM 不是 16-bit PCM WAV".into()));
    }
    if reader.duration() == 0 {
        return Err(ClientError::Decode("BGM 段为 0 帧".into()));
    }
    write_atomic_explained(path, &wav).map_err(|e| ClientError::Local(e.to_string()))
}

/// 将 N 个 30s 段拼接；不足目标时长时循环，最终严格截到目标帧数。
pub fn assemble_bgm(dir: &Path, options: &BgmOptions) -> Result<PathBuf, String> {
    let total = segment_count(options)?;
    let first = segment_path(dir, 0);
    let first_reader = hound::WavReader::open(&first).map_err(|e| e.to_string())?;
    let spec = first_reader.spec();
    if spec.bits_per_sample != 16 || !(1..=2).contains(&spec.channels) {
        return Err("BGM 需为 16-bit 单/双声道 WAV".into());
    }
    let target_frames = (options.target_seconds * spec.sample_rate as f64).round() as u64;
    let out = dir.join("bgm/bgm.wav");
    let tmp = dir.join(format!("bgm/bgm.wav.tmp{}", std::process::id()));
    let mut writer =
        hound::WavWriter::create(&tmp, spec).map_err(|e| hound_error_note(&out, 0, &e))?;
    let mut written = 0u64;
    let mut index = 0usize;
    while written < target_frames {
        let path = segment_path(dir, index % total);
        let mut reader = hound::WavReader::open(&path).map_err(|e| e.to_string())?;
        if reader.spec().sample_rate != spec.sample_rate
            || reader.spec().channels != spec.channels
            || reader.spec().bits_per_sample != spec.bits_per_sample
        {
            return Err(format!("BGM 分段参数不一致: {}", path.display()));
        }
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let frames = samples.len() / spec.channels as usize;
        if frames == 0 {
            return Err(format!("BGM 分段为 0 帧: {}", path.display()));
        }
        for frame in 0..frames {
            if written >= target_frames {
                break;
            }
            let base = frame * spec.channels as usize;
            for ch in 0..spec.channels as usize {
                writer
                    .write_sample(samples[base + ch])
                    .map_err(|e| hound_error_note(&out, 0, &e))?;
            }
            written += 1;
        }
        index += 1;
    }
    writer
        .finalize()
        .map_err(|e| hound_error_note(&out, 0, &e))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| write_failure_note(&out, 0, &e))?;
    std::fs::rename(&tmp, &out).map_err(|e| write_failure_note(&out, 0, &e))?;
    Ok(out)
}

fn read_stereo_frame<I>(samples: &mut I, channels: u16) -> Result<(i16, i16), String>
where
    I: Iterator<Item = Result<i16, hound::Error>>,
{
    let first = samples
        .next()
        .ok_or("WAV 帧数不足")?
        .map_err(|e| e.to_string())?;
    if channels == 1 {
        return Ok((first, first));
    }
    let second = samples
        .next()
        .ok_or("WAV 帧数不足")?
        .map_err(|e| e.to_string())?;
    Ok((first, second))
}

fn mix_sample(voice: i16, bgm: i16, gain: f32) -> i16 {
    (voice as f32 + bgm as f32 * gain)
        .round()
        .clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn merged_segments(mut segments: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    segments.retain(|(start, end)| start.is_finite() && end.is_finite() && end > start);
    segments.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in segments {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

fn build_duck_gains(
    segments: Vec<(f64, f64)>,
    frames: usize,
    sample_rate: u32,
    gain: f32,
    fade: f64,
) -> Vec<f32> {
    let mut gains = vec![1.0f32; frames];
    let sr = sample_rate as f64;
    for (start, end) in merged_segments(segments) {
        let first = (((start - fade).max(0.0)) * sr).floor() as usize;
        let last = (((end + fade) * sr).ceil() as usize).min(frames);
        for (frame, value) in gains.iter_mut().enumerate().take(last).skip(first) {
            let t = frame as f64 / sr;
            let candidate = if t < start {
                let p = ((t - (start - fade)) / fade).clamp(0.0, 1.0) as f32;
                1.0 + (gain - 1.0) * p
            } else if t <= end {
                gain
            } else {
                let p = ((t - end) / fade).clamp(0.0, 1.0) as f32;
                gain + (1.0 - gain) * p
            };
            *value = value.min(candidate);
        }
    }
    gains
}

/// 用配音成品的句子时间轴压低 BGM，并导出 voice / bgm / mixed。
pub fn mix_project(dir: &Path, options: &BgmOptions) -> Result<BgmArtifacts, String> {
    let project = Project::load(dir).map_err(|e| format!("读配音工程失败: {e}"))?;
    let segments: Vec<(f64, f64)> = project
        .sentences
        .iter()
        .filter_map(|s| {
            s.start
                .zip(s.duration)
                .map(|(start, dur)| (start, start + dur))
        })
        .collect();
    if segments.is_empty() {
        return Err("配音工程还没有句子时间轴，先完成一次合成+拼装".into());
    }
    let voice_path = dir.join("out/final.wav");
    let bgm_path = dir.join("bgm/bgm.wav");
    let srt_path = dir.join("out/final.srt");
    if !voice_path.is_file() || !bgm_path.is_file() {
        return Err("缺少 out/final.wav 或 bgm/bgm.wav".into());
    }
    if !srt_path.is_file() || std::fs::metadata(&srt_path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("缺少非空的 out/final.srt：先完成配音拼装".into());
    }
    let mut voice = hound::WavReader::open(&voice_path).map_err(|e| e.to_string())?;
    let vs = voice.spec();
    let mut bgm = hound::WavReader::open(&bgm_path).map_err(|e| e.to_string())?;
    let bs = bgm.spec();
    if vs.sample_rate != bs.sample_rate {
        return Err(format!(
            "人声 {}Hz 与 BGM {}Hz 不一致",
            vs.sample_rate, bs.sample_rate
        ));
    }
    if vs.bits_per_sample != 16 || bs.bits_per_sample != 16 {
        return Err("混音仅支持 16-bit PCM WAV".into());
    }
    if !(1..=2).contains(&vs.channels) || !(1..=2).contains(&bs.channels) {
        return Err("人声/BGM 仅支持单/双声道".into());
    }

    let voice_copy = dir.join("out/voice.wav");
    let voice_bytes = std::fs::read(&voice_path).map_err(|e| e.to_string())?;
    write_atomic_explained(&voice_copy, &voice_bytes).map_err(|e| e.to_string())?;

    let mixed_path = dir.join("out/mixed.wav");
    let tmp = dir.join(format!("out/mixed.wav.tmp{}", std::process::id()));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: vs.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut out =
        hound::WavWriter::create(&tmp, spec).map_err(|e| hound_error_note(&mixed_path, 0, &e))?;
    // duration() 已是每声道帧数：多除一次 channels 会把立体声人声混成一半长
    let frames = voice.duration() as u64;
    let mut vi = voice.samples::<i16>();
    let mut bi = bgm.samples::<i16>();
    let fade = (options.fade_ms as f64 / 1000.0).max(0.001);
    let gain = options.duck_gain.clamp(0.0, 1.0);
    let gains = build_duck_gains(segments, frames as usize, vs.sample_rate, gain, fade);
    for frame in 0..frames {
        let (vl, vr) = read_stereo_frame(&mut vi, vs.channels)?;
        let (bl, br) = read_stereo_frame(&mut bi, bs.channels)?;
        let g = gains[frame as usize];
        out.write_sample(mix_sample(vl, bl, g))
            .map_err(|e| hound_error_note(&mixed_path, 0, &e))?;
        out.write_sample(mix_sample(vr, br, g))
            .map_err(|e| hound_error_note(&mixed_path, 0, &e))?;
    }
    out.finalize()
        .map_err(|e| hound_error_note(&mixed_path, 0, &e))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| write_failure_note(&mixed_path, 0, &e))?;
    std::fs::rename(&tmp, &mixed_path).map_err(|e| write_failure_note(&mixed_path, 0, &e))?;
    Ok(BgmArtifacts {
        voice: Some(voice_copy),
        bgm: bgm_path,
        mixed: Some(mixed_path),
        srt: Some(srt_path),
        duration: frames as f64 / vs.sample_rate as f64,
        segments: segment_count(options)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duck_envelope_keeps_adjacent_and_overlapping_segments_ducked() {
        let adjacent = build_duck_gains(vec![(0.0, 1.0), (1.0, 2.0)], 16_000, 8_000, 0.2, 0.2);
        assert!(
            (adjacent[8_400] - 0.2).abs() < 0.01,
            "gap=0 时第二句起始段仍应压低"
        );

        let overlapping = build_duck_gains(vec![(1.0, 3.0), (0.0, 2.0)], 32_000, 8_000, 0.2, 0.2);
        assert!(
            (overlapping[12_000] - 0.2).abs() < 0.01,
            "重叠/乱序段应合并后保持压低"
        );

        let short_gap = build_duck_gains(vec![(0.0, 0.5), (0.6, 1.0)], 8_000, 8_000, 0.2, 0.2);
        assert!(
            (short_gap[4_800] - 0.2).abs() < 0.01,
            "gap<fade 在下一句活跃时应保持压低"
        );
    }

    /// 复核要求：光测 helper 不算覆盖，必须走**真实写入路径**。
    /// 让 `bgm/` 目录不可写，`assemble_bgm` 创建 bgm.wav 时必然失败——
    /// 用户拿到的必须是可以照着做的文案，而不是裸 `os error 13`。
    #[test]
    fn assemble_bgm_write_failure_reports_actionable_note() {
        let dir = std::env::temp_dir().join(format!("aw-bgm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bgm/segments")).unwrap();
        write_test_segment(&dir.join("bgm/segments/000.wav"));

        let bgm_dir = dir.join("bgm");
        let before = std::fs::metadata(&bgm_dir).unwrap().permissions();
        let mut ro = before.clone();
        ro.set_readonly(true);
        std::fs::set_permissions(&bgm_dir, ro).unwrap();

        let options = BgmOptions {
            prompt: "test prompt".into(),
            target_seconds: 1.0,
            ..Default::default()
        };
        let err = assemble_bgm(&dir, &options).unwrap_err();

        // 先复原权限，保证测试结束能清理临时目录
        std::fs::set_permissions(&bgm_dir, before).unwrap();

        assert!(
            err.contains("没有写入权限") || err.contains("写入失败"),
            "要给可执行文案，不能是裸 errno：{err}"
        );
        assert!(err.contains("bgm.wav"), "要说清写的是哪个文件：{err}");
        assert!(
            err.contains("检查该目录权限") || err.contains("请释放空间"),
            "要给动作：{err}"
        );
    }

    /// 24kHz/单声道/16bit 的最小合法分段（能过 assemble_bgm 的头检查）。
    fn write_test_segment(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..2400 {
            w.write_sample((i % 89) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 分段时长也走 `hound::duration()`，而它**已经是每声道帧数**。
    /// 立体声分段若再除一次 channels，时长正好少一半 → 续跑时"这段时长不够"
    /// 会被判成没生成，缓存永远用不上（每次续跑都重合成一遍）。
    #[test]
    fn wav_duration_seconds_counts_frames_for_stereo_segments() {
        let dir = std::env::temp_dir().join(format!("aw-bgm-stereo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..(24_000 * 2) {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();

        let secs = wav_duration_seconds(&path).expect("刚写出来的 wav 应该读得出时长");
        assert!(
            (secs - 1.0).abs() < 1e-9,
            "立体声 1 秒应读成 1.0s，实得 {secs}"
        );
    }
}
