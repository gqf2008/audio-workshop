//! 人声分离（本地 ONNX 推理）：把一首歌拆成「人声」与「伴奏」两轨。
//!
//! 上游 `stem-splitter-core`（htdemucs，纯 Rust + ONNX Runtime）出 **4 轨**
//! （vocals / drums / bass / other），它自带 `mix_except` / `save_mix_except`，
//! 所以**伴奏轨不需要我们自己求和**——直接 `save_mix_except(&[Stem::Vocals])`。
//!
//! 三个必须如实说明的约束（都源自上游 API，不是我们的取舍）：
//! 1. `Separator::separate` **没有取消参数**：跑起来就停不下来。这里的 `should_stop`
//!    只能在**返回之后**判断——一旦用户在过程中点了停止，我们丢弃结果、不落盘，
//!    但已经烧掉的算力收不回来（UI 文案里必须写清）。
//! 2. 进度/下载回调是**进程级 OnceLock，只能注册一次**：这里在首次使用时注册一次，
//!    之后每次运行只把"当前这次要往哪发"换成一个全局槽位里的 Sender。
//! 3. 模型默认从网上拉（~200MB）；给了 `model_dir` 且里面有 `.onnx` 时走本地、不联网。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use stem_splitter_core::{
    set_download_progress_callback, set_split_progress_callback, Separator, SplitOptions,
    SplitProgress, Stem,
};

/// 默认模型名（与上游 registry 里的条目一致）。
pub const DEFAULT_MODEL: &str = "htdemucs_ort_v1";

/// 分离请求。
#[derive(Clone, Debug)]
pub struct SeparationRequest {
    /// 输入音频（wav / mp3 / flac 等，解码由上游负责）。
    pub input: PathBuf,
    /// 两轨输出目录（不存在时由我们创建）。
    pub out_dir: PathBuf,
    /// 输出文件名前缀（一般用工程名，避免多个任务互相覆盖）。
    pub stem: String,
    /// 本地模型目录：里面有 `.onnx` 就用它（离线）；没有就走上游下载。
    pub model_dir: Option<PathBuf>,
    /// 分块秒数：越小越省内存、越慢。0/None = 用上游默认（60s）。
    pub chunk_seconds: Option<u32>,
}

/// 一次进度事件（已归一成"百分比 + 一句话"，UI 不需要认识上游枚举）。
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub percent: f32,
    pub note: String,
    /// true = 正在下载模型（首次使用），false = 正在分离
    pub downloading: bool,
}

/// 分离结果：两轨的落盘路径。
#[derive(Clone, Debug, PartialEq)]
pub struct SeparatedTracks {
    pub vocals: PathBuf,
    pub accompaniment: PathBuf,
    /// 裁齐相关的说明（None = 正常裁齐或无需说明）。
    /// 例：输入不是 wav（跳过裁齐）、wav 读不出时长（未裁齐）。
    pub note: Option<String>,
}

/// 结果：正常完成，或用户在过程中按了停止（此时**不落盘**）。
#[derive(Clone, Debug, PartialEq)]
pub enum SeparationOutcome {
    Done(SeparatedTracks),
    Stopped,
}

/// 当前这次运行要把进度发到哪（上游回调只能注册一次，所以用槽位换目标）。
static SINK: OnceLock<Mutex<Option<Sender<Progress>>>> = OnceLock::new();
/// 上游回调是否已经注册（OnceLock 保证只注册一次）。
static INSTALLED: OnceLock<()> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sender<Progress>>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// RAII：离开作用域时清掉 SINK，保证"出错/提前 return/子线程 panic"都不会留下槽位。
struct SinkGuard;

impl Drop for SinkGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = sink().lock() {
            *g = None;
        }
    }
}

fn send_progress(p: Progress) {
    let tx = sink().lock().ok().and_then(|g| g.clone());
    if let Some(tx) = tx {
        let _ = tx.send(p);
    }
}

/// 把上游的分块/写出进度归一成 0..1。
pub fn map_split_progress(p: SplitProgress) -> Progress {
    match p {
        SplitProgress::Stage(stage) => Progress {
            percent: 0.0,
            note: format!("准备中：{stage}"),
            downloading: false,
        },
        SplitProgress::Chunks {
            done,
            total,
            percent,
        } => Progress {
            percent: percent / 100.0,
            note: format!("分离中：分块 {done}/{total}"),
            downloading: false,
        },
        SplitProgress::Writing {
            stem,
            done,
            total,
            percent,
        } => Progress {
            percent: percent / 100.0,
            note: format!("写出 {stem}：{done}/{total}"),
            downloading: false,
        },
        SplitProgress::Finished => Progress {
            percent: 1.0,
            note: "分离完成".into(),
            downloading: false,
        },
    }
}

