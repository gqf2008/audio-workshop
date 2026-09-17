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

/// 写出两轨，并保证**任一失败都不留 `.part`**。
///
/// 为什么必须统一清理：上游 `write_audio` 是"先创建目标文件、再逐样本写"，中途失败
/// （磁盘满/权限变化）会留下半截文件。复核用 8MB 受限卷复现过：人声轨 ENOSPC 后留下
/// 8.1MB 的 `cli_vocals.wav.part`——原来的代码在人声失败分支直接 `?` 返回，没清。
fn write_stems_with_cleanup(
    vocals_tmp: &Path,
    accompaniment_tmp: &Path,
    write_vocals: impl FnOnce(&Path) -> Result<(), String>,
    write_accompaniment: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let cleanup = || {
        let _ = std::fs::remove_file(vocals_tmp);
        let _ = std::fs::remove_file(accompaniment_tmp);
    };
    if let Err(e) = write_vocals(vocals_tmp) {
        cleanup();
        return Err(e);
    }
    if let Err(e) = write_accompaniment(accompaniment_tmp) {
        cleanup();
        return Err(e);
    }
    Ok(())
}

/// 输入音频的**采样率**：拿得到就返回，拿不到就给出可执行的报错（不再有"静默跳过"这一态）。
///
/// - wav：用 hound 读头（快，不解码）；
/// - mp3/flac…：用 symphonia 探测音轨参数（上游自己就用它解码，这里复用同一套）；
/// - 读不出（损坏/权限/不支持的格式）：`Err(note)` → **在进模型之前**返回可执行错误（带上
///   支持的格式与转换建议），既不静默跳过，也不白烧一轮算力。
///
/// 为什么需要它（2026-09-17 实测）：上游**不做重采样、保留输入的帧数/时间轴**，但把输出标签
/// 写成**模型自己的采样率 44100**。48kHz 输入因此得到一个"同帧数、44.1kHz"的产物——
/// **时长 +8.84%、播放被拉慢 1.0884 倍**。
/// 把两轨按输入采样率重新打标签（样本不动）就能 1:1 还原：实测按输入采样率读时，
/// 伴奏/人声与输入的包络相关系数 0.822 / 0.800（按 44.1kHz 读只有 0.268）。
fn input_sample_rate(path: &Path) -> Result<u32, String> {
    let looks_like_wav = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);
    match hound::WavReader::open(path) {
        Ok(reader) => {
            let rate = reader.spec().sample_rate;
            if rate == 0 {
                return Err(format!("输入 wav 的采样率为 0：{}", path.display()));
            }
            Ok(rate)
        }
        Err(e) if looks_like_wav => Err(format!(
            "输入是 wav 但读不出采样率（{}）：{e}。请确认文件没损坏/有权限后重跑。",
            path.display()
        )),
        // 非 wav（mp3/flac…）：交给 symphonia 读头拿采样率——上游自己就用它解码，
        // 这里复用同一套，不另造解码逻辑。
        Err(_) => probe_sample_rate(path),
    }
}

/// 用 symphonia 探测非 wav 输入的采样率。
///
/// 只读容器头/音轨参数，不解码样本——分离本身仍由上游完成。
fn probe_sample_rate(path: &Path) -> Result<u32, String> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let file =
        std::fs::File::open(path).map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, stream, &Default::default(), &Default::default())
        .map_err(|e| format!("探测 {} 的音频格式失败：{e}", path.display()))?;
    let track = probed
        .format
        .default_track()
        .ok_or_else(|| format!("{} 里没有可用音轨", path.display()))?;
    track
        .codec_params
        .sample_rate
        .ok_or_else(|| format!("{} 的音轨没带采样率信息", path.display()))
}

