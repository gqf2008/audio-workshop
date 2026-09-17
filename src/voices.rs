//! 音色库（M4-P4）：把克隆音色存成**自包含**条目，可保存 / 应用 / 导入 / 导出。
//!
//! 产品口径（docs/product-plan.md §4.2 P4）：音色抽屉里管理多个克隆音色，支持备份与
//! 跨机导入导出；免费侧对照是"一个本地克隆音色"。
//!
//! 现状里的痛点：工程的音色只是一个**外链路径**（`voice_ref`），原文件一挪一删工程就废。
//! 所以库里的每个音色都会把参考音频**复制进库目录**，条目自己就能用：
//!
//! ```text
//! ~/Documents/音频作坊/voices/
//!   index.json          # 条目元数据（名字 / 文件名 / 备注 / 时间）
//!   口播男声.wav         # 参考音频副本
//! ```
//!
//! 这里只放纯逻辑 + 文件读写；界面接线在 `src/main.rs`。

use std::path::{Path, PathBuf};

/// 库目录名（放在应用数据目录下，跟 settings/export 同级）。
pub const VOICES_DIR: &str = "voices";
/// 库索引文件名。
pub const INDEX_FILE: &str = "index.json";
/// 导出时每个音色一个子目录，里面是这个文件 + 音频副本。
pub const EXPORT_META: &str = "voice.json";

/// 一个音色条目。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceEntry {
    /// 显示名（库内唯一，忽略大小写）
    pub name: String,
    /// 库目录里的音频文件名（**只允许裸文件名**：导入时会校验，防路径穿越）
    pub file: String,
    #[serde(default)]
    pub note: String,
    /// 入库时间（Unix 毫秒）
    #[serde(default)]
    pub created_at: u64,
}

/// 库索引。
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceIndex {
    #[serde(default)]
    pub voices: Vec<VoiceEntry>,
}

impl VoiceIndex {
    /// 按名字（忽略大小写）找条目。
    pub fn get(&self, name: &str) -> Option<&VoiceEntry> {
        self.voices
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
    }

    /// 新增或覆盖（同名忽略大小写）；最新的排前面。
    fn upsert(&mut self, entry: VoiceEntry) {
        self.voices
            .retain(|v| !v.name.eq_ignore_ascii_case(&entry.name));
        self.voices.insert(0, entry);
    }
}

/// 库目录。
pub fn library_dir(root: &Path) -> PathBuf {
    root.join(VOICES_DIR)
}

/// 允许的参考音频扩展名（与单篇音色选择一致）；其它一律拒绝并给可执行文案。
const AUDIO_EXTS: [&str; 5] = ["wav", "mp3", "flac", "m4a", "ogg"];

