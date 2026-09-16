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
    let vocals_tmp = vocals_path.with_extension("wav.part");
    let accompaniment_tmp = accompaniment_path.with_extension("wav.part");
    stems
        .save(Stem::Vocals, &vocals_tmp.display().to_string())
        .map_err(|e| format!("写出人声轨失败（{}）：{e}", vocals_tmp.display()))?;
    if let Err(e) = stems.save_mix_except(&[Stem::Vocals], &accompaniment_tmp.display().to_string())
    {
        let _ = std::fs::remove_file(&vocals_tmp);
        return Err(format!(
            "写出伴奏轨失败（{}）：{e}",
            accompaniment_tmp.display()
        ));
    }
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
}