/// 注册上游的全局回调（只注册一次；后续运行通过 SINK 换目标）。
pub fn install_progress_callbacks() {
    INSTALLED.get_or_init(|| {
        set_split_progress_callback(|p| send_progress(map_split_progress(p)));
        set_download_progress_callback(|done, total| {
            let percent = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            send_progress(Progress {
                percent,
                note: format!("下载模型：{done}/{total} 字节"),
                downloading: true,
            });
        });
    });
}

/// 两轨输出路径：`<out_dir>/<stem>_vocals.wav` 与 `<out_dir>/<stem>_accompaniment.wav`。
/// 输入音频时长（秒）。三态，别把三种情况揉成一个 `None`：
///
/// - `Ok(Some(secs))`：能裁（wav 且读得动）；
/// - `Ok(None)`：**不是 wav**（mp3/flac 由上游解码器读，长度我们拿不到）→ 按文档跳过裁齐；
/// - `Err(note)`：扩展名是 wav 却读不出来（损坏/权限）→ 如实报出去，不静默跳过
///   （静默跳过会让用户拿到比输入长的产物却不知道原因——复核指出）。
fn input_duration_seconds(path: &Path) -> Result<Option<f64>, String> {
    let looks_like_wav = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);
    match hound::WavReader::open(path) {
        Ok(reader) => {
            let sr = reader.spec().sample_rate;
            if sr == 0 {
                return Err(format!("输入 wav 的采样率为 0：{}", path.display()));
            }
            Ok(Some(reader.duration() as f64 / sr as f64))
        }
        Err(e) if looks_like_wav => Err(format!(
            "输入是 wav 但读不出时长（{}）：{e}。请确认文件没损坏/有权限后重跑。",
            path.display()
        )),
        Err(_) => Ok(None),
    }
}

/// 把 wav 裁到指定时长（原地替换：临时文件 + rename）。比目标短就原样不动。
///
/// 只处理 16-bit PCM（该模型的产物就是 16-bit）；其它位深原样返回，不让"裁不了"
/// 变成"分离失败"。
fn trim_wav_to_seconds(path: &Path, seconds: f64) -> Result<(), String> {
    let reader =
        hound::WavReader::open(path).map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 || spec.sample_rate == 0 {
        return Ok(());
    }
    let keep_frames = (seconds * spec.sample_rate as f64).round().max(0.0) as usize;
    let total_frames = reader.duration() as usize;
    if total_frames <= keep_frames {
        return Ok(());
    }
    // 临时文件显式命名（`with_extension("wav.trim")` 作用在 `xxx.wav.part` 上会得到
    // `xxx.wav.wav.trim`，名字难看也容易误导）。
    let tmp = path.with_extension("trim");
    // 写入放在闭包里：**任何**错误（创建/读样本/写样本/finalize）都会走下面的清理；
    // 只在 rename 失败时清会留下 .trim 残渣（复核指出）。
    let written = (|| -> Result<(), String> {
        let mut writer = hound::WavWriter::create(&tmp, spec)
            .map_err(|e| format!("创建 {} 失败：{e}", tmp.display()))?;
        let channels = spec.channels as usize;
        for (i, sample) in reader.into_samples::<i16>().enumerate() {
            if i / channels >= keep_frames {
                break;
            }
            writer
                .write_sample(sample.map_err(|e| format!("读 {} 失败：{e}", path.display()))?)
                .map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("收尾 {} 失败：{e}", tmp.display()))
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败：{e}", path.display())
    })
}

pub fn output_paths(out_dir: &Path, stem: &str) -> (PathBuf, PathBuf) {
    (
        out_dir.join(format!("{stem}_vocals.wav")),
        out_dir.join(format!("{stem}_accompaniment.wav")),
    )
}

