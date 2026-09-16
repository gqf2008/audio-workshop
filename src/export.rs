//! 导出：单篇（当前工程）与批量（projects/ 下所有有成品）共用同一份复制实现。
//!
//! 为什么单篇与批量放在一个模块：两者的差别只在"导给谁"，落盘动作必须一模一样
//! （原子复制、覆盖语义、失败文案）。写成两份的话，"单篇导出修了、批量导出没修"
//! 是迟早的事。
//!
//! 批量导出（产品计划 §4.2 P6 的「批量导出全部工程」）故意**不经过 worker**：
//! 它只读磁盘上已经拼好的成品（`out/final.wav` 是原子写出来的），与正在跑的合成
//! 互不干扰，跑在后台线程就够，不必占用任务队列的提交守卫。

use std::path::{Path, PathBuf};

/// 单篇导出结果。未勾选格式也给出明确结果，不静默返回。
#[derive(Debug, PartialEq, Eq)]
pub enum ExportOutcome {
    Exported(PathBuf),
    NoneSelected,
    Failed(String),
}

/// 按导出开关把一份成品（wav，可选 srt）复制到导出目录。
///
/// `srt: None` 表示这份成品没有字幕文件；此时若勾了 SRT，直接给一条能照着做的
/// 失败文案，而不是让 `copy_atomic` 抛一个裸 errno。
pub fn export_one(
    name: &str,
    dir: &Path,
    wav: &Path,
    srt: Option<&Path>,
    wav_on: bool,
    srt_on: bool,
) -> ExportOutcome {
    if !wav_on && !srt_on {
        return ExportOutcome::NoneSelected;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return ExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    let mut written: Vec<String> = Vec::new();
    if wav_on {
        let t = dir.join(format!("{name}.wav"));
        if let Err(e) = aw_core::dub::copy_atomic(wav, &t) {
            return ExportOutcome::Failed(aw_core::dub::write_failure_note(&t, 0, &e));
        }
        written.push(t.display().to_string());
    }
    if srt_on {
        let Some(srt) = srt else {
            // 文案必须跟着**实际做了什么**走：只勾了 SRT 时我们一个文件都没写，
            // 说"只导出了 WAV"是假的（复核抓到）。
            return ExportOutcome::Failed(if wav_on {
                format!("{name}：没有字幕文件（out/final.srt），这条只导出了 WAV；重新导出时把它一起带上")
            } else {
                format!("{name}：没有字幕文件（out/final.srt），这条什么都没导出")
            });
        };
        let t = dir.join(format!("{name}.srt"));
        if let Err(e) = aw_core::dub::copy_atomic(srt, &t) {
            return ExportOutcome::Failed(aw_core::dub::write_failure_note(&t, 0, &e));
        }
        written.push(t.display().to_string());
    }
    let last = written
        .last()
        .cloned()
        .unwrap_or_else(|| dir.display().to_string());
    ExportOutcome::Exported(PathBuf::from(last))
}

/// 一份工程里可导出的轨（P6「人声+BGM 分轨」的那两轨，加上混音轨）。
///
/// 命名沿用 BGM Tab 逐轨导出的既有约定（`<工程名>_voice.wav` / `_bgm.wav` / `_mixed.wav`）：
/// **同一轨不能因为从哪个按钮导出就换个名字**，否则导出目录里会同时出现同一内容的两份文件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stem {
    /// 人声 / 配音成品
    Voice,
    /// BGM 轨
    Bgm,
    /// 混音成品（人声 + BGM，已按句压 BGM）
    Mixed,
}

impl Stem {
    /// 文件后缀（既有约定，别改：BGM Tab 一直用它）
    pub fn suffix(self) -> &'static str {
        match self {
            Stem::Voice => "voice",
            Stem::Bgm => "bgm",
            Stem::Mixed => "mixed",
        }
    }

    /// 界面上的名字
    pub fn label(self) -> &'static str {
        match self {
            Stem::Voice => "人声",
            Stem::Bgm => "BGM",
            Stem::Mixed => "混音",
        }
    }
}

/// 一轨在磁盘上的状态：能不能当成"当前工程的成品"导出。
///
/// 光"文件存在"不够：`out/mixed.wav`、`bgm/bgm.wav` 是**混音那一刻**的成品，之后改稿、
/// 重录、重新拼装都会让配音成品变样——旧混音再导出就成了"新配音配旧 BGM"。
/// 所以混音成功时会写下 `bgm/mix-manifest.json`（记录当时的配音成品指纹），
/// 这里比对着看（复核指出"磁盘来源可能把旧产物当当前产物导出"）。
#[derive(Debug, PartialEq, Eq)]
pub enum StemState {
    Ready(PathBuf),
    /// 文件在，但和当前配音成品不是一次出来的（原因给用户看）
    Stale(String),
    Missing,
}

/// 配音成品的指纹（sha256）。没有配音成品 → None。
pub fn voice_fingerprint(project_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(project_dir.join("out/final.wav")).ok()?;
    Some(sha256_hex(&bytes))
}

/// 导出分轨时"当前 BGM 设定"的两件事：UI 认不认这套结果（`current`）＋参数摘要。
///
/// 参数摘要（prompt + duck 档位换算出的系数）是**跨会话**判断"这套产物是不是还配套"
/// 的依据：描述改了、压低强度改了，旧产物就不该再当当前结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BgmContext {
    /// UI 是否认为磁盘上这套 BGM 结果仍是当前结果（改描述会置 stale）
    pub current: bool,
    /// 当前设定的摘要
    pub options_digest: String,
}

impl BgmContext {
    pub fn new(current: bool, prompt: &str, duck_gain: f32) -> Self {
        Self {
            current,
            options_digest: bgm_options_digest(prompt, duck_gain),
        }
    }
}

/// BGM 参数摘要：只看**影响产物内容**的输入（描述 + 压低强度）。
///
/// 不把 `current`（UI 标志）算进去——那是"会话内有没有作废"，与"产物是什么参数做出来的"
/// 是两件事，混在一起就没法跨会话比对了。
pub fn bgm_options_digest(prompt: &str, duck_gain: f32) -> String {
    // 用固定小数位而不是 Debug 输出 f32：`0.22_f32` 的 Debug 形态会随格式化细节变化
    let body = format!("prompt={prompt}\nduck={duck_gain:.4}\n");
    sha256_hex(body.as_bytes())
}

