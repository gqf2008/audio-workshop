//! 参考音频（音色克隆的 `voice_ref`）的时长读取与 30 秒硬上限判据。
//!
//! 为什么需要（2026-09-22 真机复现，随包 v0.8.2-metalbf16）：
//! 192.9s 的参考音频发 audio8-tts 克隆请求 → 引擎按参考时长膨胀图尺寸，向 Metal
//! 申请 14,539 MiB buffer → 分配失败 → `ggml_metal_buffer_is_shared` 空指针
//! **SIGSEGV，整个引擎进程死掉**（端口消失）。同一文件裁到 20s → HTTP 200
//! （11.3s 出音频）。分配规模约 75 MiB/参考秒。应用侧必须在所有会发克隆请求的
//! 入口之前拦住超长参考音频，不能把"杀掉引擎进程"留给上游
//! （复现细节与上游修复清单见 docs/robustness.md）。
//!
//! 读取与判据都只在这里一份：main 侧只负责在入口处调用 [`reference_over_limit`]。

use std::path::Path;

/// 参考音频硬上限（秒）。
///
/// 依据：UI 引导文案本来就是「5–30 秒干净人声」；30s × 约 75 MiB/参考秒 ≈ 2.3 GiB，
/// 在常见机器上仍是安全规模。放宽/收紧只改这一个常量（文案由判据按它生成）。
pub const REFERENCE_MAX_SECONDS: f64 = 30.0;

/// 读参考音频的时长（秒）。读不出 → `None`（**fail-open**，取舍见下）。
///
/// - wav：`dub::wav_file_duration`（hound 只读 RIFF 头，不读样本）；
/// - 非 wav（mp3/flac…）：symphonia 探测音轨参数（与 `separate::probe_sample_rate`
///   同一条依赖链——上游自己就用它解码，这里复用读头，不另造解码逻辑）。
///
/// **fail-open 的取舍（注释写清）**：这个护栏只拦「时长读得出来且确实超 30s」的
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
/// 文案有意按「实际秒数 → 上限 → 为什么 → 该做什么」排：状态栏是
/// `overflow: elide`，被截断时丢掉的是尾巴，动作句必须靠前
/// （与 docs/robustness.md §2 的文案顺序原则同一条）。
pub fn reference_too_long(seconds: f64) -> Option<String> {
    if seconds > REFERENCE_MAX_SECONDS {
        Some(format!(
            "参考音频 {seconds:.1} 秒，超过 {:.0} 秒硬上限：引擎按参考时长申请内存（约 75 MiB/秒，193 秒实测申请 14,539 MiB 后引擎崩溃），超长会直接打死引擎进程。请裁到 {:.0} 秒内再合成。",
            REFERENCE_MAX_SECONDS, REFERENCE_MAX_SECONDS
        ))
    } else {
        None
    }
}

/// 读时长 + 判据的组合入口（读不出时长 = fail-open 放行）。
///
/// main 侧四个会发克隆请求的入口（开始合成 / 批量提交 / 单句重录 / 音色试听）
/// 都在**发起前**过它。
pub fn reference_over_limit(path: &Path) -> Option<String> {
    reference_duration_seconds(path).and_then(reference_too_long)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aw-reflimit-{tag}-{}.wav", std::process::id()))
    }

    /// 20s 放行 / 192.9s 拦截，且文案含实际秒数 + 「裁到 30 秒内再合成」+ 为什么。
    #[test]
    fn criterion_allows_20s_and_blocks_192_9s_with_actionable_message() {
        assert!(reference_too_long(20.0).is_none(), "20s 放行");
        assert!(
            reference_too_long(30.0).is_none(),
            "正好 30.0s 在上限上，放行"
        );
        assert!(reference_too_long(30.01).is_some(), "30.01s 必须拦");
        let note = reference_too_long(192.9).expect("192.9s 必须拦");
        assert!(note.contains("192.9"), "文案要带实际秒数：{note}");
        assert!(note.contains("30"), "文案要带上限：{note}");
        assert!(note.contains("裁到 30 秒内再合成"), "文案要带动作：{note}");
        assert!(note.contains("75 MiB"), "文案要说清为什么：{note}");
    }

    /// 真文件端到端：20s wav 放行、192.9s wav 拦截（读取出自 RIFF 头，口径与
    /// 判据同一条链路）。
    #[test]
    fn real_20s_wav_passes_and_real_192_9s_wav_is_blocked() {
        let p20 = temp_file("ok");
        write_wav(&p20, 20.0, 8000);
        let d = reference_duration_seconds(&p20).expect("能读的 wav 必须有时长");
        assert!((d - 20.0).abs() < 0.1, "时长应约 20s：{d}");
        assert!(reference_over_limit(&p20).is_none(), "20s 放行");
        let _ = std::fs::remove_file(&p20);

        let p193 = temp_file("long");
        write_wav(&p193, 192.9, 8000);
        let note = reference_over_limit(&p193).expect("192.9s 必须拦");
        assert!(note.contains("192.9"), "{note}");
        assert!(note.contains("裁到 30 秒内再合成"), "{note}");
        let _ = std::fs::remove_file(&p193);
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
    ///
    /// 夹具定位与 separate.rs 的 fixture_path 同一策略：先查运行期 cwd（跨
    /// worktree 复用 target 时编译期 CARGO_MANIFEST_DIR 可能指向已删的旧路径），
    /// 再回落编译期清单根。
    #[test]
    fn symphonia_probe_reads_non_wav_duration() {
        let rel = "tests/fixtures/probe-0.2s.flac";
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(rel));
        }
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel));
        let path = candidates
            .iter()
            .find(|p| p.is_file())
            .expect("找不到 flac 夹具（tests/fixtures/probe-0.2s.flac）");
        let d = reference_duration_seconds(path).expect("flac 夹具应读得出时长");
        assert!((d - 0.2).abs() < 0.15, "probe-0.2s.flac 应约 0.2s：{d}");
        assert!(reference_over_limit(path).is_none());
    }
}