/// 在本地模型目录里找 htdemucs 权重：**只认文件名包含 `model_name` 的 `.onnx`**。
///
/// 不"退而求其次取第一个 .onnx"：模型目录里可能有别的 ONNX（例如某个 tts 的权重），
/// 拿它当 htdemucs 加载只会得到一句莫名其妙的"分离失败"。名字不匹配就当本地没有、
/// 交给上游下载（缓存过一次之后就离线了）。
pub fn find_local_model(model_dir: &Path, model_name: &str) -> Option<PathBuf> {
    if !model_dir.is_dir() {
        return None;
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(model_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| x.eq_ignore_ascii_case("onnx"))
                .unwrap_or(false)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains(model_name))
                .unwrap_or(false)
        })
        .cloned()
}

/// 分块秒数：0 或 None 一律回落到 `None`（= 上游默认 60s）；过小的值抬到 5s。
pub fn normalize_chunk_seconds(v: Option<u32>) -> Option<u32> {
    match v {
        None | Some(0) => None,
        Some(s) if s < 5 => Some(5),
        Some(s) => Some(s),
    }
}

/// 真正干活：分离 → 保存人声 + 伴奏两轨。
///
/// `on_progress` 在本函数所在的线程上被调用；`should_stop` 只在**分离返回后**被检查
/// （上游没有取消 API，见模块头注释）。
pub fn separate_tracks(
    req: &SeparationRequest,
    mut on_progress: impl FnMut(Progress),
    should_stop: impl Fn() -> bool,
) -> Result<SeparationOutcome, String> {
    if !req.input.is_file() {
        return Err(format!("输入音频不存在：{}", req.input.display()));
    }
    install_progress_callbacks();

    let model_path = req
        .model_dir
        .as_deref()
        .and_then(|d| find_local_model(d, DEFAULT_MODEL));
    let local_model = model_path.clone();
    let opts = SplitOptions {
        output_dir: req.out_dir.display().to_string(),
        model_name: DEFAULT_MODEL.to_string(),
        manifest_url_override: None,
        model_path: model_path.map(|p| p.display().to_string()),
        chunk_seconds: normalize_chunk_seconds(req.chunk_seconds),
    };

    if let Some(dir) = req.out_dir.parent() {
        let _ = dir;
    }
    std::fs::create_dir_all(&req.out_dir)
        .map_err(|e| format!("创建输出目录失败（{}）：{e}", req.out_dir.display()))?;

    // 上游的分离是阻塞调用、且只能从全局回调里拿进度；这里把它放子线程跑，
    // 当前线程负责把全局槽位里的进度转成本次运行的 on_progress。
    let (tx, rx) = channel::<Progress>();
    *sink().lock().map_err(|_| "进度槽位被污染".to_string())? = Some(tx);
    // 从这里开始无论怎么返回（失败 / panic / 正常）都会清槽位
    let _sink_guard = SinkGuard;
    let input = req.input.display().to_string();

    let handle = std::thread::spawn(move || {
        Separator::separate(&input, opts).map_err(|e| format!("分离失败：{e}"))
    });

    // 边等边转进度（recv_timeout 轮询，直到子线程结束）
    let stems = loop {
        match rx.recv_timeout(std::time::Duration::from_millis(120)) {
            Ok(p) => on_progress(p),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if handle.is_finished() {
                    // 收干队列里剩下的进度
                    while let Ok(p) = rx.try_recv() {
                        on_progress(p);
                    }
                    break handle.join().map_err(|_| "分离线程崩溃".to_string())??;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break handle.join().map_err(|_| "分离线程崩溃".to_string())??;
            }
        }
    };

    if should_stop() {
        return Ok(SeparationOutcome::Stopped);
    }

    // 先写临时名、两个都成了再改名：否则第一步成功、第二步失败会留下"半套结果"
    let (vocals_path, accompaniment_path) = output_paths(&req.out_dir, &req.stem);
    // 上游按模型分块补齐：产物可能比输入长（本机实测 50.9s 输入 → 55.4s 产物，
    // 尾巴那 4.5s 是模型为补齐最后一块生成的内容）。输入是 wav 时把两轨裁回输入时长——
    // 用户拿到的东西该跟他给的音频一样长。mp3/flac 的长度在上游解码器里，我们拿不到，
    // 这两种输入保持上游产物（文档写明）。裁在 .part 上，再走原来的 rename 发布。
    let vocals_tmp = vocals_path.with_extension("wav.part");
    let accompaniment_tmp = accompaniment_path.with_extension("wav.part");
    stems
        .save(Stem::Vocals, &vocals_tmp.display().to_string())
        .map_err(|e| {
            format!(
                "写出人声轨失败（{}）：{e}。请检查磁盘空间与目录权限后重跑。",
                vocals_tmp.display()
            )
        })?;
    if let Err(e) = stems.save_mix_except(&[Stem::Vocals], &accompaniment_tmp.display().to_string())
    {
        let _ = std::fs::remove_file(&vocals_tmp);
        return Err(format!(
            "写出伴奏轨失败（{}）：{e}。请检查磁盘空间与目录权限后重跑。",
            accompaniment_tmp.display()
        ));
    }
    let trim_note = match input_duration_seconds(&req.input) {
        Ok(Some(seconds)) => {
            for part in [&vocals_tmp, &accompaniment_tmp] {
                if let Err(e) = trim_wav_to_seconds(part, seconds) {
                    let _ = std::fs::remove_file(&vocals_tmp);
                    let _ = std::fs::remove_file(&accompaniment_tmp);
                    let _ = std::fs::remove_file(part.with_extension("trim"));
                    return Err(format!("裁齐分离产物失败：{e}"));
                }
            }
            None
        }
        // 非 wav：按文档跳过，但把这件事**说出来**（用户可能因此拿到比输入长的产物）
        Ok(None) => Some(
            "输入不是 wav：上游按模型分块补齐，产物可能比输入略长（wav 输入会自动裁齐）"
                .to_string(),
        ),
        // wav 却读不出时长：不静默跳过，如实报出来
        Err(note) => Some(format!("产物未裁齐：{note}")),
    };
    // rename 失败也要收干净：否则会留下"半套结果"或目录里的 .part 残渣
    if let Err(e) = std::fs::rename(&vocals_tmp, &vocals_path) {
        // rename 失败也要收干净：否则会留下"半套结果"或目录里的 .part 残渣
        let _ = std::fs::remove_file(&vocals_tmp);
        let _ = std::fs::remove_file(&accompaniment_tmp);
        return Err(format!(
            "收尾人声轨失败：{}",
            crate::dub::write_failure_note(&vocals_path, 0, &e)
        ));
    }
    if let Err(e) = std::fs::rename(&accompaniment_tmp, &accompaniment_path) {
        // 人声已经落到最终名了：把它一起删掉，不留半套
        let _ = std::fs::remove_file(&vocals_path);
        let _ = std::fs::remove_file(&accompaniment_tmp);
        return Err(format!(
            "收尾伴奏轨失败：{}",
            crate::dub::write_failure_note(&accompaniment_path, 0, &e)
        ));
    }

    let _ = local_model;
    Ok(SeparationOutcome::Done(SeparatedTracks {
        vocals: vocals_path,
        accompaniment: accompaniment_path,
        note: trim_note,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_use_the_stem_prefix() {
        let (v, a) = output_paths(Path::new("/tmp/out"), "0921 开箱口播");
        assert_eq!(v, PathBuf::from("/tmp/out/0921 开箱口播_vocals.wav"));
        assert_eq!(a, PathBuf::from("/tmp/out/0921 开箱口播_accompaniment.wav"));
    }

    #[test]
    fn chunk_seconds_normalizes_zero_and_tiny_values() {
        assert_eq!(normalize_chunk_seconds(None), None, "None = 用上游默认");
        assert_eq!(normalize_chunk_seconds(Some(0)), None, "0 = 用上游默认");
        assert_eq!(normalize_chunk_seconds(Some(3)), Some(5), "过小的值抬到 5s");
        assert_eq!(normalize_chunk_seconds(Some(60)), Some(60));
    }

    #[test]
    fn find_local_model_requires_a_name_match() {
        let dir = std::env::temp_dir().join(format!("aw-sep-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 没有 .onnx 时返回 None（不能被别的文件骗到）
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();
        assert_eq!(find_local_model(&dir, DEFAULT_MODEL), None);
        // 只有名字不匹配的 .onnx → 不认（避免把别的模型当 htdemucs 加载）
        let a = dir.join("some-tts-model.onnx");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(find_local_model(&dir, DEFAULT_MODEL), None);
        // 名字匹配的 → 用它
        let b = dir.join(format!("{DEFAULT_MODEL}.onnx"));
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(find_local_model(&dir, DEFAULT_MODEL), Some(b));
        // 目录不存在 → None
        assert_eq!(find_local_model(&dir.join("nope"), DEFAULT_MODEL), None);
    }

    #[test]
    fn split_progress_maps_to_percent_and_note() {
        let p = map_split_progress(SplitProgress::Chunks {
            done: 1,
            total: 4,
            percent: 25.0,
        });
        assert_eq!(p.percent, 0.25);
        assert!(p.note.contains("1/4"), "note 要带分块进度：{}", p.note);
        assert!(!p.downloading);

        let w = map_split_progress(SplitProgress::Writing {
            stem: "vocals".into(),
            done: 1,
            total: 2,
            percent: 50.0,
        });
        assert_eq!(w.percent, 0.5);
        assert!(w.note.contains("vocals"));

        let f = map_split_progress(SplitProgress::Finished);
        assert_eq!(f.percent, 1.0);
    }

    #[test]
    fn missing_input_is_reported_before_touching_the_model() {
        let req = SeparationRequest {
            input: PathBuf::from("/definitely/not/here.wav"),
            out_dir: std::env::temp_dir(),
            stem: "x".into(),
            model_dir: None,
            chunk_seconds: None,
        };
        let err = separate_tracks(&req, |_| {}, || false).unwrap_err();
        assert!(err.contains("输入音频不存在"), "错误要能直接定位：{err}");
    }

    /// 裁齐：产物比输入长时裁到输入时长；比输入短就原样不动（不能把短的拉长）。
    /// 这条规则来自真机实测：50.9s 输入 → 55.4s 产物，尾巴 4.5s 是模型补齐最后一块
    /// 生成的"想象"内容。
    #[test]
    fn trim_wav_to_seconds_trims_padding_and_keeps_short_files() {
        let dir = std::env::temp_dir().join(format!("aw-trim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        // 1.2s 的"产物"，输入只有 1.0s
        let long = dir.join("long.wav");
        {
            let mut w = hound::WavWriter::create(&long, spec).unwrap();
            for i in 0..(44_100 * 2 * 12 / 10) {
                w.write_sample((i % 11) as i16).unwrap();
            }
            w.finalize().unwrap();
        }
        trim_wav_to_seconds(&long, 1.0).unwrap();
        let r = hound::WavReader::open(&long).unwrap();
        assert_eq!(r.duration(), 44_100, "应裁到 1.0s（44.1kHz）");
        assert!(
            r.spec().sample_rate == 44_100 && r.spec().channels == 2,
            "参数要原样保留"
        );

        // 0.5s 的"产物"：比目标短，不能被拉长
        let short = dir.join("short.wav");
        {
            let mut w = hound::WavWriter::create(&short, spec).unwrap();
            for i in 0..(44_100 * 2 / 2) {
                w.write_sample((i % 7) as i16).unwrap();
            }
            w.finalize().unwrap();
        }
        trim_wav_to_seconds(&short, 1.0).unwrap();
        assert_eq!(
            hound::WavReader::open(&short).unwrap().duration(),
            22_050,
            "比目标短就原样不动"
        );
    }

    /// 三态分类：wav 读得出（可裁）、非 wav（跳过裁齐）、扩展名是 wav 但读不出来（要报错，
    /// 不能静默跳过——静默跳过会让用户拿到比输入长的产物却不知道原因）。
    #[test]
    fn input_duration_classifies_wav_non_wav_and_broken_wav() {
        let dir = std::env::temp_dir().join(format!("aw-dur-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. 正常 wav：能拿到时长
        let good = dir.join("good.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&good, spec).unwrap();
            for _ in 0..8_000 {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        assert_eq!(input_duration_seconds(&good).unwrap(), Some(1.0));

        // 2. 非 wav（这里用 mp3 后缀的垃圾内容模拟）：跳过裁齐，不算错
        let mp3 = dir.join("song.mp3");
        std::fs::write(&mp3, b"not really an mp3").unwrap();
        assert_eq!(input_duration_seconds(&mp3).unwrap(), None);

        // 3. 后缀是 wav 但内容坏了：必须报错（含路径与建议），不能 None
        let broken = dir.join("broken.wav");
        std::fs::write(&broken, b"definitely not a wav").unwrap();
        let err = input_duration_seconds(&broken).unwrap_err();
        assert!(err.contains("wav") && err.contains("broken.wav"), "{err}");
        assert!(err.contains("损坏") || err.contains("权限"), "{err}");
    }
}