/// 名字的 64 位 FNV-1a：给库内文件名加一段稳定后缀。
///
/// 为什么不能只用 `sanitize(name)`：`a/b` 与 `a_b` 归一后是同一个文件名，后一条会把前一条的
/// 音频覆盖掉（复核指出）。后缀取自**名字本身**（小写化），所以同一个名字总是同一个文件
/// （覆盖语义不变），不同名字的文件名不会撞。
fn name_suffix(lower_name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in lower_name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 库内文件名：`<slug>-<hash8>.<ext>`（hash 取名字哈希的前 8 位，够用且短）。
fn library_file_name(slug: &str, lower_name: &str, ext: &str) -> String {
    format!("{slug}-{}.{ext}", &name_suffix(lower_name)[..8])
}

fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// 读索引。**坏文件报错，不静默当空库**（否则下一次保存就把用户音色覆盖没了）；
/// 目录/文件不存在 = 空库（第一次用）。
pub fn load(root: &Path) -> Result<VoiceIndex, String> {
    let path = library_dir(root).join(INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| format!("音色库索引解析失败：{}（{e}）", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VoiceIndex::default()),
        Err(e) => Err(format!("音色库索引读不出来：{}（{e}）", path.display())),
    }
}

fn save_index(root: &Path, index: &VoiceIndex) -> Result<(), String> {
    let dir = library_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建音色库目录失败：{}（{e}）", dir.display()))?;
    let body = serde_json::to_vec_pretty(index).map_err(|e| format!("音色库序列化失败：{e}"))?;
    aw_core::dub::write_atomic_explained(&dir.join(INDEX_FILE), &body)
        .map_err(|e| format!("音色库索引写入失败：{e}"))
}

/// 条目对应的音频绝对路径（只用于展示/比对；要用它读写请走 `usable_audio_path`）。
pub fn audio_path(root: &Path, entry: &VoiceEntry) -> PathBuf {
    library_dir(root).join(&entry.file)
}

/// 索引里的 `file` 是否是我们自己生成的那种**裸文件名**。
///
/// `import_from` 已经校验过导入清单，但 `index.json` 是磁盘上的普通文件——手改（或被人
/// 骗着改）成 `../../x.wav` 就能让"音色"指向库外。这里做同一道判断。
pub fn file_is_safe(file: &str) -> bool {
    !file.is_empty()
        && !file.contains('/')
        && !file.contains('\\')
        && Path::new(file).components().count() == 1
}

/// 取一个**可以安全读写**的库内音频路径：文件名合法 + 确实是库内的文件。
pub fn usable_audio_path(root: &Path, entry: &VoiceEntry) -> Result<PathBuf, String> {
    if !file_is_safe(&entry.file) {
        return Err(format!(
            "音色「{}」的索引里文件名不合法（{}）——修好 index.json，或重新存一次",
            entry.name, entry.file
        ));
    }
    let path = audio_path(root, entry);
    // 用 symlink_metadata 而不是 is_file：库里的 `x.wav` 若是指向库外文件的软链，
    // is_file() 会跟着它读出去（复核指出）。我们要的是"库里真有一份音频"。
    let meta = std::fs::symlink_metadata(&path).map_err(|_| {
        format!(
            "音色「{}」的音频不在库里（{}）——重新存一次，或从备份导入",
            entry.name,
            path.display()
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "音色「{}」的音频不是普通文件（{}）——符号链接/目录都不接受，重新存一次",
            entry.name,
            path.display()
        ));
    }
    Ok(path)
}

/// 把一个音频文件收进音色库（自包含：复制副本）。
///
/// 同名（忽略大小写）覆盖：只留一条，音频文件按新名字的扩展名替换。
/// `sanitize` 由调用方传（与工程名同一套：`crate::file_stem`），保证库里的文件名安全。
pub fn add_from_file(
    root: &Path,
    name: &str,
    src_audio: &Path,
    note: &str,
    now_ms: u64,
    sanitize: impl Fn(&str) -> String,
) -> Result<VoiceEntry, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("先给音色起个名字".into());
    }
    if !src_audio.is_file() {
        return Err(format!("参考音频不存在或不可读：{}", src_audio.display()));
    }
    let Some(ext) = ext_of(src_audio) else {
        return Err(format!(
            "参考音频没有扩展名，认不出格式：{}（支持 wav / mp3 / flac / m4a / ogg）",
            src_audio.display()
        ));
    };
    if !AUDIO_EXTS.contains(&ext.as_str()) {
        return Err(format!(
            "不支持的参考音频格式 .{ext}（支持 wav / mp3 / flac / m4a / ogg）"
        ));
    }
    let bytes = std::fs::read(src_audio)
        .map_err(|e| format!("参考音频读不出来：{}（{e}）", src_audio.display()))?;
    if bytes.is_empty() {
        return Err(format!("参考音频是空文件：{}", src_audio.display()));
    }
    // wav 顺手验一下能不能解析：坏 wav 收进库只会在合成时报错，越早发现越好
    if ext == "wav" {
        if let Err(e) = aw_core::dub::wav_duration(&bytes) {
            return Err(format!("这个 wav 解析不了（{e}）：{}", src_audio.display()));
        }
    }

    // **先读索引**：索引坏了就别动任何音色文件（否则会留下"索引没有、文件被换"的残局）
    let mut index = load(root)?;
    let slug = sanitize(name);
    let file = library_file_name(&slug, &name.to_lowercase(), &ext);
    let dir = library_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建音色库目录失败：{}（{e}）", dir.display()))?;
    let dst = dir.join(&file);
    // 用原子写而不是 fs::copy：库里这份是"用户的音色资产"，不能留半截文件
    aw_core::dub::write_atomic_explained(&dst, &bytes)
        .map_err(|e| format!("音色音频写入失败（{}）：{e}", dst.display()))?;

    let entry = VoiceEntry {
        name: name.to_string(),
        file,
        note: note.trim().to_string(),
        created_at: now_ms,
    };
    // 覆盖旧条目时记住旧文件；**索引落盘成功之后**再清（顺序反了会留下"索引指向已删文件"）
    let stale_old = index
        .get(name)
        .map(|old| audio_path(root, old))
        .filter(|p| *p != dst);
    index.upsert(entry.clone());
    save_index(root, &index)?;
    if let Some(old_path) = stale_old {
        // 清不掉也无所谓：那个文件已经没有条目引用（哑文件），不能因此把新音色回滚
        let _ = std::fs::remove_file(&old_path);
    }
    Ok(entry)
}