/// BGM 产物清单：这次生成/混音**用的什么参数**、混音时配的是哪份配音成品。
///
/// 单独一个文件（不塞进 `bgm/manifest.json`）：那个管分段缓存复用，字段与失效规则都不同。
pub fn write_result_manifest(project_dir: &Path, options_digest: &str) -> std::io::Result<()> {
    let fp = voice_fingerprint(project_dir);
    let path = project_dir.join(RESULT_MANIFEST);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let voice = match &fp {
        Some(fp) => format!("\"{fp}\""),
        None => "null".to_string(),
    };
    let body = format!("{{\"bgm_options\":\"{options_digest}\",\"voice_sha256\":{voice}}}\n");
    aw_core::dub::write_atomic_explained(&path, body.as_bytes())
}

/// 清单里的字段（只读原始 JSON 文本：这个文件是我们自己写的，字段少、格式稳定，
/// 为它引一个 JSON 解析依赖不划算；读不出来一律当"没有清单"）。
fn read_result_manifest(project_dir: &Path) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(project_dir.join(RESULT_MANIFEST)).ok()?;
    let options = json_string_field(&text, "\"bgm_options\"")?;
    let voice = json_string_field(&text, "\"voice_sha256\"");
    Some((options, voice))
}

/// 从一个平铺的 JSON 文本里取字符串字段（`"k":"v"`）；`"k":null` 与缺失都返回 None。
fn json_string_field(text: &str, key: &str) -> Option<String> {
    let rest = text.split(key).nth(1)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    Some(rest.split('"').next()?.to_string())
}

/// 磁盘上这套 `bgm/bgm.wav`、`out/mixed.wav` 是不是**用当前参数、配当前配音成品**做出来的。
///
/// 三件事都要对上：清单存在、参数摘要一致、配音成品指纹一致（没有配音成品时=独立生成，
/// 不比对指纹）。老工程（这批之前生成的）没有清单 → 返回 false：宁可让用户重新生成一次
/// （分段有缓存，几秒），也不拿判不出配套关系的产物当当前结果。
pub fn bgm_result_is_current(project_dir: &Path, options_digest: &str) -> bool {
    let Some((recorded_options, recorded_voice)) = read_result_manifest(project_dir) else {
        return false;
    };
    if recorded_options != options_digest {
        return false;
    }
    match voice_fingerprint(project_dir) {
        Some(fp) => recorded_voice.as_deref() == Some(fp.as_str()),
        // 没有配音成品：独立生成的 BGM，只比参数
        None => true,
    }
}

/// 产物清单的相对路径（放在 bgm/ 下，与 BGM 的分段 manifest 分开：那个管缓存复用，
/// 这个管"这套产物是哪份参数/哪版配音做出来的"，语义与失效规则都不同）。
const RESULT_MANIFEST: &str = "bgm/result-manifest.json";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 一轨**从磁盘**解析出来的状态（不看内存里的 `BgmArtifacts`）：
/// `ctx` 里的 `current` 由调用方给（UI 认为这套结果还算不算数），`options_digest`
/// 用来跟产物清单比对——两者合起来才能回答"这套 BGM 是不是当前设定做出来的"。
/// 只认内存会让"昨天混好的分轨，今天重开应用就导不出来"。
///
/// - 人声轨：`out/final.wav`。混音时那份 `out/voice.wav` 只是它的副本，副本可能是旧的
///   （复核指出"优先用 voice.wav 会导出旧人声"），所以直接以配音成品为准。
/// - BGM 轨：`bgm/bgm.wav`；混音轨：`out/mixed.wav`。两者都要过时效判定。
pub fn stem_state(project_dir: &Path, stem: Stem, ctx: &BgmContext) -> StemState {
    match stem {
        Stem::Voice => match project_dir.join("out/final.wav") {
            p if p.is_file() => StemState::Ready(p),
            _ => StemState::Missing,
        },
        Stem::Bgm | Stem::Mixed => {
            let path = match stem {
                Stem::Bgm => project_dir.join("bgm/bgm.wav"),
                _ => project_dir.join("out/mixed.wav"),
            };
            if !path.is_file() {
                return StemState::Missing;
            }
            // 三层判断各管一件事：UI 认不认这套结果、参数摘要一不一致、配音指纹配不配得上。
            if !ctx.current {
                return StemState::Stale(format!(
                    "{}轨已被改稿/改 BGM 描述作废（重新生成并混音后再导）",
                    stem.label()
                ));
            }
            if bgm_result_is_current(project_dir, &ctx.options_digest) {
                StemState::Ready(path)
            } else {
                StemState::Stale(format!(
                    "{}轨是上一次混音的产物，与当前配音成品对不上（改稿/重录后没重新混音）",
                    stem.label()
                ))
            }
        }
    }
}

/// 分轨导出的结果（P6「人声+BGM 分轨」用）。/// 分轨导出的结果（P6「人声+BGM 分轨」用）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StemExportSummary {
    /// 实际写出的文件
    pub written: Vec<PathBuf>,
    /// 没有这一轨（例如还没生成过 BGM）——不是失败，但要如实说
    pub missing: Vec<String>,
    /// 这一轨在磁盘上，但和当前配音成品不是一次混出来的（改稿/重录后没重新混音）
    pub stale: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StemExportOutcome {
    Done(StemExportSummary),
    /// 一轨都没有（连配音成品都没有）：说明白"先跑一次配音"
    NothingToExport,
    /// 有的轨存在但已过期，且没有一轨能导：告诉用户重新混音
    Stale(Vec<String>),
    Failed(String),
}

