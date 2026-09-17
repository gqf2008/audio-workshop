//! 人声分离历史：工程内的追加式索引 + 只读路径解析。
//!
//! 这份 JSON 和音频文件都在 `<工程目录>/stems/` 下，不建立全局历史目录。
//! 索引里只保存**两轨的裸文件名**；回看/试听/导出时先把文件名解析回
//! `<工程目录>/stems/`，并拒绝绝对路径、父目录、分隔符、软链和非普通文件。
//! 输入音频路径只作为历史元数据展示，绝不拿它来读写或删除文件。
//!
//! 失效规则见 `docs/separation-history.md`：历史是过去的结果，不受当前稿件或
//! 当前工程名影响；工程名不同就是另一个工程目录，历史自然不同。两轨文件被手动
//! 删除后，条目仍在索引里，但解析为不可用，界面不提供试听/导出。

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// 历史索引文件名（固定在工程内的 `stems/` 下）。
pub const HISTORY_FILE: &str = "history.json";
/// 当前 JSON schema 版本。
const HISTORY_VERSION: u32 = 1;

/// 一条成功分离记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeparationHistoryEntry {
    /// Unix 毫秒时间戳。排序只按它，避免依赖文件系统时间。
    pub created_at_ms: u64,
    /// 输入音频的绝对路径；只作元数据，不参与历史音频的读写。
    pub input_path: String,
    /// 人声轨的文件名（只允许裸文件名）。
    pub vocals_file: String,
    /// 伴奏轨的文件名（只允许裸文件名）。
    pub accompaniment_file: String,
    /// 能解析 WAV 头时的时长（秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// 能解析 WAV 头时的采样率（Hz）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
}

/// `history.json` 的外层结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    entries: Vec<SeparationHistoryEntry>,
}

/// 索引读取结果：缺失和空文件必须分开，损坏文件必须返回错误。
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryLoad {
    /// 文件不存在；这是正常的新工程状态。
    Missing,
    /// 文件存在且是有效 JSON；空表也表示已读取成功。
    Loaded(Vec<SeparationHistoryEntry>),
}

/// 一条历史记录的两轨解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TrackResolution {
    pub vocals: Option<PathBuf>,
    pub accompaniment: Option<PathBuf>,
    /// 可用时为空；不可用时说明哪一轨、为什么不可用。
    pub note: String,
}

impl TrackResolution {
    /// 两轨都通过工程内白名单和安全文件检查才可试听/导出。
    pub fn available(&self) -> bool {
        self.vocals.is_some() && self.accompaniment.is_some()
    }

    /// 可用时返回两轨路径；任一轨不可用则返回 `None`。
    pub fn paths(&self) -> Option<(&Path, &Path)> {
        Some((self.vocals.as_deref()?, self.accompaniment.as_deref()?))
    }
}

/// `<stems_dir>/history.json`。
pub fn history_path(stems_dir: &Path) -> PathBuf {
    stems_dir.join(HISTORY_FILE)
}

/// 读取历史索引。
///
/// - 不存在 → `Missing`
/// - 存在且可解析 → `Loaded`
/// - 损坏、软链、目录或版本不支持 → `Err`（调用方不得当成空历史覆盖）
pub fn load(stems_dir: &Path) -> Result<HistoryLoad, String> {
    match std::fs::symlink_metadata(stems_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "分离历史目录是软链，拒绝读取：{}",
                    stems_dir.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!(
                    "分离历史目录不是目录，拒绝读取：{}",
                    stems_dir.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HistoryLoad::Missing);
        }
        Err(e) => {
            return Err(format!(
                "分离历史目录读取失败：{e}。路径：{}",
                stems_dir.display()
            ))
        }
    }

    let path = history_path(stems_dir);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HistoryLoad::Missing),
        Err(e) => return Err(format!("分离历史读取失败：{e}。路径：{}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("分离历史文件是软链，拒绝读取：{}", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!("分离历史路径不是普通文件：{}", path.display()));
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| format!("分离历史读取失败：{e}。路径：{}", path.display()))?;
    let file: HistoryFile = serde_json::from_slice(&bytes)
        .map_err(|e| format!("分离历史解析失败：{e}。路径：{}", path.display()))?;
    if file.version != HISTORY_VERSION {
        return Err(format!(
            "分离历史版本不支持：版本 {}（当前支持 {}）。路径：{}",
            file.version,
            HISTORY_VERSION,
            path.display()
        ));
    }
    Ok(HistoryLoad::Loaded(file.entries))
}

