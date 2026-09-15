//! BGM 链路：文本生成分段 → 拼接/循环到配音时长 → 按句时间轴 duck → 三轨导出。

use crate::audio_client::{Client, ClientError};
use crate::dub::{write_atomic, Project};
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
    pub voice: PathBuf,
    pub bgm: PathBuf,
    pub mixed: PathBuf,
    pub srt: PathBuf,
    pub duration: f64,
    pub segments: usize,
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
    Some(reader.duration() as f64 / spec.channels as f64 / spec.sample_rate as f64)
}

/// 逐段生成 BGM；已存在且时长足够的段自动跳过，支持中断续跑。
/// `on_progress(done, total, note)`。
pub fn generate_segments(
    client: &Client,
    dir: &Path,
    options: &BgmOptions,
    mut on_progress: impl FnMut(usize, usize, &str),
) -> Result<usize, ClientError> {
    let total = segment_count(options).map_err(ClientError::Http)?;
    std::fs::create_dir_all(dir.join("bgm/segments")).ok();
    for i in 0..total {
        let path = segment_path(dir, i);
        if wav_duration_seconds(&path)
            .map(|d| d + 0.05 >= options.segment_seconds)
            .unwrap_or(false)
        {
            on_progress(i + 1, total, "cached");
            continue;
        }
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
        write_atomic(&path, &wav).map_err(|e| ClientError::Http(e.to_string()))?;
        on_progress(i + 1, total, "done");
    }
    Ok(total)
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
    let mut writer = hound::WavWriter::create(&tmp, spec).map_err(|e| e.to_string())?;
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
        for frame in 0..frames {
            if written >= target_frames {
                break;
            }
            let base = frame * spec.channels as usize;
            for ch in 0..spec.channels as usize {
                writer
                    .write_sample(samples[base + ch])
                    .map_err(|e| e.to_string())?;
            }
            written += 1;
        }
        index += 1;
    }
    writer.finalize().map_err(|e| e.to_string())?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
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

fn duck_gain_at(t: f64, segments: &[(f64, f64)], index: &mut usize, gain: f32, fade: f64) -> f32 {
    while *index < segments.len() && t >= segments[*index].1 + fade {
        *index += 1;
    }
    let Some(&(start, end)) = segments.get(*index) else {
        return 1.0;
    };
    if t >= start && t <= end {
        gain
    } else if t >= start - fade && t < start {
        let p = ((t - (start - fade)) / fade).clamp(0.0, 1.0) as f32;
        1.0 + (gain - 1.0) * p
    } else if t > end && t <= end + fade {
        let p = ((t - end) / fade).clamp(0.0, 1.0) as f32;
        gain + (1.0 - gain) * p
    } else {
        1.0
    }
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
    write_atomic(&voice_copy, &voice_bytes).map_err(|e| e.to_string())?;

    let mixed_path = dir.join("out/mixed.wav");
    let tmp = dir.join(format!("out/mixed.wav.tmp{}", std::process::id()));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: vs.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut out = hound::WavWriter::create(&tmp, spec).map_err(|e| e.to_string())?;
    let frames = voice.duration() as u64 / vs.channels as u64;
    let mut vi = voice.samples::<i16>();
    let mut bi = bgm.samples::<i16>();
    let fade = (options.fade_ms as f64 / 1000.0).max(0.001);
    let gain = options.duck_gain.clamp(0.0, 1.0);
    let mut seg_index = 0usize;
    for frame in 0..frames {
        let (vl, vr) = read_stereo_frame(&mut vi, vs.channels)?;
        let (bl, br) = read_stereo_frame(&mut bi, bs.channels)?;
        let g = duck_gain_at(
            frame as f64 / vs.sample_rate as f64,
            &segments,
            &mut seg_index,
            gain,
            fade,
        );
        out.write_sample(mix_sample(vl, bl, g))
            .map_err(|e| e.to_string())?;
        out.write_sample(mix_sample(vr, br, g))
            .map_err(|e| e.to_string())?;
    }
    out.finalize().map_err(|e| e.to_string())?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &mixed_path).map_err(|e| e.to_string())?;
    Ok(BgmArtifacts {
        voice: voice_copy,
        bgm: bgm_path,
        mixed: mixed_path,
        srt: srt_path,
        duration: frames as f64 / vs.sample_rate as f64,
        segments: segment_count(options)?,
    })
}