/// 导出一轨（BGM Tab 的逐轨导出走这里）。
pub fn export_stem(
    name: &str,
    project_dir: &Path,
    dir: &Path,
    stem: Stem,
    ctx: &BgmContext,
) -> StemExportOutcome {
    let src = match stem_state(project_dir, stem, ctx) {
        StemState::Ready(p) => p,
        StemState::Stale(reason) => return StemExportOutcome::Stale(vec![reason]),
        StemState::Missing => return StemExportOutcome::NothingToExport,
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return StemExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    let dst = dir.join(format!("{name}_{}.wav", stem.suffix()));
    match aw_core::dub::copy_atomic(&src, &dst) {
        Ok(()) => StemExportOutcome::Done(StemExportSummary {
            written: vec![dst],
            ..Default::default()
        }),
        Err(e) => StemExportOutcome::Failed(aw_core::dub::write_failure_note(&dst, 0, &e)),
    }
}

/// 一次导出「人声 + BGM」两轨（P6 的分轨导出）。
///
/// 缺一轨不算整次失败：人声导出成功、BGM 还没生成时，用户要的是"拿到人声 + 知道 BGM 没有"，
/// 而不是一个"全失败"。`missing` 里会写清缺哪一轨。
pub fn export_stems(
    name: &str,
    project_dir: &Path,
    dir: &Path,
    ctx: &BgmContext,
) -> StemExportOutcome {
    let wanted = [Stem::Voice, Stem::Bgm];
    let mut ready: Vec<(Stem, PathBuf)> = Vec::new();
    let mut summary = StemExportSummary::default();
    for stem in wanted {
        match stem_state(project_dir, stem, ctx) {
            StemState::Ready(p) => ready.push((stem, p)),
            StemState::Stale(reason) => summary.stale.push(reason),
            StemState::Missing => summary.missing.push(stem.label().to_string()),
        }
    }
    // 全过期：一轨都不该导，直接告诉用户重新混音（别把旧产物当当前成品发出去）
    if ready.is_empty() {
        return if summary.stale.is_empty() {
            StemExportOutcome::NothingToExport
        } else {
            StemExportOutcome::Stale(summary.stale)
        };
    }
    // 先解析来源再建目录：一轨都没有时不该在导出目录里留下一个空目录
    if let Err(e) = std::fs::create_dir_all(dir) {
        return StemExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    for (stem, src) in ready {
        let dst = dir.join(format!("{name}_{}.wav", stem.suffix()));
        if let Err(e) = aw_core::dub::copy_atomic(&src, &dst) {
            return StemExportOutcome::Failed(aw_core::dub::write_failure_note(&dst, 0, &e));
        }
        summary.written.push(dst);
    }
    StemExportOutcome::Done(summary)
}

/// 分轨导出的状态行文案。
pub fn stem_summary_text(summary: &StemExportSummary, dir: &Path) -> String {
    if summary.written.is_empty() {
        return "分轨导出：没有可导出的轨".to_string();
    }
    let names: Vec<String> = summary
        .written
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut text = format!("分轨导出：{} → {}", names.join(" / "), dir.display());
    if !summary.missing.is_empty() {
        // 缺哪一轨给的下一步不一样：人声来自配音合成，BGM 来自 BGM 页。
        // 一句"先在 BGM 页生成"套到人声上就是错的指引（本机真实工程里
        // 就有"只有 BGM、没人声"的这种，正好照出来）。
        let hints: Vec<String> = summary
            .missing
            .iter()
            .map(|m| match m.as_str() {
                "人声" => "还没有人声轨（先完成配音合成）".to_string(),
                "BGM" => "还没有 BGM 轨（先在 BGM 页生成）".to_string(),
                other => format!("还没有{other}轨"),
            })
            .collect();
        text.push_str(&format!(" · {}", hints.join("；")));
    }
    if !summary.stale.is_empty() {
        text.push_str(&format!(" · {}", summary.stale.join("；")));
    }
    text
}

/// 一个"有成品"的工程（批量导出的输入）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectOut {
    /// 工程名（= 目录名，也是导出到导出目录时的文件名）
    pub name: String,
    pub wav: PathBuf,
    pub srt: Option<PathBuf>,
}

/// 扫出 `projects/` 下有成品（`out/final.wav`）的工程。
///
/// 顺序按工程名排序：批量导出是"一次性把一批文件放进导出目录"，顺序不稳定会让
/// 每次跑的结果对不上（也让测试变成碰运气）。没有 `out/final.wav` 的目录直接不算
/// 候选——那是没跑完/跑失败/别的杂物，不是"导出失败"。
///
/// **目录读不出来要报错**，不能和"目录里没有成品"合成同一个结果：前者用户要去看
/// 权限/路径，后者是"你还没跑过配音"，两句话完全不同。
pub fn scan_projects(root: &Path) -> Result<ScanOutcome, String> {
    if !root.is_dir() {
        return Err(format!(
            "工程目录不存在：{}（还没有跑过配音？）",
            root.display()
        ));
    }
    let entries = std::fs::read_dir(root)
        .map_err(|e| format!("读工程目录失败：{}（{e}）", root.display()))?;
    let mut scan = ScanOutcome::default();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            // 目录项读不出来要如实报出来：`.flatten()` 会把它静默吞掉，
            // 用户只会看到"怎么少导了一篇"（复核抓到）
            Err(e) => {
                scan.problems
                    .push(format!("读取目录项失败（该篇没导）：{e}"));
                continue;
            }
        };
        let dir = entry.path();
        // 符号链接一律不跟：projects/ 下放一个指向别处的链接，导出就会把工程目录
        // 之外的东西复制出来（路径逃逸）。目录名本身也要求是 UTF-8，否则
        // `to_string_lossy` 会把两个不同的目录名压成同一个名字、互相覆盖产物。
        let md = match std::fs::symlink_metadata(&dir) {
            Ok(md) => md,
            Err(e) => {
                scan.problems
                    .push(format!("读目录属性失败（该篇没导）：{e}"));
                continue;
            }
        };
        if md.file_type().is_symlink() {
            scan.problems
                .push("跳过符号链接（不导出工程目录之外的内容）".to_string());
            continue;
        }
        if !md.is_dir() {
            continue;
        }
        let wav = dir.join("out/final.wav");
        if !wav.is_file() {
            continue;
        }
        let name = match export_name(&entry.file_name()) {
            Ok(n) => n,
            Err(note) => {
                scan.problems.push(note);
                continue;
            }
        };
        let srt = dir.join("out/final.srt");
        scan.projects.push(ProjectOut {
            name,
            wav,
            srt: srt.is_file().then_some(srt),
        });
    }
    scan.projects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(scan)
}