/// 追加一条成功记录。
///
/// 先读现有索引，读到损坏文件时直接失败，不把坏文件覆盖掉；写入时使用
/// `aw_core::dub::write_atomic_explained`（临时文件 + fsync + rename）。
pub fn append_entry(stems_dir: &Path, entry: &SeparationHistoryEntry) -> Result<(), String> {
    ensure_stems_dir(stems_dir)?;
    let mut entries = match load(stems_dir)? {
        HistoryLoad::Missing => Vec::new(),
        HistoryLoad::Loaded(entries) => entries,
    };
    entries.push(entry.clone());

    let file = HistoryFile {
        version: HISTORY_VERSION,
        entries,
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| format!("分离历史序列化失败：{e}"))?;
    let path = history_path(stems_dir);
    aw_core::dub::write_atomic_explained(&path, &bytes)
        .map_err(|e| format!("分离历史写入失败：{e}"))?;
    Ok(())
}

/// 从刚刚成功写出的两轨生成记录并追加到历史。
///
/// `stems_dir` 必须是工程内的 `stems/`；两轨会先在它下面按裸文件名重新解析，
/// 因而软链、绝对路径、父目录和缺失文件都会在写历史前被拒绝。时长和采样率只从
/// 标准 PCM WAV 头里读取，拿不到时合法地写 `None`。
pub fn record_success(
    stems_dir: &Path,
    created_at_ms: u64,
    input: &Path,
    vocals: &Path,
    accompaniment: &Path,
) -> Result<SeparationHistoryEntry, String> {
    let vocals_file = track_file_name(vocals)?;
    let accompaniment_file = track_file_name(accompaniment)?;
    let vocals_path = resolve_track(stems_dir, &vocals_file)?;
    let accompaniment_path = resolve_track(stems_dir, &accompaniment_file)?;

    let probe = probe_wav(&vocals_path).or_else(|| probe_wav(&accompaniment_path));
    let (duration_secs, sample_rate_hz) = match probe {
        Some((duration, sample_rate)) => (Some(duration), Some(sample_rate)),
        None => (None, None),
    };
    let entry = SeparationHistoryEntry {
        created_at_ms,
        input_path: absolute_path_string(input)?,
        vocals_file,
        accompaniment_file,
        duration_secs,
        sample_rate_hz,
    };
    append_entry(stems_dir, &entry)?;
    Ok(entry)
}

/// 解析一条历史的两轨；不修改磁盘，也不删除任何条目。
pub fn resolve_tracks(stems_dir: &Path, entry: &SeparationHistoryEntry) -> TrackResolution {
    resolve_track_names(stems_dir, &entry.vocals_file, &entry.accompaniment_file)
}

/// 解析一组裸文件名；给“当前结果”在试听/导出前做一次新鲜检查。
pub fn resolve_track_names(
    stems_dir: &Path,
    vocals_file: &str,
    accompaniment_file: &str,
) -> TrackResolution {
    let vocals = resolve_track(stems_dir, vocals_file);
    let accompaniment = resolve_track(stems_dir, accompaniment_file);
    let mut notes = Vec::new();
    if let Err(note) = &vocals {
        notes.push(format!("人声：{note}"));
    }
    if let Err(note) = &accompaniment {
        notes.push(format!("伴奏：{note}"));
    }
    TrackResolution {
        vocals: vocals.ok(),
        accompaniment: accompaniment.ok(),
        note: if notes.is_empty() {
            String::new()
        } else {
            format!("文件不在了或不可用（{}）", notes.join("；"))
        },
    }
}

fn ensure_stems_dir(stems_dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(stems_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "分离历史目录是软链，拒绝写入：{}",
                    stems_dir.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!("分离历史路径不是目录：{}", stems_dir.display()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(stems_dir)
                .map_err(|e| format!("创建分离历史目录失败：{e}。路径：{}", stems_dir.display()))?;
            let metadata = std::fs::symlink_metadata(stems_dir)
                .map_err(|e| format!("复核分离历史目录失败：{e}。路径：{}", stems_dir.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("分离历史目录不是安全目录：{}", stems_dir.display()));
            }
        }
        Err(e) => {
            return Err(format!(
                "分离历史目录不可用：{e}。路径：{}",
                stems_dir.display()
            ))
        }
    }
    Ok(())
}

/// 从任意输出路径提取“裸文件名”，并拒绝目录/父目录/分隔符。
fn track_file_name(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("轨道文件名不是 UTF-8：{}", path.display()))?;
    validate_bare_file_name(name)?;
    Ok(name.to_string())
}

fn validate_bare_file_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(format!("文件名不安全：{name}（只接受裸文件名）"));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(format!("文件名不安全：{name}（只接受裸文件名）")),
    }
}