/// 把 wav 的采样率标签改成 `rate`（样本一个不动、帧数不变）。
///
/// 上游只错了标签：它喂给模型的是"把输入样本当 44.1kHz 播"的慢放版本，产物的**帧索引与输入
/// 帧索引对齐**，所以改回输入的标签是无损往返（比真做一次重采样更保真——不引入抗混叠滤波损失）。
/// 标签已经一样时原样返回；不是 16-bit PCM 也返回（上游 `write_audio` 目前固定写 16-bit，
/// 这是个理论分支——真出现别的位深时应该先确认标签是否需要修，而不是在这里硬套）。
fn relabel_wav_sample_rate(path: &Path, rate: u32) -> Result<(), String> {
    let reader =
        hound::WavReader::open(path).map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
    let mut spec = reader.spec();
    // 上游 write_audio 目前固定 16-bit，这个分支今天不可达；保持"跳过"而不是伪造一次修正
    if spec.sample_rate == rate || spec.bits_per_sample != 16 {
        return Ok(());
    }
    spec.sample_rate = rate;
    // 临时文件显式命名；写入放进闭包，**任何**错误都清理（只在 rename 失败时清会留渣）。
    let tmp = path.with_extension("rate");
    let written = (|| -> Result<(), String> {
        let mut writer = hound::WavWriter::create(&tmp, spec)
            .map_err(|e| format!("创建 {} 失败：{e}", tmp.display()))?;
        for sample in reader.into_samples::<i16>() {
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
    // 先探输入采样率：① 拿到才知道产物该标什么采样率（上游一律标模型采样率 44100）；
    // ② 拿不到就**在花算力之前**报错——上游不支持的容器（如 m4a/aac）会先失败，
    //    只留一句「分离失败：end of stream」，用户既不知道原因也不知道下一步（复核指出）。
    let input_rate = input_sample_rate(&req.input)
        .map_err(|note| format!("{note}（目前支持 wav / mp3 / flac；可以先转成 wav 再试）"))?;
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
    // 上游把产物**一律声明成 44.1kHz**（同帧数），48kHz 输入因此被拉慢 1.0884 倍、时长 +8.84%。
    // 输入是 wav 时把两轨标签改回输入的采样率（样本不动）→ 时长与速度 1:1 还原；
    // 输入采样率在上面的进门处就探好了：能探到就直接把两轨标签改回去（wav 走 hound、
    // mp3/flac 走 symphonia probe）；探不到则那时已经返回可执行错误，不存在"保持上游产物"这条路径。
    let vocals_tmp = vocals_path.with_extension("wav.part");
    let accompaniment_tmp = accompaniment_path.with_extension("wav.part");
    write_stems_with_cleanup(
        &vocals_tmp,
        &accompaniment_tmp,
        |path| {
            stems
                .save(Stem::Vocals, &path.display().to_string())
                .map_err(|e| {
                    format!(
                        "写出人声轨失败（{}）：{e}。请检查磁盘空间与目录权限后重跑。",
                        path.display()
                    )
                })
        },
        |path| {
            stems
                .save_mix_except(&[Stem::Vocals], &path.display().to_string())
                .map_err(|e| {
                    format!(
                        "写出伴奏轨失败（{}）：{e}。请检查磁盘空间与目录权限后重跑。",
                        path.display()
                    )
                })
        },
    )?;
    // 把两轨标签改回输入采样率（样本不动）：上游不重采样、保留输入帧数，但把标签写成模型
    // 采样率 44100 —— 48kHz 输入会因此被拉慢 1.0884 倍。采样率上面已经探过，这里只做改写。
    for part in [&vocals_tmp, &accompaniment_tmp] {
        if let Err(e) = relabel_wav_sample_rate(part, input_rate) {
            let _ = std::fs::remove_file(&vocals_tmp);
            let _ = std::fs::remove_file(&accompaniment_tmp);
            let _ = std::fs::remove_file(part.with_extension("rate"));
            return Err(format!("修正分离产物采样率失败：{e}"));
        }
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

    /// 测试夹具路径：**运行期**解析，编译期路径只作兜底。
    ///
    /// `env!("CARGO_MANIFEST_DIR")` 是编译期常量，会被烧进测试二进制；这个 crate 的
    /// 产物一旦被跨 worktree 复用（共享 `CARGO_TARGET_DIR`、worktree 改名/搬迁/删除），
    /// 它指向的就是一个早已不存在的旧目录 —— 夹具找不到，报出来却像是真实代码回归。
    /// cargo 跑测试时工作目录 = 包根，所以先按 cwd 找，编译期路径只当兜底。
    ///
    /// 抽成吃显式参数的函数是为了让"运行期优先"这条本身可测（用例直接喂两个假根）。
    fn fixture_path_with(
        rel: &str,
        cwd: Option<&Path>,
        manifest_dir: &Path,
    ) -> Result<PathBuf, String> {
        let mut tried = Vec::new();
        if let Some(cwd) = cwd {
            let from_cwd = cwd.join(rel);
            if from_cwd.is_file() {
                return Ok(from_cwd);
            }
            tried.push(from_cwd);
        }
        let from_manifest = manifest_dir.join(rel);
        if from_manifest.is_file() {
            return Ok(from_manifest);
        }
        tried.push(from_manifest);
        Err(format!(
            "找不到测试夹具 {rel}：依次检查了 {}。这通常意味着复用了在别处（别的目录名 / \
             别的 worktree）编译出来的产物——测试二进制里烧着编译期的 CARGO_MANIFEST_DIR，\
             指向已经不存在的旧路径。`touch crates/aw-core/src/lib.rs` 或 \
             `cargo clean -p aw-core` 强制重建后再跑。",
            tried
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("、")
        ))
    }

    /// `crates/aw-core/tests/fixtures/` 下的夹具路径（运行期解析，见上）。
    fn fixture_path(rel: &str) -> PathBuf {
        fixture_path_with(
            rel,
            std::env::current_dir().ok().as_deref(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    /// 夹具必须优先用**运行期** cwd 下那一份：两边都在（旧 worktree 与当前 cwd 各一份）
    /// 时要拿 cwd 的，只有 cwd 有而编译期根不存在时也要能拿到。
    ///
    /// 阳性对照：把 `fixture_path_with` 改成先查 `manifest_dir`（或退回直接用编译期常量），
    /// 本用例转红。
    #[test]
    fn fixture_path_prefers_runtime_cwd_over_baked_manifest_dir() {
        let rel = "tests/fixtures/probe-0.2s.flac";
        let root = std::env::temp_dir().join(format!("aw-fixture-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("current-worktree");
        let gone = root.join("old-deleted-worktree-name");
        std::fs::create_dir_all(cwd.join("tests/fixtures")).unwrap();
        std::fs::write(cwd.join(rel), b"cwd-copy").unwrap();

        // 1. 编译期那个根压根不存在（模拟"产物来自已被删掉的旧 worktree"）→ 仍能拿到夹具
        let got = fixture_path_with(rel, Some(&cwd), &gone).unwrap();
        assert_eq!(got, cwd.join(rel));

        // 2. 两边都在 → 必须是运行期 cwd 那一份，而不是烧进产物的编译期路径
        std::fs::create_dir_all(gone.join("tests/fixtures")).unwrap();
        std::fs::write(gone.join(rel), b"from-old-worktree").unwrap();
        let got = fixture_path_with(rel, Some(&cwd), &gone).unwrap();
        assert_eq!(got, cwd.join(rel), "必须优先用运行期 cwd 下的夹具");
        assert_eq!(std::fs::read(&got).unwrap(), b"cwd-copy");

        let _ = std::fs::remove_dir_all(root);
    }

    /// 两条路径都不在时，错误信息要**同时列出两条被检查过的路径**，并点明"通常意味着
    /// 复用了别处编译出来的产物"——否则跨 worktree 复用 target 的假红会被当成真实回归。
    ///
    /// 阳性对照：从错误里去掉任一条路径，或去掉"复用产物"那句提示，本用例转红。
    #[test]
    fn fixture_path_error_names_both_roots_and_the_reused_artifact_hint() {
        let rel = "tests/fixtures/probe-0.2s.flac";
        let cwd = Path::new("/definitely/not/here/current-cwd");
        let manifest = Path::new("/definitely/not/here/old-worktree/crates/aw-core");
        let err = fixture_path_with(rel, Some(cwd), manifest).unwrap_err();
        assert!(err.contains(&cwd.join(rel).display().to_string()), "{err}");
        assert!(
            err.contains(&manifest.join(rel).display().to_string()),
            "{err}"
        );
        assert!(err.contains("复用"), "{err}");
        assert!(err.contains("CARGO_MANIFEST_DIR"), "{err}");
        assert!(err.contains("aw-core"), "{err}");
    }

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

    /// 改标签：`relabel_wav_sample_rate` 只改头里的采样率，**帧数与样本值都不变**；
    /// 标签已经一样时原样返回（不该白白重写文件）。
    #[test]
    fn relabel_changes_only_the_declared_sample_rate() {
        let dir = std::env::temp_dir().join(format!("aw-relabel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = dir.join("stem.wav");
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..(44_100 * 2) {
                w.write_sample((i % 101) as i16).unwrap();
            }
            w.finalize().unwrap();
        }
        let before: Vec<i16> = hound::WavReader::open(&path)
            .unwrap()
            .into_samples::<i16>()
            .map(|s| s.unwrap())
            .collect();

        relabel_wav_sample_rate(&path, 48_000).unwrap();
        let r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.spec().sample_rate, 48_000, "标签要改成输入的采样率");
        assert_eq!(r.duration(), 44_100, "帧数不变（时长按新标签重新解释）");
        let after: Vec<i16> = hound::WavReader::open(&path)
            .unwrap()
            .into_samples::<i16>()
            .map(|s| s.unwrap())
            .collect();
        assert_eq!(before, after, "样本一个都不能动");

        // 再改一次（这次标签已经相同）：内容不变
        relabel_wav_sample_rate(&path, 48_000).unwrap();
        assert_eq!(
            hound::WavReader::open(&path).unwrap().spec().sample_rate,
            48_000
        );
    }

    /// 采样率探测的三条路径：wav 走 hound 读头、非 wav 走 symphonia probe（flac 夹具）、
    /// 探测失败（垃圾 .mp3）要报错；另外 `separate_tracks` 对上游同样不支持的 m4a
    /// 必须在**进模型之前**失败。
    #[test]
    fn input_sample_rate_classifies_wav_non_wav_and_broken_wav() {
        let dir = std::env::temp_dir().join(format!("aw-rate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&good, spec).unwrap();
            for _ in 0..800 {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        assert_eq!(input_sample_rate(&good).unwrap(), 8_000);

        // 2. 非 wav 但格式可探测（flac fixture，0.2s 8kHz 单声道）：拿到真实采样率
        let flac = fixture_path("tests/fixtures/probe-0.2s.flac");
        assert_eq!(
            input_sample_rate(&flac).unwrap(),
            8_000,
            "flac 也要能拿到采样率（否则 mp3/flac 输入的产物修不了标签）"
        );

        // 2b. 非 wav 且探测不了：报错（不静默跳过）
        let junk = dir.join("song.mp3");
        std::fs::write(&junk, b"not really an mp3").unwrap();
        let err = input_sample_rate(&junk).unwrap_err();
        assert!(err.contains("song.mp3") && err.contains("探测"), "{err}");

        // 2c. 上游同样不支持的容器（m4a/aac：symphonia 默认不含 aac/isomp4，上游也没开）
        //     —— 必须在**进模型之前**失败，并给出"支持哪些格式"的动作提示
        let m4a = fixture_path("tests/fixtures/probe-0.2s.m4a");
        let err = separate_tracks(
            &SeparationRequest {
                input: m4a,
                out_dir: dir.join("m4a-out"),
                stem: "x".into(),
                model_dir: None,
                chunk_seconds: None,
            },
            |_| {},
            || false,
        )
        .unwrap_err();
        assert!(
            err.contains("wav") && err.contains("mp3") && err.contains("flac"),
            "要告诉用户支持哪些格式：{err}"
        );
        assert!(err.contains("probe-0.2s.m4a"), "要带上路径：{err}");

        let broken = dir.join("broken.wav");
        std::fs::write(&broken, b"definitely not a wav").unwrap();
        let err = input_sample_rate(&broken).unwrap_err();
        assert!(err.contains("broken.wav") && err.contains("wav"), "{err}");
        assert!(err.contains("损坏") || err.contains("权限"), "{err}");
    }

    /// 两轨写出：**任一失败都不留 `.part`**（上游是先建文件再逐样本写，中途失败会留半截）。
    /// 复核用 8MB 受限卷复现过"人声轨 ENOSPC 后留下 8.1MB .part"。
    #[test]
    fn stem_write_failure_leaves_no_part_files() {
        let dir = std::env::temp_dir().join(format!("aw-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v = dir.join("v.wav.part");
        let a = dir.join("a.wav.part");

        // 1. 人声轨失败（上游已经写了半截）：两个都不留
        let err = write_stems_with_cleanup(
            &v,
            &a,
            |p| {
                std::fs::write(p, b"half").unwrap();
                Err("磁盘空间不足（模拟）".to_string())
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.contains("磁盘空间不足"), "{err}");
        assert!(!v.exists() && !a.exists(), "人声失败也要清干净");

        // 2. 伴奏轨失败：人声已经写成，也要一起清掉（不留半套）
        let err = write_stems_with_cleanup(
            &v,
            &a,
            |p| {
                std::fs::write(p, b"ok").unwrap();
                Ok(())
            },
            |p| {
                std::fs::write(p, b"half").unwrap();
                Err("权限不足（模拟）".to_string())
            },
        )
        .unwrap_err();
        assert!(err.contains("权限"), "{err}");
        assert!(!v.exists() && !a.exists(), "伴奏失败要把人声也清掉");

        // 3. 成功路径：两个文件都在（别把清理写成"总是删"）
        write_stems_with_cleanup(
            &v,
            &a,
            |p| {
                std::fs::write(p, b"v").unwrap();
                Ok(())
            },
            |p| {
                std::fs::write(p, b"a").unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert!(v.is_file() && a.is_file());
    }
}