/// 目录名 → 导出用的工程名。
///
/// 非 UTF-8 直接拒绝而不是 `to_string_lossy`：lossy 会把两个不同的目录名压成含 `�`
/// 的同一个字符串，导出时就写到同一个 `<名字>.wav` 上互相覆盖（复核抓到）。
fn export_name(file_name: &std::ffi::OsStr) -> Result<String, String> {
    match file_name.to_str() {
        Some(n) => Ok(n.to_string()),
        None => Err(format!(
            "工程目录名不是 UTF-8，跳过（导出文件名会与别的工程撞车）：{}",
            file_name.to_string_lossy()
        )),
    }
}

/// 扫描结果：能导的工程 + 扫描时就发现的问题（读不出的目录项、非 UTF-8 目录名、
/// 符号链接）。问题不能吞——它们会让"少导了一篇"看起来像成功。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub projects: Vec<ProjectOut>,
    pub problems: Vec<String>,
}

/// 批量导出汇总。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchExportSummary {
    /// 扫到的工程数（= exported + failures.len()）
    pub total: usize,
    pub exported: usize,
    /// 导出失败的工程（含原因）。**一条失败不影响其它篇**：某篇缺字幕不该让
    /// 另外 19 篇也导不出去。
    pub failures: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BatchExportOutcome {
    Done(BatchExportSummary),
    NoneSelected,
    Failed(String),
}