/// 解析工程内的一轨：白名单文件名 + 工程内前缀 + 拒绝软链/目录。
fn resolve_track(stems_dir: &Path, file_name: &str) -> Result<PathBuf, String> {
    validate_bare_file_name(file_name)?;

    let dir_metadata = std::fs::symlink_metadata(stems_dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("分离目录不存在：{}", stems_dir.display())
        } else {
            format!("分离目录读不了：{e}。路径：{}", stems_dir.display())
        }
    })?;
    if dir_metadata.file_type().is_symlink() {
        return Err(format!("分离目录是软链，拒绝使用：{}", stems_dir.display()));
    }
    if !dir_metadata.is_dir() {
        return Err(format!(
            "分离目录不是目录，拒绝使用：{}",
            stems_dir.display()
        ));
    }

    let path = stems_dir.join(file_name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{file_name} 文件不在了")
        } else {
            format!("{file_name} 读不了：{e}")
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{file_name} 不是工程内的普通文件（软链/目录已拒绝）"
        ));
    }

    // 再做一次 canonicalize 前缀核对：即使父目录里混入链接，也不允许解析到工程外。
    let canonical_dir =
        std::fs::canonicalize(stems_dir).map_err(|e| format!("分离目录无法规范化：{e}"))?;
    let canonical_path =
        std::fs::canonicalize(&path).map_err(|e| format!("{file_name} 无法规范化：{e}"))?;
    if !canonical_path.starts_with(&canonical_dir) {
        return Err(format!("{file_name} 解析到了工程目录之外，拒绝使用"));
    }
    Ok(path)
}

fn absolute_path_string(path: &Path) -> Result<String, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("无法取得当前目录，输入音频路径不能写入历史：{e}"))?
            .join(path)
    };
    absolute
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("输入音频路径不是 UTF-8：{}", absolute.display()))
}