/// 导出：`<dest>/<音色名>/voice.json` + 音频副本。返回写出的目录。
pub fn export_to(
    root: &Path,
    name: &str,
    dest_root: &Path,
    sanitize: impl Fn(&str) -> String,
) -> Result<PathBuf, String> {
    let index = load(root)?;
    let Some(entry) = index.get(name) else {
        return Err(format!("音色库里没有「{name}」"));
    };
    let src = usable_audio_path(root, entry)?;
    // 目录名同样带名字哈希后缀：`a/b` 与 `a_b` 归一后不能落到同一个导出目录
    let dir = dest_root.join(format!(
        "{}-{}",
        sanitize(&entry.name),
        &name_suffix(&entry.name.to_lowercase())[..8]
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建导出目录失败：{}（{e}）", dir.display()))?;
    let meta = serde_json::to_vec_pretty(entry).map_err(|e| format!("导出序列化失败：{e}"))?;
    aw_core::dub::write_atomic_explained(&dir.join(EXPORT_META), &meta)
        .map_err(|e| format!("导出清单写入失败：{e}"))?;
    let bytes = std::fs::read(&src).map_err(|e| format!("读音色音频失败：{e}"))?;
    aw_core::dub::write_atomic_explained(&dir.join(&entry.file), &bytes)
        .map_err(|e| format!("导出音频写入失败：{e}"))?;
    Ok(dir)
}

/// 导入：从 `<src_dir>/voice.json` + 音频副本收进库（自包含）。
pub fn import_from(
    root: &Path,
    src_dir: &Path,
    now_ms: u64,
    sanitize: impl Fn(&str) -> String,
) -> Result<VoiceEntry, String> {
    let meta_path = src_dir.join(EXPORT_META);
    let raw = std::fs::read_to_string(&meta_path)
        .map_err(|e| format!("导入失败：读不到 {}（{e}）", meta_path.display()))?;
    let entry: VoiceEntry = serde_json::from_str(&raw)
        .map_err(|e| format!("导入失败：{} 解析不了（{e}）", meta_path.display()))?;
    // 清单里的 audio 名只允许裸文件名：`../x.wav` 这种既可能是坏包，也可能是恶意构造
    if !file_is_safe(&entry.file) {
        return Err(format!(
            "导入失败：清单里的音频名不合法（{}）——只允许裸文件名",
            entry.file
        ));
    }
    let src = src_dir.join(&entry.file);
    let meta = std::fs::symlink_metadata(&src)
        .map_err(|_| format!("导入失败：音频文件不在包里（{}）", src.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "导入失败：包里的音频不是普通文件（{}）——符号链接/目录都不接受",
            src.display()
        ));
    }
    let bytes = std::fs::read(&src).map_err(|e| format!("导入失败：读音频（{e}）"))?;
    if bytes.is_empty() {
        return Err(format!("导入失败：音频是空文件（{}）", src.display()));
    }
    let name = if entry.name.trim().is_empty() {
        "导入的音色".to_string()
    } else {
        entry.name.trim().to_string()
    };
    // 统一走"从文件入库"这条路：校验格式、复制副本、同名覆盖、写索引都在一处
    add_from_file(root, &name, &src, &entry.note, now_ms, sanitize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(name: &str) -> String {
        crate::file_stem(name)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-voices-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个能通过校验的最小 wav（24kHz 单声道 16bit，0.1 秒）。
    ///
    /// 手写 RIFF 头：`hound` 只是 aw-core 的依赖，这个 crate 里没有它
    /// （测试只需要一段确定能被 `wav_duration` 解析的字节）。
    fn write_wav(path: &Path, fill: i16) {
        let rate = 24_000u32;
        let frames = 2_400u32;
        let mut data = Vec::with_capacity(frames as usize * 2);
        for _ in 0..frames {
            data.extend_from_slice(&fill.to_le_bytes());
        }
        let mut out = Vec::with_capacity(data.len() + 44);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn add_copies_audio_so_the_library_is_self_contained() {
        let root = temp_dir("add");
        let src = root.join("外链参考.wav");
        write_wav(&src, 1000);

        let entry = add_from_file(&root, "口播男声", &src, "试音用", 42, sanitize).unwrap();
        assert_eq!(entry.name, "口播男声");
        assert!(
            entry.file.starts_with("口播男声-") && entry.file.ends_with(".wav"),
            "库内文件名 = slug + 名字哈希后缀：{}",
            entry.file
        );
        let in_lib = audio_path(&root, &entry);
        assert!(in_lib.is_file());
        let lib_bytes = std::fs::read(&in_lib).unwrap();
        assert_eq!(
            lib_bytes,
            std::fs::read(&src).unwrap(),
            "库里应是同一份音频"
        );

        // 原文件删掉后，库里那份仍然可用（这正是"自包含"的意义）
        std::fs::remove_file(&src).unwrap();
        assert!(audio_path(&root, &entry).is_file());

        let index = load(&root).unwrap();
        assert_eq!(index.voices.len(), 1);
        assert_eq!(index.get("口播男声").unwrap().note, "试音用");
    }

    #[test]
    fn same_name_overwrites_case_insensitively() {
        let root = temp_dir("overwrite");
        let a = root.join("a.wav");
        let b = root.join("b.wav");
        write_wav(&a, 100);
        write_wav(&b, 200);
        add_from_file(&root, "我的音色", &a, "", 1, sanitize).unwrap();
        add_from_file(&root, "我的音色", &b, "换了", 2, sanitize).unwrap();

        let index = load(&root).unwrap();
        assert_eq!(index.voices.len(), 1, "同名只该留一条");
        assert_eq!(index.voices[0].note, "换了");
        let entry = index.get("我的音色").unwrap();
        assert_eq!(
            std::fs::read(audio_path(&root, entry)).unwrap(),
            std::fs::read(&b).unwrap(),
            "覆盖后库里应是新音频"
        );
    }

    #[test]
    fn rejects_bad_inputs_with_actionable_notes() {
        let root = temp_dir("bad");
        // 空名字
        let wav = root.join("ok.wav");
        write_wav(&wav, 1);
        assert!(add_from_file(&root, "   ", &wav, "", 1, sanitize)
            .unwrap_err()
            .contains("名字"));
        // 不存在的文件
        let missing = root.join("没有.wav");
        assert!(add_from_file(&root, "x", &missing, "", 1, sanitize)
            .unwrap_err()
            .contains("不存在"));
        // 不支持的扩展名
        let bad_ext = root.join("参考.txt");
        std::fs::write(&bad_ext, b"not audio").unwrap();
        let err = add_from_file(&root, "x", &bad_ext, "", 1, sanitize).unwrap_err();
        assert!(err.contains("不支持的参考音频格式"), "{err}");
        // 空文件
        let empty = root.join("empty.wav");
        std::fs::write(&empty, b"").unwrap();
        assert!(add_from_file(&root, "x", &empty, "", 1, sanitize)
            .unwrap_err()
            .contains("空文件"));
        // 坏 wav
        let broken = root.join("broken.wav");
        std::fs::write(&broken, b"not a wav").unwrap();
        let err = add_from_file(&root, "x", &broken, "", 1, sanitize).unwrap_err();
        assert!(err.contains("解析不了"), "{err}");
    }

    #[test]
    fn load_reports_broken_index_instead_of_silently_empty() {
        let root = temp_dir("broken-index");
        assert_eq!(
            load(&root).unwrap(),
            VoiceIndex::default(),
            "第一次用 = 空库"
        );
        std::fs::create_dir_all(library_dir(&root)).unwrap();
        std::fs::write(library_dir(&root).join(INDEX_FILE), b"{ not json").unwrap();
        let err = load(&root).unwrap_err();
        assert!(err.contains("解析失败"), "{err}");
    }

    #[test]
    fn export_then_import_round_trips() {
        let root = temp_dir("export");
        let other = temp_dir("import");
        let src = root.join("ref.wav");
        write_wav(&src, 777);
        add_from_file(&root, "跨机音色", &src, "从 A 机导出", 7, sanitize).unwrap();

        let out = root.join("导出到这里");
        let dir = export_to(&root, "跨机音色", &out, sanitize).unwrap();
        assert!(dir.join(EXPORT_META).is_file());
        let exported = load(&root).unwrap().get("跨机音色").unwrap().clone();
        assert!(
            dir.join(&exported.file).is_file(),
            "导出的音频名应与库里一致：{}",
            exported.file
        );

        let entry = import_from(&other, &dir, 9, sanitize).unwrap();
        assert_eq!(entry.name, "跨机音色");
        assert_eq!(entry.note, "从 A 机导出");
        let index = load(&other).unwrap();
        assert_eq!(index.voices.len(), 1);
        assert_eq!(
            std::fs::read(audio_path(&other, index.get("跨机音色").unwrap())).unwrap(),
            std::fs::read(&src).unwrap(),
            "导入后音频应与导出前一致"
        );
    }

    #[test]
    fn import_rejects_missing_meta_missing_audio_and_path_traversal() {
        let root = temp_dir("import-bad");
        let pkg = temp_dir("pkg");

        // 没有 voice.json
        assert!(import_from(&root, &pkg, 1, sanitize)
            .unwrap_err()
            .contains("读不到"));

        // 清单里指名的音频不在包里
        std::fs::write(
            pkg.join(EXPORT_META),
            serde_json::to_vec(&VoiceEntry {
                name: "缺音频".into(),
                file: "没有.wav".into(),
                note: String::new(),
                created_at: 1,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(import_from(&root, &pkg, 1, sanitize)
            .unwrap_err()
            .contains("音频文件不在包里"));

        // 音频名带路径：必须拒绝（不能顺着清单去读包外的文件）
        std::fs::write(
            pkg.join(EXPORT_META),
            serde_json::to_vec(&VoiceEntry {
                name: "穿越".into(),
                file: "../外面的.wav".into(),
                note: String::new(),
                created_at: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let err = import_from(&root, &pkg, 1, sanitize).unwrap_err();
        assert!(err.contains("不合法"), "{err}");
    }

    /// 两个不同的名字归一后是**同一个 slug**（`a/b` vs `a_b`）：文件名必须仍然不同，
    /// 否则后一条会把前一条的音频覆盖掉（复核指出）。
    #[test]
    fn names_that_sanitize_the_same_still_get_distinct_files() {
        let root = temp_dir("slug-collision");
        let a = root.join("a.wav");
        let b = root.join("b.wav");
        write_wav(&a, 100);
        write_wav(&b, 200);

        let e1 = add_from_file(&root, "a/b", &a, "", 1, sanitize).unwrap();
        let e2 = add_from_file(&root, "a_b", &b, "", 2, sanitize).unwrap();
        assert_ne!(
            e1.file, e2.file,
            "两个名字不能共用一个文件：{e1:?} / {e2:?}"
        );

        let index = load(&root).unwrap();
        assert_eq!(index.voices.len(), 2, "两条都要在库里");
        assert_eq!(
            std::fs::read(audio_path(&root, &e1)).unwrap(),
            std::fs::read(&a).unwrap(),
            "第一条的音频不能被第二条覆盖"
        );
        assert_eq!(
            std::fs::read(audio_path(&root, &e2)).unwrap(),
            std::fs::read(&b).unwrap()
        );
    }

    /// 库里的音频是**指向库外文件的符号链接**：不接受（否则"自包含"是假的）。
    #[cfg(unix)]
    #[test]
    fn symlinked_audio_in_library_is_refused() {
        let root = temp_dir("symlink-lib");
        let outside = root.join("外面的.wav");
        write_wav(&outside, 9);
        let mut index = VoiceIndex::default();
        let file = "伪装.wav".to_string();
        std::fs::create_dir_all(library_dir(&root)).unwrap();
        std::os::unix::fs::symlink(&outside, library_dir(&root).join(&file)).unwrap();
        index.voices.push(VoiceEntry {
            name: "伪装".into(),
            file,
            note: String::new(),
            created_at: 1,
        });
        save_index(&root, &index).unwrap();

        let entry = load(&root).unwrap().voices[0].clone();
        let err = usable_audio_path(&root, &entry).unwrap_err();
        assert!(err.contains("不是普通文件"), "{err}");
        assert!(
            export_to(&root, "伪装", &root.join("out"), sanitize).is_err(),
            "导出也要拒绝符号链接"
        );
    }

    /// 手改 `index.json` 把 `file` 写成库外路径：导出/应用都必须拒绝，不能顺着它读库外文件。
    #[test]
    fn unsafe_index_file_names_are_refused() {
        let root = temp_dir("unsafe-index");
        let src = root.join("ok.wav");
        write_wav(&src, 5);
        add_from_file(&root, "正常", &src, "", 1, sanitize).unwrap();

        // 手改索引：这条"音色"指向库外
        let mut index = load(&root).unwrap();
        index.voices[0].file = "../外面的.wav".into();
        save_index(&root, &index).unwrap();

        let entry = load(&root).unwrap().voices[0].clone();
        let err = usable_audio_path(&root, &entry).unwrap_err();
        assert!(err.contains("文件名不合法"), "{err}");
        let err = export_to(&root, "正常", &root.join("out"), sanitize).unwrap_err();
        assert!(err.contains("文件名不合法"), "导出也要挡住：{err}");

        // 绝对路径同样不行
        let mut index = load(&root).unwrap();
        index.voices[0].file = "/etc/passwd".into();
        save_index(&root, &index).unwrap();
        let entry = load(&root).unwrap().voices[0].clone();
        assert!(usable_audio_path(&root, &entry).is_err());
    }

    #[test]
    fn export_unknown_voice_is_an_error() {
        let root = temp_dir("export-missing");
        let err = export_to(&root, "没有这个音色", &root.join("out"), sanitize).unwrap_err();
        assert!(err.contains("没有「没有这个音色」"), "{err}");
    }
}