/// 把 `root` 下所有有成品的工程，按导出开关复制到 `dir`。
pub fn export_all(root: &Path, dir: &Path, wav_on: bool, srt_on: bool) -> BatchExportOutcome {
    if !wav_on && !srt_on {
        return BatchExportOutcome::NoneSelected;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return BatchExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    let scan = match scan_projects(root) {
        Ok(p) => p,
        Err(e) => return BatchExportOutcome::Failed(e),
    };
    let mut summary = BatchExportSummary {
        // 扫描期发现问题的那些目录也算进总数：它们确实是"有个工程没导出成"
        total: scan.projects.len() + scan.problems.len(),
        failures: scan.problems,
        ..Default::default()
    };
    for p in scan.projects {
        match export_one(&p.name, dir, &p.wav, p.srt.as_deref(), wav_on, srt_on) {
            ExportOutcome::Exported(_) => summary.exported += 1,
            // NoneSelected 在上面已经拦过，这里只可能出现在"开关在循环里变"那种
            // 不可能的场景；当成失败报出来，别静默吞
            ExportOutcome::NoneSelected => summary
                .failures
                .push(format!("{}：没有勾选导出格式", p.name)),
            ExportOutcome::Failed(e) => summary.failures.push(e),
        }
    }
    BatchExportOutcome::Done(summary)
}

/// 批量导出的状态行文案（成功与失败分开说；失败要能看见第一条原因）。
pub fn summary_text(summary: &BatchExportSummary, dir: &Path) -> String {
    if summary.total == 0 {
        return format!("批量导出：没有找到有成品的工程（看 {}", dir.display());
    }
    let mut text = format!("批量导出：{} 篇 → {}", summary.exported, dir.display());
    if !summary.failures.is_empty() {
        text.push_str(&format!(
            " · 失败 {} 篇（{}）",
            summary.failures.len(),
            summary.failures[0]
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的参数摘要（真实摘要由 UI 的 prompt + duck 档算出来，见 `bgm_options_digest`）
    const TEST_DIGEST: &str = "digest-under-test";

    fn ctx_current() -> BgmContext {
        BgmContext {
            current: true,
            options_digest: TEST_DIGEST.to_string(),
        }
    }

    fn ctx_stale() -> BgmContext {
        BgmContext {
            current: false,
            options_digest: TEST_DIGEST.to_string(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个"跑完的工程"：projects/<name>/out/{final.wav,final.srt}
    fn make_project(root: &Path, name: &str, with_srt: bool) {
        let out = root.join(name).join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("final.wav"), format!("wav-{name}")).unwrap();
        if with_srt {
            std::fs::write(out.join("final.srt"), format!("srt-{name}")).unwrap();
        }
    }

    #[test]
    fn scan_lists_only_finished_projects_in_name_order() {
        let root = temp_dir("scan");
        make_project(&root, "乙工程", true);
        make_project(&root, "甲工程", true);
        // 没成品的目录：不算候选（不是"导出失败"）
        std::fs::create_dir_all(root.join("没跑完/out")).unwrap();
        std::fs::write(root.join("没跑完/out/中间物.txt"), b"x").unwrap();
        std::fs::write(root.join("随便一个文件.txt"), b"x").unwrap();

        let scan = scan_projects(&root).expect("工程目录存在");
        assert!(
            scan.problems.is_empty(),
            "不该有扫描问题：{:?}",
            scan.problems
        );
        let got = scan.projects;
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        // 排序是**码位序**（`String::cmp`）：乙(U+4E59) < 甲(U+7532)，所以乙在前。
        // 这里钉的是"顺序确定、与目录遍历顺序无关"，不是"字典序"——本地化排序会随
        // 系统语言变，那正是批量导出不该有的不确定性。
        assert_eq!(names, vec!["乙工程", "甲工程"]);
        assert!(got.iter().all(|p| p.srt.is_some()));
    }

    #[test]
    fn scan_marks_missing_srt_as_none() {
        let root = temp_dir("scan-nosrt");
        make_project(&root, "只有wav", false);
        let scan = scan_projects(&root).expect("工程目录存在");
        let got = scan.projects;
        assert_eq!(got.len(), 1);
        assert!(got[0].srt.is_none(), "缺字幕要如实记成 None");
    }

    #[test]
    fn export_all_copies_every_finished_project() {
        let root = temp_dir("all");
        let dst = temp_dir("all-dst");
        make_project(&root, "第一集", true);
        make_project(&root, "第二集", true);
        make_project(&root, "没跑完", false);
        std::fs::remove_file(root.join("没跑完/out/final.wav")).unwrap();

        match export_all(&root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.total, 2, "只算有成品的那两个");
                assert_eq!(s.exported, 2);
                assert!(s.failures.is_empty(), "{:?}", s.failures);
            }
            other => panic!("应导出成功：{other:?}"),
        }
        assert_eq!(
            std::fs::read(dst.join("第一集.wav")).unwrap(),
            "wav-第一集".as_bytes()
        );
        assert_eq!(
            std::fs::read(dst.join("第一集.srt")).unwrap(),
            "srt-第一集".as_bytes()
        );
        assert!(dst.join("第二集.wav").is_file());
        assert!(!dst.join("没跑完.wav").exists());
    }

    /// 某篇缺字幕只该让那篇失败：其它篇照常导出（失败隔离）。
    #[test]
    fn export_all_isolates_failures() {
        let root = temp_dir("isolate");
        let dst = temp_dir("isolate-dst");
        make_project(&root, "有字幕", true);
        make_project(&root, "没字幕", false);

        match export_all(&root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.total, 2);
                assert_eq!(s.exported, 1);
                assert_eq!(s.failures.len(), 1, "{:?}", s.failures);
                assert!(s.failures[0].contains("没字幕"), "{:?}", s.failures);
                assert!(s.failures[0].contains("字幕"), "{:?}", s.failures);
            }
            other => panic!("{other:?}"),
        }
        assert!(dst.join("有字幕.wav").is_file());
        assert!(dst.join("有字幕.srt").is_file());

        // 只导 WAV 时，缺字幕那篇不该算失败（没勾 SRT）
        let dst_wav = temp_dir("isolate-wav");
        match export_all(&root, &dst_wav, true, false) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.exported, 2, "只导 WAV 时两篇都该成");
                assert!(s.failures.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    /// 工程目录不存在（还没跑过配音 / 路径被删）要报出来，不能和"目录在但没成品"
    /// 共用一个结果——两句话对用户意味着完全不同的下一步。
    #[test]
    fn scan_reports_missing_projects_root() {
        let dir = temp_dir("missing-root");
        let missing = dir.join("根本没有这个目录");
        match scan_projects(&missing) {
            Err(e) => assert!(
                e.contains("工程目录不存在") && e.contains("根本没有这个目录"),
                "要说清是哪个目录：{e}"
            ),
            Ok(v) => panic!("目录不存在时不该返回空表：{v:?}"),
        }

        // 对照：目录在、但没有成品 → 成功返回空结果（export_all 会报"没有找到有成品"）
        let empty_root = temp_dir("empty-root");
        assert_eq!(scan_projects(&empty_root).unwrap(), ScanOutcome::default());
        let dst = temp_dir("empty-dst");
        match export_all(&empty_root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.total, 0);
                assert_eq!(s.exported, 0);
                assert!(s.failures.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    /// 非 UTF-8 目录名：**拒绝**而不是 `to_string_lossy`——两个不同的非法名字会被
    /// lossy 压成同一个含 `�` 的名字，导出时互相覆盖（复核抓到）。
    ///
    /// 用纯函数测：macOS 的 APFS 不允许创建非 UTF-8 的名字（`EILSEQ`），这条判定在
    /// Linux/ext4 上才会真的遇到，所以不能靠造目录来测。
    #[cfg(unix)]
    #[test]
    fn export_name_rejects_non_utf8_directory_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // 只有中间那个非法字节不同：lossy 之后两者**一模一样**
        let a = OsString::from_vec(vec![b'a', 0xff, b'1']);
        let b = OsString::from_vec(vec![b'a', 0xfe, b'1']);
        // 前提：lossy 之后两者会撞名（这正是不能用 lossy 的原因）
        assert_eq!(
            a.to_string_lossy(),
            b.to_string_lossy(),
            "前提变了，请重看这条理由"
        );

        for name in [a, b] {
            let err = export_name(&name).expect_err("非 UTF-8 目录名要拒绝");
            assert!(err.contains("不是 UTF-8"), "{err}");
            assert!(err.contains("撞车"), "要说清为什么拒绝：{err}");
        }
        assert_eq!(
            export_name(std::ffi::OsStr::new("第一集")).unwrap(),
            "第一集"
        );
    }

    /// 符号链接指向工程目录之外的东西：不能跟着导出（路径逃逸），而且要如实报出来。
    #[cfg(unix)]
    #[test]
    fn scan_reports_symlinked_directories() {
        let root = temp_dir("symlink-root");
        let outside = temp_dir("symlink-outside");
        make_project(&outside, "外面的工程", true);
        std::os::unix::fs::symlink(outside.join("外面的工程"), root.join("链接进来的")).unwrap();

        let scan = scan_projects(&root).expect("工程目录存在");
        assert!(
            scan.projects.is_empty(),
            "符号链接不该被当成可导出工程：{:?}",
            scan.projects
        );
        assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
        assert!(scan.problems[0].contains("符号链接"), "{:?}", scan.problems);

        let dst = temp_dir("symlink-dst");
        match export_all(&root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.exported, 0);
                assert_eq!(s.total, s.failures.len());
                assert_eq!(s.total, 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(!dst.join("外面的工程.wav").exists(), "不该把外面那篇导出来");
    }

    /// 造一个"混过音"的工程：out/{final,voice,mixed}.wav + bgm/bgm.wav
    fn make_mixed_project(root: &Path, name: &str) {
        let out = root.join(name).join("out");
        let bgm = root.join(name).join("bgm");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&bgm).unwrap();
        std::fs::write(out.join("final.wav"), b"final").unwrap();
        std::fs::write(out.join("voice.wav"), b"voice").unwrap();
        std::fs::write(out.join("mixed.wav"), b"mixed").unwrap();
        std::fs::write(bgm.join("bgm.wav"), b"bgm").unwrap();
        // 真混音时会写下产物清单（参数摘要 + 混音那一刻的 out/final.wav 指纹）；
        // 没有它就算"无法确认配套"
        write_result_manifest(&root.join(name), TEST_DIGEST).unwrap();
    }

    /// 参数摘要：同样输入稳定、改描述或改压低强度就变。它要能跨会话比对，
    /// 所以必须避开 `{:?}` 那种随格式化细节变化的表示。
    #[test]
    fn bgm_options_digest_tracks_prompt_and_duck() {
        let base = bgm_options_digest("温暖口播背景", 0.22);
        assert_eq!(
            base,
            bgm_options_digest("温暖口播背景", 0.22),
            "同样输入要稳定"
        );
        assert_ne!(
            base,
            bgm_options_digest("换成摇滚", 0.22),
            "描述变了摘要要变"
        );
        assert_ne!(
            base,
            bgm_options_digest("温暖口播背景", 0.05),
            "duck 变了摘要要变"
        );
        assert_eq!(base.len(), 64, "sha256 十六进制");
    }

    /// 产物清单：参数摘要与配音指纹都要对上才算"当前产物"；
    /// 独立生成（没有配音成品）只比参数；老工程没有清单 → 一律不算当前。
    #[test]
    fn result_manifest_checks_options_and_voice() {
        let root = temp_dir("manifest");
        make_project(&root, "甲", true); // 只有 out/final.wav/.srt
        let p = root.join("甲");
        let digest_a = bgm_options_digest("口播背景", 0.22);
        let digest_b = bgm_options_digest("另一个描述", 0.22);

        // 没有清单（老工程）→ 不算当前
        assert!(!bgm_result_is_current(&p, &digest_a));

        write_result_manifest(&p, &digest_a).unwrap();
        assert!(
            bgm_result_is_current(&p, &digest_a),
            "参数一致就该是当前产物"
        );
        assert!(
            !bgm_result_is_current(&p, &digest_b),
            "参数不同就不是当前产物"
        );

        // 配音成品变了（改稿/重录后重新拼装）→ 即使参数没变也不算
        std::fs::write(p.join("out/final.wav"), b"final-v2").unwrap();
        assert!(
            !bgm_result_is_current(&p, &digest_a),
            "配音成品变了要判过期"
        );

        // 独立生成（没有配音成品）：只比参数
        let only_bgm = root.join("只有BGM");
        std::fs::create_dir_all(only_bgm.join("bgm")).unwrap();
        std::fs::write(only_bgm.join("bgm/bgm.wav"), b"bgm").unwrap();
        write_result_manifest(&only_bgm, &digest_a).unwrap();
        assert!(bgm_result_is_current(&only_bgm, &digest_a));
        assert!(!bgm_result_is_current(&only_bgm, &digest_b));
    }

    /// 参数摘要不一致时，BGM 轨按过期处理（端到端：digest 变了就导不出去）。
    #[test]
    fn digest_mismatch_marks_bgm_stale() {
        let root = temp_dir("digest-mismatch");
        make_mixed_project(&root, "甲");
        let p = root.join("甲");
        let ctx_other = BgmContext {
            current: true,
            options_digest: bgm_options_digest("换了个描述", 0.22),
        };
        match stem_state(&p, Stem::Bgm, &ctx_other) {
            StemState::Stale(reason) => assert!(reason.contains("重新混音"), "{reason}"),
            other => panic!("参数变了就该判过期：{other:?}"),
        }
        assert_eq!(
            stem_state(&p, Stem::Voice, &ctx_other),
            StemState::Ready(p.join("out/final.wav")),
            "人声轨不受 BGM 参数影响"
        );
    }

    /// 人声轨**永远**是当前的配音成品（`out/final.wav`）：混音时那份 `out/voice.wav`
    /// 只是它的副本，副本可能是旧的——优先用副本会导出旧人声（复核指出）。
    #[test]
    fn voice_stem_is_always_the_current_dub_product() {
        let root = temp_dir("stem-src");
        make_mixed_project(&root, "甲");
        let p = root.join("甲");
        write_result_manifest(&p, TEST_DIGEST).unwrap();
        assert_eq!(
            stem_state(&p, Stem::Voice, &ctx_current()),
            StemState::Ready(p.join("out/final.wav")),
            "别用混音时那份可能过期的 voice.wav 副本"
        );
        assert_eq!(
            stem_state(&p, Stem::Bgm, &ctx_current()),
            StemState::Ready(p.join("bgm/bgm.wav"))
        );
        assert_eq!(
            stem_state(&p, Stem::Mixed, &ctx_current()),
            StemState::Ready(p.join("out/mixed.wav"))
        );

        // 老工程（没混过音）：只有 final.wav 时人声照样能导
        let old = root.join("老工程");
        std::fs::create_dir_all(old.join("out")).unwrap();
        std::fs::write(old.join("out/final.wav"), b"final").unwrap();
        assert_eq!(
            stem_state(&old, Stem::Voice, &ctx_current()),
            StemState::Ready(old.join("out/final.wav"))
        );
        assert_eq!(
            stem_state(&old, Stem::Bgm, &ctx_current()),
            StemState::Missing
        );
        assert_eq!(
            stem_state(&old, Stem::Mixed, &ctx_current()),
            StemState::Missing
        );
    }

    /// 独立生成的 BGM（没有配音成品）：不比对配音指纹，但**参数摘要还是要对**——
    /// 没有清单（老工程）只能算"判不出配套"，按过期处理。
    #[test]
    fn standalone_bgm_is_current_once_the_manifest_matches() {
        let root = temp_dir("stem-standalone");
        let dir = root.join("只有BGM");
        std::fs::create_dir_all(dir.join("bgm")).unwrap();
        std::fs::write(dir.join("bgm/bgm.wav"), b"bgm").unwrap();

        assert!(
            matches!(
                stem_state(&dir, Stem::Bgm, &ctx_current()),
                StemState::Stale(_)
            ),
            "没有清单 = 判不出配套，只能算过期"
        );
        write_result_manifest(&dir, TEST_DIGEST).unwrap();
        assert_eq!(
            stem_state(&dir, Stem::Bgm, &ctx_current()),
            StemState::Ready(dir.join("bgm/bgm.wav"))
        );
        let other = BgmContext {
            current: true,
            options_digest: bgm_options_digest("另一个描述", 0.22),
        };
        assert!(
            matches!(stem_state(&dir, Stem::Bgm, &other), StemState::Stale(_)),
            "参数变了就不算当前"
        );
    }

    /// **旧混音不许当当前成品导出**：改稿/重录后没重新混音时，磁盘上的
    /// `mixed.wav` / `bgm.wav` 与当前的 `final.wav` 不是一次出来的（复核指出）。
    #[test]
    fn stale_mix_is_not_exported_as_current() {
        let root = temp_dir("stem-stale");
        make_mixed_project(&root, "甲");
        let p = root.join("甲");
        write_result_manifest(&p, TEST_DIGEST).unwrap();
        assert!(
            bgm_result_is_current(&p, TEST_DIGEST),
            "刚写完清单时应该是当前版本"
        );

        // 重新拼装（配音成品变了），但没重新混音
        std::fs::write(p.join("out/final.wav"), b"final-v2").unwrap();
        assert!(
            !bgm_result_is_current(&p, TEST_DIGEST),
            "配音成品变了就不再是当前混音"
        );
        match stem_state(&p, Stem::Mixed, &ctx_current()) {
            StemState::Stale(reason) => {
                assert!(reason.contains("重新混音"), "要给下一步：{reason}");
            }
            other => panic!("旧混音要判过期：{other:?}"),
        }

        // 人声轨仍是当前的（它就是 final.wav）
        assert_eq!(
            stem_state(&p, Stem::Voice, &ctx_current()),
            StemState::Ready(p.join("out/final.wav"))
        );

        // 一次导出：人声照常出去，混音轨那半边缺；全过期的情况另测
        let dst = temp_dir("stem-stale-dst");
        match export_stems("甲", &p, &dst, &ctx_current()) {
            StemExportOutcome::Done(s) => {
                assert_eq!(s.written.len(), 1, "只有人声能导");
                assert!(s.missing.is_empty(), "{:?}", s.missing);
                assert_eq!(s.stale.len(), 1, "{:?}", s.stale);
                let text = stem_summary_text(&s, &dst);
                assert!(text.contains("重新混音"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read(dst.join("甲_voice.wav")).unwrap(),
            b"final-v2"
        );
        assert!(!dst.join("甲_bgm.wav").exists(), "过期 BGM 不该被导出去");
    }

    /// UI 说"这轮 BGM 结果已作废"（改稿 / 改 BGM 描述后 UI 会清掉当前结果标志）：
    /// BGM 轨按过期处理，人声照常导出——磁盘判断不了描述改没改，这一层只能由调用方给。
    #[test]
    fn invalidated_bgm_result_is_treated_as_stale() {
        let root = temp_dir("stem-bgm-invalid");
        make_mixed_project(&root, "甲");
        let p = root.join("甲");
        assert!(bgm_result_is_current(&p, TEST_DIGEST), "指纹与参数都还配套");

        match stem_state(&p, Stem::Bgm, &ctx_stale()) {
            StemState::Stale(reason) => {
                assert!(
                    reason.contains("作废") || reason.contains("重新生成"),
                    "{reason}"
                );
            }
            other => panic!("UI 说作废了就不该 Ready：{other:?}"),
        }

        let dst = temp_dir("stem-bgm-invalid-dst");
        match export_stems("甲", &p, &dst, &ctx_stale()) {
            StemExportOutcome::Done(s) => {
                assert_eq!(s.written.len(), 1, "人声照导");
                assert_eq!(s.stale.len(), 1, "{:?}", s.stale);
            }
            other => panic!("{other:?}"),
        }
        assert!(dst.join("甲_voice.wav").is_file());
        assert!(!dst.join("甲_bgm.wav").exists());
    }

    /// 一轨都导不出、而且原因是"过期"而不是"没有"：返回 Stale，不建目录。
    /// （例：独立生成的 BGM，用户改了描述 → 这一轨作废，又没有配音成品可退。）
    #[test]
    fn all_stale_stems_report_stale_and_do_not_create_dir() {
        let root = temp_dir("stem-all-stale");
        let dir = root.join("只有BGM");
        std::fs::create_dir_all(dir.join("bgm")).unwrap();
        std::fs::write(dir.join("bgm/bgm.wav"), b"bgm").unwrap();
        let dst = root.join("不该建");
        match export_stems("只有BGM", &dir, &dst, &ctx_stale()) {
            StemExportOutcome::Stale(reasons) => {
                assert_eq!(reasons.len(), 1, "{reasons:?}");
                assert!(reasons[0].contains("重新生成"), "{reasons:?}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!dst.exists(), "全过期就别建导出目录");
    }

    /// 分轨导出写出的名字必须与 BGM Tab 逐轨导出**完全一致**（同一轨一套命名）。
    #[test]
    fn stem_export_uses_the_legacy_suffixes() {
        let root = temp_dir("stem-name");
        make_mixed_project(&root, "甲");
        let dst = temp_dir("stem-name-dst");
        match export_stems("甲", &root.join("甲"), &dst, &ctx_current()) {
            StemExportOutcome::Done(s) => {
                assert_eq!(s.written.len(), 2);
                assert!(s.missing.is_empty(), "{:?}", s.missing);
                assert!(s.stale.is_empty(), "{:?}", s.stale);
            }
            other => panic!("{other:?}"),
        }
        // 人声轨现在取 out/final.wav（voice.wav 只是它的副本）
        assert_eq!(std::fs::read(dst.join("甲_voice.wav")).unwrap(), b"final");
        assert_eq!(std::fs::read(dst.join("甲_bgm.wav")).unwrap(), b"bgm");

        // 单轨导出走同一套命名（BGM Tab 的按钮就是它）
        let one = temp_dir("stem-name-one");
        assert!(matches!(
            export_stem("甲", &root.join("甲"), &one, Stem::Mixed, &ctx_current()),
            StemExportOutcome::Done(_)
        ));
        assert!(one.join("甲_mixed.wav").is_file());
    }

    /// 缺 BGM 不算整次失败：人声照常导出，`missing` 里说清缺哪一轨。
    #[test]
    fn export_stems_reports_missing_bgm_without_failing_voice() {
        let root = temp_dir("stem-missing-bgm");
        let dir = root.join("只有人声");
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/final.wav"), b"final-as-voice").unwrap();
        let dst = temp_dir("stem-missing-dst");

        match export_stems("只有人声", &dir, &dst, &ctx_current()) {
            StemExportOutcome::Done(s) => {
                assert_eq!(s.written.len(), 1, "人声要导出来");
                assert_eq!(s.missing, vec!["BGM".to_string()]);
                let text = stem_summary_text(&s, &dst);
                assert!(text.contains("只有人声_voice.wav"), "{text}");
                assert!(text.contains("还没有 BGM 轨"), "要说清缺什么：{text}");
                assert!(text.contains("先在 BGM 页生成"), "要给对下一步：{text}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read(dst.join("只有人声_voice.wav")).unwrap(),
            b"final-as-voice"
        );
        assert!(!dst.join("只有人声_bgm.wav").exists());
    }

    /// 只有 BGM、没有配音成品（本机 `projects/示例工程 · 频道口播` 就是这种）：
    /// BGM 照常导出，缺人声的提示要指到配音页——套用"先在 BGM 页生成"是指错路。
    #[test]
    fn export_stems_only_bgm_hints_at_dubbing_not_bgm_page() {
        let root = temp_dir("stem-bgm-only");
        let dir = root.join("只有BGM");
        std::fs::create_dir_all(dir.join("bgm")).unwrap();
        std::fs::write(dir.join("bgm/bgm.wav"), b"bgm-only").unwrap();
        // 真生成过就有清单（参数一致 → BGM 轨可导；人声轨仍然缺）
        write_result_manifest(&dir, TEST_DIGEST).unwrap();
        let dst = temp_dir("stem-bgm-only-dst");

        match export_stems("只有BGM", &dir, &dst, &ctx_current()) {
            StemExportOutcome::Done(s) => {
                assert_eq!(s.written.len(), 1);
                assert_eq!(s.missing, vec!["人声".to_string()]);
                let text = stem_summary_text(&s, &dst);
                assert!(text.contains("只有BGM_bgm.wav"), "{text}");
                assert!(text.contains("还没有人声轨"), "{text}");
                assert!(
                    text.contains("先完成配音合成"),
                    "缺人声要指向配音页，不是 BGM 页：{text}"
                );
                assert!(!text.contains("先在 BGM 页生成"), "别给错的下一步：{text}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read(dst.join("只有BGM_bgm.wav")).unwrap(),
            b"bgm-only"
        );
    }

    /// 一轨都没有（连配音成品都没有）：明确说"没有可导出的轨"，且不建导出目录。
    #[test]
    fn export_stems_with_nothing_to_export_does_not_create_dir() {
        let root = temp_dir("stem-empty");
        let dir = root.join("空工程");
        std::fs::create_dir_all(&dir).unwrap();
        let dst = root.join("不该建");
        assert_eq!(
            export_stems("空工程", &dir, &dst, &ctx_current()),
            StemExportOutcome::NothingToExport
        );
        assert!(!dst.exists(), "没有可导的轨就别建目录");
    }

    #[test]
    fn export_all_needs_at_least_one_format_and_does_not_create_dir() {
        let root = temp_dir("none");
        let dst = root.join("不该被创建");
        assert_eq!(
            export_all(&root, &dst, false, false),
            BatchExportOutcome::NoneSelected
        );
        assert!(!dst.exists(), "没选格式不该建目录");
    }

    /// 重复导出：覆盖旧文件，且不留下 .tmp 残渣（copy_atomic 的那条不变量）。
    #[test]
    fn export_all_overwrites_and_leaves_no_temp_files() {
        let root = temp_dir("overwrite");
        let dst = temp_dir("overwrite-dst");
        make_project(&root, "重导", true);
        assert!(matches!(
            export_all(&root, &dst, true, true),
            BatchExportOutcome::Done(_)
        ));
        // 换掉源内容再导一次
        std::fs::write(root.join("重导/out/final.wav"), b"wav-new").unwrap();
        assert!(matches!(
            export_all(&root, &dst, true, true),
            BatchExportOutcome::Done(_)
        ));
        assert_eq!(std::fs::read(dst.join("重导.wav")).unwrap(), b"wav-new");
        let leftovers: Vec<String> = std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留临时文件：{leftovers:?}");
    }

    #[test]
    fn export_one_reports_missing_srt_actionably() {
        let dir = temp_dir("one");
        let wav = dir.join("final.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let dst = dir.join("dst");
        match export_one("工程", &dst, &wav, None, true, true) {
            ExportOutcome::Failed(e) => {
                assert!(e.contains("字幕"), "{e}");
                assert!(e.contains("final.srt"), "要说清缺的是哪个文件：{e}");
            }
            other => panic!("缺字幕时要报失败：{other:?}"),
        }
        // WAV 已经写出去了：这条不变量要说清楚（不能假装什么都没发生）
        assert!(dst.join("工程.wav").is_file());
    }

    /// 只勾 SRT（没勾 WAV）时缺字幕：一个文件都没写，文案就不能说"只导出了 WAV"。
    #[test]
    fn export_one_missing_srt_does_not_claim_wav_was_exported_when_it_was_not() {
        let dir = temp_dir("one-srt-only");
        let wav = dir.join("final.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let dst = dir.join("dst");
        match export_one("工程", &dst, &wav, None, false, true) {
            ExportOutcome::Failed(e) => {
                assert!(e.contains("没有字幕文件"), "{e}");
                assert!(
                    !e.contains("只导出了 WAV"),
                    "没勾 WAV 就不该说导出了 WAV：{e}"
                );
                assert!(e.contains("什么都没导出"), "要如实说清：{e}");
            }
            other => panic!("缺字幕时要报失败：{other:?}"),
        }
        assert!(!dst.join("工程.wav").exists(), "没勾 WAV 不该写出 WAV");
        assert!(!dst.join("工程.srt").exists());
    }

    #[test]
    fn summary_text_says_zero_and_names_first_failure() {
        let dir = PathBuf::from("/tmp/out");
        let empty = BatchExportSummary::default();
        assert!(summary_text(&empty, &dir).contains("没有找到有成品"));
        let s = BatchExportSummary {
            total: 3,
            exported: 2,
            failures: vec!["B：没有字幕文件".into()],
        };
        let text = summary_text(&s, &dir);
        assert!(text.contains("2 篇"), "{text}");
        assert!(text.contains("失败 1 篇"), "{text}");
        assert!(text.contains("没有字幕文件"), "第一条原因要能看见：{text}");
    }
}