/// 只读标准 RIFF/WAVE 头，避免为了历史元数据把整首音频读进内存。
fn probe_wav(path: &Path) -> Option<(f64, u32)> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }

    let mut sample_rate = None;
    let mut block_align = None;
    let mut data_len = None;
    loop {
        let mut chunk = [0u8; 8];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        let id = &chunk[0..4];
        let size = u32::from_le_bytes(chunk[4..8].try_into().ok()?) as u64;
        match id {
            b"fmt " => {
                let read_len = size.min(64) as usize;
                let mut fmt = vec![0u8; read_len];
                file.read_exact(&mut fmt).ok()?;
                if read_len >= 16 {
                    let rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
                    let align = u16::from_le_bytes(fmt[12..14].try_into().ok()?);
                    if rate > 0 && align > 0 {
                        sample_rate = Some(rate);
                        block_align = Some(align as u64);
                    }
                }
                let rest = size.saturating_sub(read_len as u64);
                if rest > 0 {
                    file.seek(SeekFrom::Current(rest as i64)).ok()?;
                }
            }
            b"data" => {
                data_len = Some(size);
                if sample_rate.is_some() && block_align.is_some() {
                    break;
                }
                file.seek(SeekFrom::Current(size as i64)).ok()?;
            }
            _ => {
                file.seek(SeekFrom::Current(size as i64)).ok()?;
            }
        }
        if size % 2 == 1 {
            file.seek(SeekFrom::Current(1)).ok()?;
        }
    }

    let rate = sample_rate?;
    let align = block_align?;
    let len = data_len?;
    if len == 0 {
        return None;
    }
    let duration = (len / align) as f64 / rate as f64;
    duration.is_finite().then_some((duration, rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aw-sep-history-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(vocals_file: &str, accompaniment_file: &str) -> SeparationHistoryEntry {
        SeparationHistoryEntry {
            created_at_ms: 1,
            input_path: "/tmp/input.wav".into(),
            vocals_file: vocals_file.into(),
            accompaniment_file: accompaniment_file.into(),
            duration_secs: None,
            sample_rate_hz: None,
        }
    }

    fn write_test_wav(path: &Path, sample_rate: u32, frames: u32) {
        let data_len = frames * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + data_len as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn history_read_distinguishes_missing_empty_and_corrupt() {
        let root = temp_dir("states");
        let stems = root.join("stems");

        assert_eq!(load(&stems).unwrap(), HistoryLoad::Missing);

        std::fs::create_dir_all(&stems).unwrap();
        std::fs::write(history_path(&stems), br#"{"version":1,"entries":[]}"#).unwrap();
        assert_eq!(load(&stems).unwrap(), HistoryLoad::Loaded(Vec::new()));

        let broken = br#"{"version":1,"entries":"not-an-array"}"#;
        std::fs::write(history_path(&stems), broken).unwrap();
        let err = load(&stems).unwrap_err();
        assert!(err.contains("解析失败"), "{err}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn append_keeps_valid_entries_and_does_not_overwrite_corruption() {
        let root = temp_dir("append");
        let stems = root.join("stems");
        std::fs::create_dir_all(&stems).unwrap();

        append_entry(&stems, &entry("a.wav", "b.wav")).unwrap();
        append_entry(&stems, &entry("c.wav", "d.wav")).unwrap();
        let HistoryLoad::Loaded(entries) = load(&stems).unwrap() else {
            panic!("刚写入的索引应可读取");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].vocals_file, "a.wav");
        assert_eq!(entries[1].accompaniment_file, "d.wav");

        let broken = br#"{"version":1,"entries":"broken"}"#;
        std::fs::write(history_path(&stems), broken).unwrap();
        let before = std::fs::read(history_path(&stems)).unwrap();
        let err = append_entry(&stems, &entry("e.wav", "f.wav")).unwrap_err();
        assert!(err.contains("解析失败"), "{err}");
        assert_eq!(std::fs::read(history_path(&stems)).unwrap(), before);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bare_file_name_whitelist_rejects_escape_attempts() {
        let root = temp_dir("names");
        let stems = root.join("stems");
        std::fs::create_dir_all(&stems).unwrap();

        for name in ["../outside.wav", "/tmp/outside.wav", "a/b.wav", "..", ""] {
            let resolution = resolve_tracks(&stems, &entry(name, "ok.wav"));
            assert!(!resolution.available(), "{name:?} 不应可用");
            assert!(
                resolution.note.contains("文件名不安全"),
                "{name:?}: {resolution:?}"
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_tracks_stay_visible_but_disabled() {
        let root = temp_dir("missing");
        let stems = root.join("stems");
        std::fs::create_dir_all(&stems).unwrap();

        let resolution = resolve_tracks(&stems, &entry("missing_vocals.wav", "missing_acc.wav"));
        assert!(!resolution.available());
        assert!(
            resolution.note.contains("文件不在了"),
            "{}",
            resolution.note
        );
        assert!(resolution.note.contains("missing_vocals.wav"));
        assert!(resolution.note.contains("missing_acc.wav"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_stems_dir_is_refused() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("stem-dir-symlink");
        let real = root.join("real-stems");
        let link = root.join("stems");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();

        let err = load(&link).unwrap_err();
        assert!(err.contains("软链"), "{err}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_tracks_are_refused() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("symlink");
        let stems = root.join("stems");
        let outside = root.join("outside.wav");
        std::fs::create_dir_all(&stems).unwrap();
        write_test_wav(&outside, 44_100, 441);
        symlink(&outside, stems.join("vocals.wav")).unwrap();

        let resolution = resolve_tracks(&stems, &entry("vocals.wav", "accompaniment.wav"));
        assert!(!resolution.available());
        assert!(resolution.note.contains("普通文件"), "{}", resolution.note);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_success_probes_wav_and_appends_entry() {
        let root = temp_dir("record");
        let stems = root.join("stems");
        let input = root.join("input.wav");
        std::fs::create_dir_all(&stems).unwrap();
        write_test_wav(&input, 48_000, 4_800);
        write_test_wav(&stems.join("tone_vocals.wav"), 44_100, 44_100);
        write_test_wav(&stems.join("tone_accompaniment.wav"), 44_100, 44_100);

        let recorded = record_success(
            &stems,
            1234,
            &input,
            &stems.join("tone_vocals.wav"),
            &stems.join("tone_accompaniment.wav"),
        )
        .unwrap();
        assert_eq!(recorded.created_at_ms, 1234);
        assert_eq!(recorded.vocals_file, "tone_vocals.wav");
        assert_eq!(recorded.accompaniment_file, "tone_accompaniment.wav");
        assert_eq!(recorded.sample_rate_hz, Some(44_100));
        assert!((recorded.duration_secs.unwrap() - 1.0).abs() < 1e-9);
        assert!(Path::new(&recorded.input_path).is_absolute());

        let HistoryLoad::Loaded(entries) = load(&stems).unwrap() else {
            panic!("成功分离后应有历史索引");
        };
        assert_eq!(entries, vec![recorded]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn record_success_refuses_symlinked_track_without_writing_entry() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("record-symlink");
        let stems = root.join("stems");
        let outside = root.join("outside.wav");
        let input = root.join("input.wav");
        std::fs::create_dir_all(&stems).unwrap();
        write_test_wav(&outside, 44_100, 441);
        write_test_wav(&input, 44_100, 441);
        symlink(&outside, stems.join("tone_vocals.wav")).unwrap();
        write_test_wav(&stems.join("tone_accompaniment.wav"), 44_100, 441);

        let err = record_success(
            &stems,
            1,
            &input,
            &stems.join("tone_vocals.wav"),
            &stems.join("tone_accompaniment.wav"),
        )
        .unwrap_err();
        assert!(err.contains("普通文件"), "{err}");
        assert!(matches!(load(&stems).unwrap(), HistoryLoad::Missing));

        let _ = std::fs::remove_dir_all(root);
    }
}
