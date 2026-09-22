//! 随包分发的模型下载清单（`config/model-downloads.json`）。
//!
//! ## 为什么要有它
//! P7 的下载队列（串行 / 断点续传 / 校验后提交）本身是好的，但**下载入口只在服务清单
//! 的模型带 `url` 时才出现**，而 `server.json` 是引擎写的，随包清单里的模型一个 `url` 都没有——
//! 于是真机上「下载」按钮一个都不出现：机制有了、没有数据源。
//!
//! 数据源是上游 audio.cpp 的 `model_specs/*.json`（CHARTER §6 认定的 source of truth）。
//! 那个仓库不在用户机器上，所以由 `tools/gen_model_downloads.py` 把它投影成
//! `config/model-downloads.json`（提交进仓库、`include_str!` 打进二进制）：
//!
//! - **映射按落点，不按 family**：`audio8_tts` 这一个 family 同时管 0.1B 与 0.6B，
//!   `index_tts2` 同时管 2.0 / 2.5，`stable_audio` 同时管 medium / small —— 只按 family
//!   会把这些指向**别的权重**（生成脚本头部有实测对照表）。这里用的是产品自己的
//!   `${models_root}/...` 落点与包 `target_directory + strip_prefix` 的精确匹配。
//! - **下不了就如实说**：`status == "no-source"` 的模型只显示原因，`action = None`
//!   让按钮不可点——不给一个点了必然失败的入口（gated / 上游没有该权重的包 / 没有 spec）。
//!
//! ## 优先级
//! 服务清单里**显式给了 `url` 的以服务为准**（服务侧可以覆盖内置清单，例如内网镜像）；
//! 服务没给才查内置清单。
//!
//! ## 档位与「本机推荐」
//! 内置清单里每个 family 都带全部可下载包（`packages[]`）。本模块把**与入口同一个目录**
//! 的单文件包当作"这个模型的档位"（同一变体的 q8_0 / f16 / orig …），并按**服务的内存守卫
//! 口径**给出"本机推荐哪一档"：调档只看下载体积会把运行时那部分漏掉（`qwen3-asr` 0.6B
//! q8_0 权重 1.07 GiB、服务估的是 3.31 GiB），公式与 X 的理由见 `footprint_estimate` 与
//! `BUDGET_PERCENT_OF_PHYSICAL` 的注释。**只列 + 推荐 + 说明**：不自动改服务清单、
//! 不自动替用户下推荐档。
//!
//! ## 落点
//! 目标一律是 `<模型目录>/<相对落点>`，相对落点优先取内置清单的 `local_paths[0]`
//! （那是上游 `target_directory + strip_prefix` 的布局，与 `server.json` 的 `path`
//! 同源）。若算出来的落点与 `server.json` 声明的 `path` **不是同一处**，`conflict`
//! 里带上两边——否则用户下完 2 GB，服务照样加载不了，还看不出为什么。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 打进二进制的内置清单（由 `tools/gen_model_downloads.py` 生成，别手改）。
const CATALOG_JSON: &str = include_str!("../config/model-downloads.json");

/// 结构体只声明**应用真的会读**的字段（serde 默认忽略其余）——JSON 里那份更全的契约
/// （family / precision / target_directory / repo / revision / packages[]）由生成脚本
/// 与 `docs/model-download.md` 负责，这里不重复声明一遍没人用的字段。
#[derive(Debug, serde::Deserialize)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CatalogModel {
    pub id: String,
    /// 来源 spec 文件名（`model_specs/<spec>`），查源用。
    #[serde(default)]
    pub spec: String,
    /// `downloadable` | `no-source`
    pub status: String,
    /// 人话说明（为什么没有下载源 / 为什么选中这个量化档）。
    #[serde(default)]
    pub note: String,
    /// 选中的那条包（`status == "downloadable"` 时必有）。
    #[serde(default)]
    pub entry: Option<Package>,
    /// 这个 family 的全部包 —— 界面从这里筛出"与入口同一个目录"的档位。
    #[serde(default)]
    pub packages: Vec<Package>,
    /// `session_options` 里辅助权重的体积合计（服务估算占用时要一起算）。
    #[serde(default)]
    pub aux_bytes: u64,
    /// 辅助权重里**没能核实体积**的键名（非空 = 估算偏低，界面必须如实说）。
    #[serde(default)]
    pub aux_unresolved: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Package {
    /// 上游包 id（判断"哪一档是下载按钮会下的那条"用）。
    #[serde(default)]
    pub id: String,
    /// 档位标签：`q8_0` / `f16` / `orig` / `native`。
    #[serde(default)]
    pub precision: String,
    /// 权重格式：`gguf` / `safetensors`（没有 precision 时的兜底标签）。
    #[serde(default)]
    pub format: String,
    /// 整包体积（生成时 HTTP HEAD 取到；**任一文件未知就是 `None`**，不许拿已知项推算）。
    #[serde(default)]
    pub bytes: Option<u64>,
    /// 这个包是不是真的能下（认识 `kind` + 有 repo + 非 gated + 每个文件都有 URL）。
    #[serde(default)]
    pub downloadable: bool,
    /// 相对「模型目录」的落点（上游 `target_directory` + `strip_prefix` 之后的文件路径）。
    #[serde(default)]
    pub local_paths: Vec<String>,
    /// 上游标了「需要 HuggingFace 授权」。**这里也拦一道**：清单里万一标成 gated
    /// 却仍进了 downloadable，应用也不许把那个 URL 交出去（点开必然 401）。
    #[serde(default)]
    pub gated: bool,
    #[serde(default)]
    pub files: Vec<PackageFile>,
}

/// 文件级 `sha256` 也在清单里，应用真的会读它（内置清单侧的校验兜底，见 `action_for`）。
/// 本模块的约定仍是只声明"应用真的会读"的字段（serde 忽略其余）；`bytes` / `remote_path`
/// 等是生成脚本离线重生成时自己要沿用的，应用不读。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PackageFile {
    pub url: Option<String>,
    /// 上游 LFS 真 sha256（生成脚本 `--fetch-hashes` 从 HF tree API 的 `lfs.oid` 取到；
    /// 取不到是 null）。**只描述这个 `url` 指向的那份文件**。
    #[serde(default)]
    pub sha256: Option<String>,
}

/// 内置清单。`Err` = **内置文件本身坏了**（读不出来），不是"这个模型没有下载源"——
/// 两种情况界面必须说不同的话，所以这里是 `Result` 而不是 `Option`。
pub fn catalog() -> Result<&'static Catalog, &'static str> {
    static CACHE: OnceLock<Result<Catalog, String>> = OnceLock::new();
    match CACHE
        .get_or_init(|| serde_json::from_str::<Catalog>(CATALOG_JSON).map_err(|e| e.to_string()))
    {
        Ok(c) => Ok(c),
        Err(e) => Err(e.as_str()),
    }
}

// ===========================================================================
// 档位体积 + 「本机推荐哪一档」
// ===========================================================================

/// 服务内存守卫的运行时开销 —— 抄自上游 `app/server/model_memory.cpp` 的
/// `estimate_model_memory_bytes()`：
///
/// ```text
/// estimate = weights × 1.5 + 128 MiB
/// ```
///
/// **抄同一把尺**是因为"推荐哪一档装得下"问的就是服务那句话：只看下载体积会把运行时的
/// KV / 激活图 / GPU 缓冲全漏掉（实测 `qwen3-asr` 0.6B q8_0 权重 1.07 GiB、服务估的是
/// 3.31 GiB，差 3 倍）。跨进程复制一份公式就有漂移风险，所以用三个**真机 503 原文**里的
/// 数字钉住它（`footprint_estimate_reproduces_the_three_real_503_numbers`）——上游改了系数
/// 这里不会自动知道，所以界面上说的始终是"估算"，最终能不能加载由服务的守卫决定。
const RUNTIME_OVERHEAD_PERCENT: u64 = 50;
const FIXED_OVERHEAD_BYTES: u64 = 128 * 1024 * 1024;

/// `server.json` 没写 `min_free_memory_mb` 时的余量（schema 的默认就是 1024 MiB）。
pub const DEFAULT_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;

/// 推荐时只把物理内存的**这一半**算作"这一档可以花的预算"。
///
/// 取 50% 的理由：服务的守卫比的是**当时可用**内存（macOS 上 free+inactive+purgeable），
/// 而这里只知道**物理**内存总量；可用永远小于物理，而且在真机上差得很远 —— 同一台 16 GiB
/// 的机器上，并行编译时服务只拿到 1.0–3.9 GiB 可用（物理的 6%–24%）。所以推荐只能是
/// "这台机器适合哪一档"的**规划口径**：一半留给系统、桌面、引擎常驻的其它模型与文件缓存。
/// 它是**天花板不是承诺**：真正能不能加载由服务守卫决定，文案里不许写"保证装得下"。
pub const BUDGET_PERCENT_OF_PHYSICAL: u64 = 50;

/// 服务口径的加载占用估算（`weights × 1.5 + 128 MiB`；整数运算与上游的
/// `floor(weights * 1.5)` 等价）。
pub fn footprint_estimate(weights_bytes: u64) -> u64 {
    weights_bytes + weights_bytes * RUNTIME_OVERHEAD_PERCENT / 100 + FIXED_OVERHEAD_BYTES
}

/// 体积文案：`GiB` / `MiB`。与服务 503 里报的 `GiB` 同一口径，用户可以逐字对照；
/// 进度行的 `short_bytes` 只求短，是另一回事。
pub fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else {
        format!("{:.0} MiB", value / MIB)
    }
}

/// 推荐要用的"本机尺度"。两个输入都来自真实来源（物理内存来自平台探测、`headroom`
/// 来自 `server.json` 的 `min_free_memory_mb`），做成结构体是为了在单测里**注入** ——
/// 一台机器只有一个物理内存值，不注入就写不出能红的用例。
#[derive(Debug, Clone, Copy)]
pub struct MachineBudget {
    pub physical_memory: Option<u64>,
    pub headroom_bytes: u64,
}

impl MachineBudget {
    /// 物理内存走平台探测；`headroom_bytes` 由调用方从服务清单取。
    pub fn detect(headroom_bytes: u64) -> Self {
        Self {
            physical_memory: physical_memory_bytes(),
            headroom_bytes,
        }
    }

    /// 这一档可以花的预算（物理内存的一半；拿不到物理内存时 `None`）。
    fn per_model_budget(&self) -> Option<u64> {
        self.physical_memory
            .map(|total| total / 100 * BUDGET_PERCENT_OF_PHYSICAL)
    }
}

/// 同落点目录里的一个可下载档位（同一变体的 q8_0 / f16 / orig …）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub precision: String,
    pub format: String,
    /// 下载体积（HTTP HEAD 取到的原始字节）；清单里没取到 → `None`
    pub download_bytes: Option<u64>,
    /// 服务口径的加载占用 = `(下载体积 + 辅助权重) × 1.5 + 128 MiB`
    pub footprint_bytes: Option<u64>,
    /// 是不是「下载」按钮实际会下的那一档（与清单 `path` 对应的那一条）
    pub is_entry: bool,
}

/// 「本机推荐哪一档」+ 理由。`tier = None` = 没有可推荐的档（只有一档 / 体积未知 /
/// 内存未知 / 全都装不下），此时 `note` 必须说清是哪种。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advice {
    pub tier: Option<String>,
    pub note: String,
}

/// 档位在界面上的短标签：优先 `precision`（q8_0 / f16 / orig），没有就用 `format`。
fn label_of(tier: &Tier) -> String {
    if !tier.precision.trim().is_empty() {
        tier.precision.clone()
    } else if !tier.format.trim().is_empty() {
        tier.format.clone()
    } else {
        "（未标精度）".to_string()
    }
}

/// `(档位, 本机尺度) → 推荐` 的**唯一判据**（纯函数）。
///
/// 规则：`占用估算 + 服务的余量 ≤ 物理内存的一半` 里**最大的那一档**。
/// - 为什么取最大的：档位越大质量越好，装得下就没理由让用户退而求其次；
///   "越小越稳"会让 64 GiB 的机器也去下最小档，白白浪费机器。
/// - 为什么用**物理**内存而不是当时可用内存：可用每秒钟都在变（同一台机器实测
///   1.0→3.9 GiB），一个会跳的推荐等于没推荐；物理内存是稳定的机器属性。
pub fn recommend_tier(tiers: &[Tier], budget: &MachineBudget) -> Advice {
    if tiers.is_empty() {
        return Advice::default();
    }
    let Some(per_model) = budget.per_model_budget() else {
        return Advice {
            tier: None,
            note: "拿不到本机物理内存，不猜推荐".into(),
        };
    };
    let physical = budget.physical_memory.unwrap_or_default();
    let known: Vec<&Tier> = tiers
        .iter()
        .filter(|tier| tier.footprint_bytes.is_some())
        .collect();
    let unknown = tiers.len() - known.len();
    let with_caveat = |mut note: String| {
        if unknown > 0 {
            note.push_str(&format!("（另有 {unknown} 档体积未知，未参与比较）"));
        }
        note
    };
    if known.is_empty() {
        return Advice {
            tier: None,
            note: "档位体积未知（生成清单时没取到），无法判断哪一档装得下".into(),
        };
    }
    let required = |tier: &Tier| tier.footprint_bytes.unwrap_or_default() + budget.headroom_bytes;
    if tiers.len() == 1 {
        let only = &tiers[0];
        let fits = required(only) <= per_model;
        let (sign, verdict) = if fits {
            ("≤", "装得下")
        } else {
            (">", "装不下")
        };
        return Advice {
            tier: None,
            note: format!(
                "只有一档 {}：估算占用 {} + 余量 {} {} 物理内存 {} 的一半 {} —— {verdict}",
                label_of(only),
                human_bytes(only.footprint_bytes.unwrap_or_default()),
                human_bytes(budget.headroom_bytes),
                sign,
                human_bytes(physical),
                human_bytes(per_model),
            ),
        };
    }
    let mut fits: Vec<&Tier> = known
        .iter()
        .copied()
        .filter(|tier| required(tier) <= per_model)
        .collect();
    fits.sort_by_key(|tier| {
        (
            tier.footprint_bytes.unwrap_or_default(),
            tier.precision.clone(),
        )
    });
    match fits.last() {
        Some(best) => {
            let mut note = format!(
                "本机推荐 {}：估算占用 {} + 余量 {} ≤ 物理内存 {} 的一半 {}",
                label_of(best),
                human_bytes(best.footprint_bytes.unwrap_or_default()),
                human_bytes(budget.headroom_bytes),
                human_bytes(physical),
                human_bytes(per_model),
            );
            if !best.is_entry {
                if let Some(entry) = tiers.iter().find(|tier| tier.is_entry) {
                    note.push_str(&format!(
                        "；下载按钮取的是 {}（与清单 path 对应的那一档）",
                        label_of(entry)
                    ));
                }
            }
            Advice {
                tier: Some(best.precision.clone()),
                note: with_caveat(note),
            }
        }
        None => {
            let smallest = known
                .iter()
                .copied()
                .min_by_key(|tier| tier.footprint_bytes.unwrap_or_default())
                .unwrap_or(&tiers[0]);
            Advice {
                tier: None,
                note: with_caveat(format!(
                    "本机装不下任何一档：最小档 {} 要估算占用 {} + 余量 {} = {} > 物理内存 {} 的一半 {}",
                    label_of(smallest),
                    human_bytes(smallest.footprint_bytes.unwrap_or_default()),
                    human_bytes(budget.headroom_bytes),
                    human_bytes(required(smallest)),
                    human_bytes(physical),
                    human_bytes(per_model),
                )),
            }
        }
    }
}

/// 一个模型的档位 = **与入口同一个目录**里的可下载单文件包。
///
/// 为什么不按 `target_directory` 或 family 分组：
/// - `target_directory` 太粗：`ace-step` 的 turbo / base / xl 是**不同变体**，都挂在
///   `ACE-Step1.5-GGUF` 下，混在一起会把"另一个模型"当成"另一档"；
/// - family 更粗：`qwen3_asr` 同时管 0.6B 与 1.7B，`index_tts2` 同时管 2.0 与 2.5。
///
/// 只收**单文件**包：多文件包（safetensors 等）下载器还不支持，列出来等于给一个点不了的入口。
fn tiers_of(model: &CatalogModel) -> Vec<Tier> {
    let Some(entry) = model.entry.as_ref() else {
        return Vec::new();
    };
    let Some(entry_path) = entry.local_paths.first() else {
        return Vec::new();
    };
    let dir = parent_dir(entry_path);
    let aux = model.aux_bytes;
    let mut tiers: Vec<Tier> = model
        .packages
        .iter()
        .filter(|package| {
            package.downloadable
                && package.files.len() == 1
                && package
                    .local_paths
                    .first()
                    .is_some_and(|rel| parent_dir(rel) == dir)
        })
        .map(|package| Tier {
            precision: package.precision.clone(),
            format: package.format.clone(),
            download_bytes: package.bytes,
            footprint_bytes: package
                .bytes
                .map(|weights| footprint_estimate(weights + aux)),
            is_entry: package.id == entry.id,
        })
        .collect();
    // 稳定顺序：体积升序（未知排最后），同体积按 precision —— 界面顺序必须可复现
    tiers.sort_by_key(|tier| {
        (
            tier.footprint_bytes.unwrap_or(u64::MAX),
            tier.precision.clone(),
        )
    });
    tiers
}

/// 相对落点里"文件所在的那一层目录"（清单里的路径一律 `/` 分隔，与平台无关）。
fn parent_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// 一个模型的档位 + 本机推荐（`plan_rows` 之外单独取：推荐要看**同目录**的其它档）。
///
/// `catalog` 传 `Result` 而不是 `Option`：内置清单**读不出来**是"说不清"，与"这个模型
/// 没有档位"是两回事（前者界面要说清，后者不显示）。
pub fn tiers_and_advice(
    catalog: Result<&Catalog, &str>,
    model_id: &str,
    budget: &MachineBudget,
) -> (Vec<Tier>, Advice) {
    let model = catalog
        .ok()
        .and_then(|catalog| catalog.models.iter().find(|m| m.id == model_id));
    let Some(model) = model else {
        return (Vec::new(), Advice::default());
    };
    let tiers = tiers_of(model);
    let mut advice = recommend_tier(&tiers, budget);
    if !model.aux_unresolved.is_empty() && !advice.note.is_empty() {
        advice.note.push_str(&format!(
            "（另有 {} 项辅助权重没能核实体积，估算偏低）",
            model.aux_unresolved.len()
        ));
    }
    (tiers, advice)
}

// ---------------------------------------------------------------------------
// 本机物理内存
// ---------------------------------------------------------------------------

/// 本机物理内存（字节）。拿不到就是 `None` —— 推荐必须能区分"内存未知"，
/// 不许拿 0 兜底（0 会让每一档都判成装不下，等于凭空编一个结论）。
///
/// **进程内只探一次**：Windows 的探测要起一个 PowerShell（WMI 查询，冷启动几百毫秒到
/// 数秒），而调用方（下载面板每次重建行都会问一遍）跟着下载进度被反复调到 —— 2026-09-19
/// 真机反馈的"下载模型界面卡死"正是这条：每条 64 KiB 的进度快照都去 spawn 一个
/// powershell.exe，界面永远回不到事件循环。物理内存是机器属性，问一次就够；缓存放这里
/// 而不是调用方，以后多个调用点也不会各自探一遍。
pub fn physical_memory_bytes() -> Option<u64> {
    static CACHE: OnceLock<Option<u64>> = OnceLock::new();
    physical_memory_once(&CACHE, &platform_memory_probe)
}

/// 带缓存的探测：`probe` 只会被调用一次（`OnceLock` 保证），拿到的（哪怕是失败）就是答案。
///
/// 拆出来是为了可测：`probe` 计数即可证明"没有反复探测"（平台探测本身要用真命令，
/// 单测里换不了）。
fn physical_memory_once(
    cache: &OnceLock<Option<u64>>,
    probe: &dyn Fn() -> Option<String>,
) -> Option<u64> {
    *cache.get_or_init(|| physical_memory_with(probe))
}

/// 探测缝：`probe` 给平台的原始输出，解析是纯函数（三种格式在任一平台上都可测）。
fn physical_memory_with(probe: &dyn Fn() -> Option<String>) -> Option<u64> {
    parse_physical_memory(&probe()?)
}

/// 解析平台探测输出里的物理内存字节数。三种格式都认：
///
/// - macOS `sysctl -n hw.memsize` → `17179869184`
/// - Windows PowerShell `(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory` → `17179869184`
/// - Linux `/proc/meminfo` → `MemTotal:       16384000 kB`
pub fn parse_physical_memory(raw: &str) -> Option<u64> {
    for line in raw.lines() {
        // PowerShell 的 stdout 可能带 BOM
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return kib.checked_mul(1024).filter(|total| *total > 0);
        }
        if let Ok(bytes) = line.parse::<u64>() {
            return (bytes > 0).then_some(bytes);
        }
    }
    None
}

/// 平台探测：macOS `sysctl` / Linux `/proc/meminfo` / Windows PowerShell。
///
/// 命令与解析都留在本模块，别处只消费 `physical_memory_bytes()`（"这份机器尺度从哪来"
/// 只有一处，免得回显与实际各算一份）。
#[cfg(target_os = "macos")]
fn platform_memory_probe() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn platform_memory_probe() -> Option<String> {
    std::fs::read_to_string("/proc/meminfo").ok()
}

#[cfg(windows)]
fn platform_memory_probe() -> Option<String> {
    // 不弹控制台：统一走 win_child（`CREATE_NO_WINDOW` 的唯一来源）。
    let out = crate::win_child::hidden_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 其它平台：不猜（`None` → 界面说"拿不到本机物理内存"）。
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform_memory_probe() -> Option<String> {
    None
}

/// 服务清单里一条模型（只取下载相关的字段，不依赖 `main.rs` 的 `ServerModel`）。
#[derive(Debug, Clone, Default)]
pub struct ServerEntry {
    pub id: String,
    /// 服务清单显式给的下载地址（非空即**以服务为准**）。
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    /// 服务清单声明的落点：**服务会去哪里加载**的真相。
    pub path: String,
}

/// 这条下载的信息是从哪来的（界面要如实说，别把内置清单说成服务给的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// 服务清单里显式写了 url（覆盖内置清单）
    Server,
    /// 随包分发的内置清单
    Builtin,
}

impl Origin {
    /// 界面上的来源标签：下载面板每行都要如实说权重是哪来的。
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Server => "来源：server.json（服务清单）",
            Origin::Builtin => "来源：内置下载清单",
        }
    }
}

/// 一条真的能点的下载入口。
#[derive(Debug, Clone)]
pub struct Action {
    pub url: String,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    /// 最终落点：`<模型目录>/<相对落点>`。
    pub dest: PathBuf,
    pub origin: Origin,
    /// 与 `server.json` 的 `path` 对不上时的提醒（`None` = 对得上或清单没声明）。
    pub conflict: Option<String>,
}

/// 下载面板的一行：能下载的（`action = Some`）或没有下载源的（`action = None` + 原因）。
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub action: Option<Action>,
    pub reason: String,
}

/// 把「服务清单 ∪ 内置清单」投影成下载面板的行（纯函数，便于单测）。
///
/// - 服务清单里显式给了 `url` 的**以服务为准**；
/// - 其余查内置清单（`status == "downloadable"` 才有入口，否则带原因但不给按钮）；
/// - 内置清单里有、服务清单里没有的也给一行——否则"能下但没有入口"会静默消失。
pub fn plan_rows(
    server: &[ServerEntry],
    catalog: Result<&Catalog, &str>,
    model_dir: &Path,
) -> Vec<Row> {
    let index: HashMap<&str, &CatalogModel> = match catalog {
        Ok(c) => c.models.iter().map(|m| (m.id.as_str(), m)).collect(),
        Err(_) => HashMap::new(),
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for s in server {
        seen.insert(s.id.as_str());
        let cat = index.get(s.id.as_str()).copied();
        rows.push(match catalog {
            Ok(_) => row_for_server(s, cat, model_dir),
            // 内置清单坏了：服务清单带 url 的照常能用，其余如实说"内置清单读不出来"
            Err(e) => match server_action(s, cat, model_dir) {
                Some(action) => Row {
                    id: s.id.clone(),
                    action: Some(action),
                    reason: String::new(),
                },
                None => Row {
                    id: s.id.clone(),
                    action: None,
                    reason: format!(
                        "内置下载清单读不出来（{e}），服务清单也没给 {} 下载地址——这条暂时没有入口",
                        s.id
                    ),
                },
            },
        });
    }

    if let Ok(c) = catalog {
        for m in &c.models {
            if !seen.contains(m.id.as_str()) {
                rows.push(match usable_builtin_package(Some(m)) {
                    Some(pkg) => {
                        let url = pkg.files[0].url.clone().unwrap_or_default();
                        let dest = model_dir.join(&pkg.local_paths[0]);
                        Row {
                            id: m.id.clone(),
                            // 服务清单里没有这个模型 → 服务侧校验信息没得谈（空条目）；
                            // sha256 由内置清单 per-file 兜底
                            action: Some(action_for(
                                &ServerEntry::default(),
                                url,
                                dest,
                                Origin::Builtin,
                                pkg.files[0].sha256.as_deref(),
                                model_dir,
                            )),
                            reason: String::new(),
                        }
                    }
                    None => no_source_row(m),
                });
            }
        }
    }
    rows
}

fn row_for_server(s: &ServerEntry, cat: Option<&CatalogModel>, model_dir: &Path) -> Row {
    match server_action(s, cat, model_dir) {
        Some(action) => Row {
            id: s.id.clone(),
            action: Some(action),
            reason: String::new(),
        },
        None => Row {
            id: s.id.clone(),
            action: None,
            reason: match cat {
                Some(c) => no_source_reason(c),
                None => format!(
                    "服务清单没给下载地址，内置清单里也没有 {} —— 暂无下载源（不猜地址）",
                    s.id
                ),
            },
        },
    }
}

/// 服务清单这一条的下载入口：服务给了 url 就用服务的，否则退到内置清单。
fn server_action(s: &ServerEntry, cat: Option<&CatalogModel>, model_dir: &Path) -> Option<Action> {
    let url = s.url.trim();
    if !url.is_empty() {
        let rel = builtin_rel(cat).unwrap_or_else(|| declared_rel(&s.path, &s.id, url, model_dir));
        let dest = model_dir.join(rel);
        let builtin_sha = usable_builtin_package(cat).and_then(|p| p.files[0].sha256.as_deref());
        return Some(action_for(
            s,
            url.to_string(),
            dest,
            Origin::Server,
            builtin_sha,
            model_dir,
        ));
    }
    let pkg = usable_builtin_package(cat)?;
    let rel = &pkg.local_paths[0];
    let url = pkg.files[0].url.clone()?;
    let dest = model_dir.join(rel);
    Some(action_for(
        s,
        url,
        dest,
        Origin::Builtin,
        pkg.files[0].sha256.as_deref(),
        model_dir,
    ))
}

/// 组装一条下载入口——**唯一一处**（两个分支共用，免得"服务侧校验信息"在其中一条上漂掉）。
///
/// 口径是**按字段**回落，不是"要么全用服务、要么全用内置"：
/// `url` 由调用方决定（服务清单有就用服务的）；`sha256` 服务清单写了就用服务侧的，没写就
/// 用内置清单 per-file 的（生成脚本 `--fetch-hashes` 从 HF tree API 的 `lfs.oid` 取到，
/// 描述的就是内置清单那条 url 指向的文件）；两者都没有 → `None`（只按大小校验，界面会如实说）。
/// `size` 仍只认服务侧：内置清单的包级体积是**估算口径**，不做校验依据。
fn action_for(
    s: &ServerEntry,
    url: String,
    dest: PathBuf,
    origin: Origin,
    builtin_sha: Option<&str>,
    model_dir: &Path,
) -> Action {
    let server_sha = s.sha256.trim();
    let sha = if !server_sha.is_empty() {
        Some(server_sha.to_string())
    } else {
        builtin_sha
            .map(str::trim)
            .filter(|sha| !sha.is_empty())
            .map(str::to_string)
    };
    Action {
        url,
        sha256: sha,
        size: s.size,
        conflict: conflict_note(&s.path, &dest, model_dir),
        dest,
        origin,
    }
}

/// 内置清单给的相对落点（上游布局，含 `target_directory` 那一层）。
fn builtin_rel(cat: Option<&CatalogModel>) -> Option<String> {
    usable_builtin_package(cat)?.local_paths.first().cloned()
}

/// 内置清单这一条是不是真的能点：`status == downloadable`、未 gated、单文件且带 URL。
///
/// **gated 在这里再拦一道**：清单里万一标成 gated 却仍进了 downloadable，应用也不许把
/// 那个 URL 交出去（HF 上会 401/403，点了必然失败）——与"不给假入口"同一条口径。
fn usable_builtin_package(cat: Option<&CatalogModel>) -> Option<&Package> {
    let pkg = cat
        .filter(|c| c.status == "downloadable")
        .and_then(|c| c.entry.as_ref())?;
    if pkg.gated || pkg.local_paths.len() != 1 || pkg.files.len() != 1 || pkg.files[0].url.is_none()
    {
        return None;
    }
    Some(pkg)
}

/// 服务清单没给可用 url、也没有内置包时的原因（原样把清单里的说明摆出来，便于查源）。
fn no_source_reason(c: &CatalogModel) -> String {
    let mut reason = if c.note.trim().is_empty() {
        format!("内置清单把 {} 标成没有下载源，但没写原因", c.id)
    } else {
        c.note.trim().to_string()
    };
    if !c.spec.is_empty() {
        reason.push_str(&format!("（上游 spec：model_specs/{}）", c.spec));
    }
    reason
}

fn no_source_row(m: &CatalogModel) -> Row {
    Row {
        id: m.id.clone(),
        action: None,
        reason: no_source_reason(m),
    }
}

/// 服务清单没给 url、也不能用内置清单时的兜底落点：
/// 用清单 `path`（相对模型目录的那一段），否则退回文件名，最后 `<id>.gguf`。
fn declared_rel(path: &str, id: &str, url: &str, model_dir: &Path) -> String {
    if let Some(rel) = path_under_model_dir(path, model_dir) {
        return rel;
    }
    let from_path = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty());
    let from_url = url
        .split('?')
        .next()
        .unwrap_or("")
        .split('/')
        .next_back()
        .filter(|s| !s.is_empty());
    from_path
        .or(from_url)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{id}.gguf"))
}

/// `path` 落在模型目录里的话，返回它相对模型目录的那一段（**按真实路径**比，挡 `..` 与软链）。
fn path_under_model_dir(path: &str, model_dir: &Path) -> Option<String> {
    if path.trim().is_empty() {
        return None;
    }
    let declared = crate::paths::resolve_for_compare(Path::new(path), model_dir).ok()?;
    let base = crate::paths::resolve_for_compare(model_dir, model_dir).ok()?;
    let rel = declared.strip_prefix(&base).ok()?;
    let rel = rel.to_string_lossy().to_string();
    (!rel.is_empty()).then_some(rel)
}

/// 落点校验：算出来的下载目标与 `server.json` 声明的 `path` 是不是同一处。
///
/// · `path` 没写 → `None`（没有可比的声明）。
/// · 指向同一个文件，或 `path` 是它的**目录**（gen 类模型 `path` 指目录）→ `None`。
/// · 否则给出两边：用户要知道"下到哪、服务去哪找"，否则下完仍加载不了却看不出原因。
fn conflict_note(path: &str, dest: &Path, model_dir: &Path) -> Option<String> {
    if path.trim().is_empty() {
        return None;
    }
    let declared = crate::paths::resolve_for_compare(Path::new(path), model_dir).ok()?;
    let target = crate::paths::resolve_for_compare(dest, model_dir).ok()?;
    if target == declared || target.starts_with(&declared) {
        return None;
    }
    Some(format!(
        "落点与 server.json 对不上：清单 path 是 {}，本次会下到 {} —— 下完服务仍可能加载不到（把「模型目录」指到清单所在的位置，或把清单 path 改成实际落点）",
        declared.display(),
        target.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, url: &str, path: &str) -> ServerEntry {
        ServerEntry {
            id: id.into(),
            url: url.into(),
            sha256: String::new(),
            size: None,
            path: path.into(),
        }
    }

    fn cat_model(id: &str, status: &str, note: &str, pkg: Option<Package>) -> CatalogModel {
        let packages: Vec<Package> = pkg.iter().cloned().collect();
        CatalogModel {
            id: id.into(),
            spec: "f.json".into(),
            status: status.into(),
            note: note.into(),
            entry: pkg,
            packages,
            aux_bytes: 0,
            aux_unresolved: Vec::new(),
        }
    }

    fn pkg(local: &str, url: &str) -> Package {
        pkg_sha(local, url, None)
    }

    fn pkg_sha(local: &str, url: &str, sha256: Option<&str>) -> Package {
        Package {
            id: format!("pkg-{local}"),
            precision: "q8_0".into(),
            format: "gguf".into(),
            bytes: None,
            downloadable: true,
            local_paths: vec![local.into()],
            gated: false,
            files: vec![PackageFile {
                url: Some(url.into()),
                sha256: sha256.map(str::to_string),
            }],
        }
    }

    /// 造一个"有档位"的模型：`(precision, 体积, 是不是下载按钮那一档)`，同目录。
    fn model_with_tiers(
        dir: &str,
        tiers: &[(&str, Option<u64>, bool)],
        aux_bytes: u64,
    ) -> CatalogModel {
        let packages: Vec<Package> = tiers
            .iter()
            .map(|(precision, bytes, _)| Package {
                id: format!("{dir}::{precision}"),
                precision: (*precision).into(),
                format: "gguf".into(),
                bytes: *bytes,
                downloadable: true,
                local_paths: vec![format!("{dir}/{precision}.gguf")],
                gated: false,
                files: vec![PackageFile {
                    url: Some(format!("https://example.com/{dir}/{precision}.gguf")),
                    sha256: None,
                }],
            })
            .collect();
        let entry = tiers
            .iter()
            .find(|(_, _, is_entry)| *is_entry)
            .or_else(|| tiers.first())
            .and_then(|(precision, _, _)| {
                packages
                    .iter()
                    .find(|package| package.precision == *precision)
                    .cloned()
            });
        CatalogModel {
            id: "m".into(),
            spec: "f.json".into(),
            status: "downloadable".into(),
            note: String::new(),
            entry,
            packages,
            aux_bytes,
            aux_unresolved: Vec::new(),
        }
    }

    fn budget(physical_gib: f64, headroom_gib: f64) -> MachineBudget {
        const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
        MachineBudget {
            physical_memory: Some((physical_gib * GIB) as u64),
            headroom_bytes: (headroom_gib * GIB) as u64,
        }
    }

    /// ④-1：**服务清单带 url 时以服务为准**（内置清单不能盖掉服务侧的内网镜像）。
    #[test]
    fn server_url_wins_over_the_builtin_catalog() {
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg("M-GGUF/m.gguf", "https://builtin.example/m.gguf")),
            )],
        };
        let rows = plan_rows(
            &[entry("m", "https://mirror.example/m.gguf", "")],
            Ok(&catalog),
            Path::new("/models"),
        );
        let action = rows[0].action.as_ref().expect("服务给了 url 就该有入口");
        assert_eq!(action.url, "https://mirror.example/m.gguf");
        assert_eq!(action.origin, Origin::Server);
        // 落点仍取内置清单的上游布局（服务只给了 URL，没给布局）
        assert_eq!(action.dest, Path::new("/models/M-GGUF/m.gguf"));
    }

    /// 服务清单没给 url → 用内置清单的 URL 与上游落点。
    #[test]
    fn builtin_catalog_fills_in_when_the_server_has_no_url() {
        let catalog = Catalog {
            models: vec![cat_model(
                "audio8-tts",
                "downloadable",
                "落点精确匹配",
                Some(pkg(
                    "Audio8-TTS-Preview-0.6B-GGUF/audio8-tts-preview-0.6b-q8_0.gguf",
                    "https://huggingface.co/x/y/resolve/main/z.gguf",
                )),
            )],
        };
        let rows = plan_rows(
            &[entry(
                "audio8-tts",
                "",
                "/models/Audio8-TTS-Preview-0.6B-GGUF/audio8-tts-preview-0.6b-q8_0.gguf",
            )],
            Ok(&catalog),
            Path::new("/models"),
        );
        let action = rows[0].action.as_ref().expect("内置清单有就该有入口");
        assert_eq!(action.origin, Origin::Builtin);
        assert_eq!(action.url, "https://huggingface.co/x/y/resolve/main/z.gguf");
        assert_eq!(
            action.dest,
            Path::new("/models/Audio8-TTS-Preview-0.6B-GGUF/audio8-tts-preview-0.6b-q8_0.gguf")
        );
        assert_eq!(action.conflict, None, "落点与清单 path 一致时不该报冲突");
    }

    /// ④-2：`gated` / 没有下载源 → **不给入口**，只给原因（不许发明一个必然失败的按钮）。
    ///
    /// `sneaky-gated` 刻意标成 `downloadable`、带一个 URL、但 `gated: true`：
    /// 即清单漏标/错标时应用侧也得拦住。阳性对照：删掉 `usable_builtin_package` 里的
    /// `pkg.gated` 判断 → 这条红（会真的把那个 401 的 URL 交出去）。
    #[test]
    fn no_source_and_gated_models_get_no_action_but_a_reason() {
        let mut gated = pkg("Gated-GGUF/g.gguf", "https://huggingface.co/gated/x.gguf");
        gated.gated = true;
        let catalog = Catalog {
            models: vec![
                cat_model(
                    "yue2",
                    "no-source",
                    "上游 model_specs 里没有 family=yue2 的 spec",
                    None,
                ),
                cat_model(
                    "gated-model",
                    "no-source",
                    "该模型需要 HuggingFace 授权下载（gated）",
                    None,
                ),
                cat_model(
                    "sneaky-gated",
                    "downloadable",
                    "该模型需要 HuggingFace 授权下载（gated）",
                    Some(gated),
                ),
            ],
        };
        let rows = plan_rows(
            &[
                entry("yue2", "", ""),
                entry("gated-model", "", ""),
                entry("sneaky-gated", "", ""),
            ],
            Ok(&catalog),
            Path::new("/models"),
        );
        for row in &rows {
            assert!(row.action.is_none(), "{} 不该给下载入口", row.id);
            assert!(!row.reason.is_empty(), "{} 必须给出原因", row.id);
            assert!(
                !row.reason.contains("http"),
                "{} 的原因里不许出现下载地址：{}",
                row.id,
                row.reason
            );
        }
        assert!(rows[0].reason.contains("没有 family=yue2"));
        assert!(rows[1].reason.contains("授权"));
    }

    /// 内置清单坏掉时：服务给了 url 的照常能用，其余如实说"内置清单读不出来"
    /// （不能把"读不出来"说成"这个模型没有源"，那是两回事）。
    #[test]
    fn broken_builtin_catalog_is_reported_as_such_not_as_no_source() {
        let rows = plan_rows(
            &[
                entry("a", "https://mirror.example/a.gguf", ""),
                entry("b", "", ""),
            ],
            Err("expected value"),
            Path::new("/models"),
        );
        assert_eq!(
            rows[0].action.as_ref().map(|a| a.url.as_str()),
            Some("https://mirror.example/a.gguf")
        );
        let reason = &rows[1].reason;
        assert!(reason.contains("内置下载清单读不出来"), "原因：{reason}");
        assert!(
            !reason.contains("没有下载源"),
            "别把读失败说成没有源：{reason}"
        );
    }

    /// 服务清单给了 url、而内置清单里没有这个模型时的兜底落点：
    /// path 在模型目录里 → 保留相对结构；不在 → 退回文件名；没有 path → url 末段；
    /// url 末段为空 → `<id>.gguf`（不能把目录当文件名）。
    #[test]
    fn server_only_models_fall_back_to_a_sane_relative_landing_path() {
        let rows = plan_rows(
            &[
                entry("a", "https://example.com/a.gguf", "/models/sub/a.gguf"),
                entry("b", "https://example.com/b.gguf", "/elsewhere/b.gguf"),
                entry("c", "https://example.com/w/c.gguf?token=x", ""),
                entry("d", "https://example.com/dir/", ""),
            ],
            Err("没有内置清单"),
            Path::new("/models"),
        );
        let dest = |id: &str| {
            rows.iter()
                .find(|r| r.id == id)
                .unwrap()
                .action
                .as_ref()
                .unwrap()
                .dest
                .clone()
        };
        assert_eq!(
            dest("a"),
            Path::new("/models/sub/a.gguf"),
            "清单 path 在模型目录里就保留结构"
        );
        assert_eq!(dest("b"), Path::new("/models/b.gguf"), "不在就退回文件名");
        assert_eq!(
            dest("c"),
            Path::new("/models/c.gguf"),
            "没有 path 就取 url 末段并剥 query"
        );
        assert_eq!(
            dest("d"),
            Path::new("/models/d.gguf"),
            "url 以 / 结尾时退回 <id>.gguf"
        );
        // path 在模型目录之外的这几条，必须同时给落点提醒（否则用户不知道服务会去哪找）
        assert!(rows
            .iter()
            .find(|r| r.id == "b")
            .unwrap()
            .action
            .as_ref()
            .unwrap()
            .conflict
            .is_some());
    }

    /// **按字段**回落：url 来自内置清单时，服务清单显式写了的 `sha256` / `size` 必须留下。
    ///
    /// 复核给的原始反例（这两个字段曾在内置分支被硬写成 `None`）：服务端声明的校验信息
    /// 被静默丢掉，等于把校验降级成"只对长度"——`server.json` 是服务方写的，它说了算。
    /// 现在内置清单也有 per-file sha256（①：两边都有时仍以服务侧为准，内置值不覆盖服务侧）。
    #[test]
    fn server_side_checksum_and_size_are_kept_when_the_url_comes_from_the_builtin_catalog() {
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg_sha(
                    "M-GGUF/m.gguf",
                    "https://builtin.example/m.gguf",
                    Some("builtinfilehash"),
                )),
            )],
        };
        let mut s = entry("m", "", "/models/M-GGUF/m.gguf");
        s.sha256 = "deadbeef".into();
        s.size = Some(123);
        let rows = plan_rows(&[s], Ok(&catalog), Path::new("/models"));
        let a = rows[0].action.as_ref().expect("内置清单有就该有入口");
        assert_eq!(a.origin, Origin::Builtin, "url 确实来自内置清单");
        assert_eq!(a.url, "https://builtin.example/m.gguf");
        assert_eq!(
            a.sha256.as_deref(),
            Some("deadbeef"),
            "服务侧 sha256 不能被丢掉，也不能被内置清单的 builtinfilehash 覆盖"
        );
        assert_eq!(a.size, Some(123), "服务侧 size 不能被丢掉");
    }

    /// ②：服务侧 sha256 为空 → 用内置清单 per-file 的 sha256（不是 None）——
    /// 校验不能静默退化成"只对长度"。
    #[test]
    fn builtin_file_sha256_is_the_fallback_when_the_server_writes_none() {
        let builtin_sha = "a".repeat(64);
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg_sha(
                    "M-GGUF/m.gguf",
                    "https://builtin.example/m.gguf",
                    Some(builtin_sha.as_str()),
                )),
            )],
        };
        let rows = plan_rows(
            &[entry("m", "", "/models/M-GGUF/m.gguf")],
            Ok(&catalog),
            Path::new("/models"),
        );
        let a = rows[0].action.as_ref().expect("内置清单有就该有入口");
        assert_eq!(a.origin, Origin::Builtin);
        assert_eq!(
            a.sha256.as_deref(),
            Some(builtin_sha.as_str()),
            "服务侧没写 sha256 时必须用内置清单 per-file 的值兜底"
        );
    }

    /// ③：两边都没有 sha256 → `None`（只按大小校验，行为与之前一致）。
    #[test]
    fn no_sha256_on_either_side_yields_none_size_only_check() {
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg("M-GGUF/m.gguf", "https://builtin.example/m.gguf")),
            )],
        };
        let mut s = entry("m", "", "/models/M-GGUF/m.gguf");
        s.size = Some(9); // 大小校验依据还在，只是没有哈希
        let rows = plan_rows(&[s], Ok(&catalog), Path::new("/models"));
        let a = rows[0].action.as_ref().expect("内置清单有就该有入口");
        assert_eq!(a.sha256, None, "两边都没有哈希就该如实 None");
        assert_eq!(a.size, Some(9));
    }

    /// 三个字段**各自**独立回落（不搞"要么全用服务、要么全用内置"）：
    /// url 有服务侧的用服务侧的，没有才用内置；sha256 / size 各自"服务侧非空即用"，
    /// sha256 服务侧为空时用内置 per-file 的兜底。
    #[test]
    fn url_checksum_and_size_fall_back_field_by_field() {
        const BUILTIN: &str = "https://builtin.example/m.gguf";
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg_sha("M-GGUF/m.gguf", BUILTIN, Some("builtinsha"))),
            )],
        };
        /// 服务清单侧给了什么、期望最终算出什么（字段名写清楚，别用嵌套元组——
        /// 嵌套元组会触发 clippy::type_complexity）。
        struct Case {
            server_url: &'static str,
            server_sha256: &'static str,
            server_size: Option<u64>,
            want_url: &'static str,
            want_sha256: Option<&'static str>,
            want_size: Option<u64>,
        }
        let cases = [
            // 服务只给了校验信息：url 用内置、校验用服务
            Case {
                server_url: "",
                server_sha256: "deadbeef",
                server_size: Some(123),
                want_url: BUILTIN,
                want_sha256: Some("deadbeef"),
                want_size: Some(123),
            },
            // 服务三个字段都给：全用服务
            Case {
                server_url: "https://mirror.example/m.gguf",
                server_sha256: "cafe",
                server_size: Some(9),
                want_url: "https://mirror.example/m.gguf",
                want_sha256: Some("cafe"),
                want_size: Some(9),
            },
            // 服务什么都没给：url 用内置，sha256 用内置 per-file 的兜底
            Case {
                server_url: "",
                server_sha256: "",
                server_size: None,
                want_url: BUILTIN,
                want_sha256: Some("builtinsha"),
                want_size: None,
            },
            // 服务只给了 size：size 用服务，sha256 仍用内置（不因服务侧空而退成 None）
            Case {
                server_url: "",
                server_sha256: "",
                server_size: Some(7),
                want_url: BUILTIN,
                want_sha256: Some("builtinsha"),
                want_size: Some(7),
            },
        ];
        for c in cases {
            let mut s = entry("m", c.server_url, "");
            s.sha256 = c.server_sha256.into();
            s.size = c.server_size;
            let rows = plan_rows(&[s], Ok(&catalog), Path::new("/models"));
            let a = rows[0].action.as_ref().unwrap();
            let got = (a.url.as_str(), a.sha256.as_deref(), a.size);
            assert_eq!(
                got,
                (c.want_url, c.want_sha256, c.want_size),
                "服务侧 url={:?} sha256={:?} size={:?} 的字段级回落不对",
                c.server_url,
                c.server_sha256,
                c.server_size
            );
        }
    }

    /// 落点校验：清单 `path` 在模型目录之外 → 必须把两边都摆出来。
    #[test]
    fn conflict_is_reported_when_the_declared_path_is_elsewhere() {
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg("M-GGUF/m.gguf", "https://example.com/m.gguf")),
            )],
        };
        let rows = plan_rows(
            &[entry("m", "", "/Volumes/DataExt/models/M-GGUF/m.gguf")],
            Ok(&catalog),
            Path::new("/tmp/aw-nowhere/models"),
        );
        let conflict = rows[0].action.as_ref().unwrap().conflict.clone().unwrap();
        // `conflict` 里是 `Path::display()`，Windows 上分隔符是 `\` —— 硬编码 `/` 的断言
        // 在那边必然假红（2026-09-18 三平台 CI 实测）。把两边都归一成 `/` 再比，
        // 断言的**强度不变**（仍然要求把两边路径都点出来），只是不再假设平台分隔符。
        let slashes = |s: &str| s.replace('\\', "/");
        let conflict = slashes(&conflict);
        assert!(
            conflict.contains("M-GGUF/m.gguf"),
            "要点名清单里的落点：{conflict}"
        );
        assert!(
            conflict.contains("aw-nowhere/models/M-GGUF/m.gguf"),
            "要点名本次会下到哪：{conflict}"
        );
    }

    /// `path` 指目录（gen 类模型）时不算冲突：文件落在那个目录里就是对上了。
    #[test]
    fn a_directory_declared_path_is_not_a_conflict() {
        let dest = PathBuf::from("/models/Stable-Audio-3-Small-Music-GGUF/x.gguf");
        assert_eq!(
            conflict_note(
                "/models/Stable-Audio-3-Small-Music-GGUF",
                &dest,
                Path::new("/models")
            ),
            None
        );
    }

    /// 服务清单里没有、内置清单里有的模型也要出现——否则"能下但没有入口"会静默消失。
    #[test]
    fn catalog_only_models_still_get_a_row() {
        let catalog = Catalog {
            models: vec![cat_model(
                "only-in-catalog",
                "downloadable",
                "",
                Some(pkg("X-GGUF/x.gguf", "https://example.com/x.gguf")),
            )],
        };
        let rows = plan_rows(&[], Ok(&catalog), Path::new("/models"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "only-in-catalog");
        assert!(rows[0].action.is_some());
    }

    /// **打进二进制的那份清单**必须能解析，且映射正确（生成脚本错了这里就红）。
    ///
    /// 重点钉住"不能只按 family 映射"：同一个 `audio8_tts` family 下 0.6B 有包、0.1B 没有；
    /// `index_tts2` 要选 2.5 而不是上游默认的 2.0；`qwen3_asr` 要选 0.6B 而不是默认 1.7B；
    /// `stable_audio` 要选 small-music 而不是默认的 medium。
    #[test]
    fn bundled_catalog_parses_and_maps_by_landing_path_not_family() {
        let c = catalog().expect("内置清单必须能解析");
        assert_eq!(
            c.models.len(),
            14,
            "随包清单的 14 个产品模型都该在 model-downloads.json 里"
        );

        let by_id: HashMap<&str, &CatalogModel> =
            c.models.iter().map(|m| (m.id.as_str(), m)).collect();
        let local = |id: &str| -> Option<String> {
            let m = by_id.get(id)?;
            let pkg = m.entry.as_ref()?;
            pkg.local_paths.first().cloned()
        };

        assert_eq!(
            local("audio8-tts").as_deref(),
            Some("Audio8-TTS-Preview-0.6B-GGUF/audio8-tts-preview-0.6b-q8_0.gguf")
        );
        assert_eq!(
            local("index-tts2").as_deref(),
            Some("IndexTTS2.5-GGUF/index-tts2_5-q8_0.gguf"),
            "上游默认包是 2.0，产品要的是 2.5"
        );
        assert_eq!(
            local("qwen3-asr").as_deref(),
            Some("Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf"),
            "上游默认包是 1.7B，产品要的是 0.6B"
        );
        assert_eq!(
            local("stable-audio-small-music").as_deref(),
            Some("Stable-Audio-3-Small-Music-GGUF/stable-audio-3-small-music-q8_0.gguf"),
            "上游默认包是 medium，产品要的是 small-music"
        );
        assert!(
            by_id["stable-audio-small-music"].note.contains("q8_0"),
            "同目录多量化档时必须写明按什么挑的：{}",
            by_id["stable-audio-small-music"].note
        );

        // 0.1B 变体与没有 spec 的两个：如实标"没有源"，且不给 URL
        for id in [
            "audio8-tts-01b",
            "audio8-tts-01b-stream",
            "yue2",
            "sheetsage2",
        ] {
            let m = by_id[id];
            assert_eq!(m.status, "no-source", "{id} 应标 no-source");
            assert!(m.entry.is_none(), "{id} 不该有 entry");
            assert!(!m.note.trim().is_empty(), "{id} 必须给出原因");
        }
        assert_eq!(
            by_id["audio8-asr"].status, "no-source",
            "上游明确不发布它的 GGUF"
        );

        // 每个能下载的入口都得是 https、落点相对、且带文件 URL
        for m in &c.models {
            let Some(pkg) = m.entry.as_ref() else {
                continue;
            };
            assert_eq!(
                m.status, "downloadable",
                "{}：有 entry 就该是 downloadable",
                m.id
            );
            assert!(!pkg.local_paths.is_empty(), "{}：缺落点", m.id);
            for rel in &pkg.local_paths {
                assert!(
                    !rel.starts_with('/') && !rel.split('/').any(|s| s == ".."),
                    "{}：落点必须是模型目录下的相对路径：{rel}",
                    m.id
                );
            }
            let url = pkg.files[0].url.as_deref().expect("必须有 URL");
            assert!(
                url.starts_with("https://"),
                "{}：URL 必须 https：{url}",
                m.id
            );
            assert!(pkg.files.len() == 1, "{}：本批只给单文件包下载入口", m.id);
        }

        // 本批验收点：真实清单的 9 条下载入口全部带 per-file sha256（64 位十六进制）——
        // 缺失 = 那条下载会静默退化成"只对长度"（同大小的旧权重查不出来）。
        let entries: Vec<&Package> = c.models.iter().filter_map(|m| m.entry.as_ref()).collect();
        assert_eq!(entries.len(), 9, "随包清单的下载入口数量变了，同步本用例");
        for pkg in entries {
            let sha = pkg.files[0].sha256.as_deref().unwrap_or("");
            assert_eq!(
                sha.len(),
                64,
                "{}：per-file sha256 缺失或不是 64 位",
                pkg.id
            );
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{}：sha256 必须是十六进制：{sha}",
                pkg.id
            );
        }
    }

    /// 真实 14 个产品模型的规划结果：**9 个有入口、5 个如实标没有源**。
    #[test]
    fn plan_for_the_real_machine_counts_nine_with_entry_and_five_without() {
        let c = catalog().unwrap();
        let server: Vec<ServerEntry> = c
            .models
            .iter()
            .map(|m| ServerEntry {
                id: m.id.clone(),
                // 随包清单里的条目默认都没有显式 url；显式 url 的覆盖在别的用例验证
                url: String::new(),
                sha256: String::new(),
                size: None,
                path: String::new(),
            })
            .collect();
        let rows = plan_rows(&server, Ok(c), Path::new("/models"));
        let ready: Vec<&str> = rows
            .iter()
            .filter(|r| r.action.is_some())
            .map(|r| r.id.as_str())
            .collect();
        let missing: Vec<&str> = rows
            .iter()
            .filter(|r| r.action.is_none())
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(
            ready,
            vec![
                "ace-step",
                "audio8-tts",
                "audio8-tts-stream",
                "fun-asr",
                "index-tts2",
                "qwen3-asr",
                "qwen3-tts-voicedesign",
                "sortformer-diar",
                "stable-audio-small-music",
            ],
            "9 个有下载入口"
        );
        assert_eq!(
            missing,
            vec![
                "audio8-asr",
                "audio8-tts-01b",
                "audio8-tts-01b-stream",
                "sheetsage2",
                "yue2",
            ],
            "5 个如实标没有源"
        );
        // 9 条入口都必须带 sha256（内置清单 per-file 兜底）——没带的那条会静默退化成只对长度
        assert!(
            rows.iter()
                .filter(|r| r.action.is_some())
                .all(|r| r.action.as_ref().is_some_and(|a| a.sha256.is_some())),
            "9 条下载入口都必须带 sha256"
        );
    }

    // ---- 档位体积 + 推荐 -------------------------------------------------

    /// 估算公式必须与服务**自己报的数字**逐位吻合：三个 `estimated N GiB` 来自真机
    /// `insufficient_memory` 的 503 原文，权重是实测的文件大小。
    ///
    /// 阳性对照：把 `RUNTIME_OVERHEAD_PERCENT` 改成 100（或删掉 `FIXED_OVERHEAD_BYTES`）
    /// → 本条红。
    #[test]
    fn footprint_estimate_reproduces_the_three_real_503_numbers() {
        // qwen3-asr 0.6B q8_0 + forced aligner 0.6B q8_0（合计 2282478660 B）
        // → 服务报 "estimated 3.31 GiB"
        assert_eq!(footprint_estimate(2282478660), 3557935718);
        assert_eq!(human_bytes(3557935718), "3.31 GiB");
        // index-tts2 2.5 q8_0（3502955328 B）→ 服务报 "estimated 5.02 GiB"
        assert_eq!(footprint_estimate(3502955328), 5388650720);
        assert_eq!(human_bytes(5388650720), "5.02 GiB");
        // stable-audio-small-music 目录树（1683577736 B）→ 服务报 "estimated 2.48 GiB"
        assert_eq!(footprint_estimate(1683577736), 2659584332);
        assert_eq!(human_bytes(2659584332), "2.48 GiB");
    }

    /// 多档：推荐"预算内最大的那一档"。
    ///
    /// 用 index-tts2 的真实三档（3.26 / 4.24 / 7.34 GiB 权重）：
    /// 16 GiB 机器预算 8 GiB → 推荐 f16（7.48 GiB 需求）；orig 要 12.14 GiB，排除。
    /// 阳性对照：把 `BUDGET_PERCENT_OF_PHYSICAL` 改成 100 → 会推荐 orig，本条红。
    #[test]
    fn recommend_tier_picks_the_largest_that_fits() {
        let model = model_with_tiers(
            "IndexTTS2.5-GGUF",
            &[
                ("q8_0", Some(3502955328), true),
                ("f16", Some(4547355072), false),
                ("orig", Some(7885093440), false),
            ],
            0,
        );
        let tiers = tiers_of(&model);
        assert_eq!(tiers.len(), 3, "三档都在同一个目录里");
        assert!(tiers[0].is_entry, "入口是 q8_0（清单 path 指向它）");
        assert_eq!(tiers[0].footprint_bytes, Some(5388650720));

        let advice = recommend_tier(&tiers, &budget(16.0, 1.0));
        assert_eq!(advice.tier.as_deref(), Some("f16"), "{}", advice.note);
        assert!(advice.note.contains("本机推荐 f16"), "{}", advice.note);
        // 推荐的**不是**入口那一档（入口是 q8_0）→ 必须点明按钮下的是哪一档
        assert!(
            advice.note.contains("下载按钮取的是 q8_0"),
            "{}",
            advice.note
        );

        // 32 GiB 的机器：预算 16 GiB，最大的 orig（12.14 GiB）也装得下
        assert_eq!(
            recommend_tier(&tiers, &budget(32.0, 1.0)).tier.as_deref(),
            Some("orig")
        );
    }

    /// 推荐的**就是**入口那一档时，别再补一句"下载按钮取的是…"（没有错位就不用解释）。
    #[test]
    fn advice_stays_quiet_about_the_button_when_it_matches_the_recommendation() {
        let model = model_with_tiers(
            "IndexTTS2.5-GGUF",
            &[
                ("q8_0", Some(3502955328), true),
                ("orig", Some(7885093440), false),
            ],
            0,
        );
        // 13 GiB 机器：预算 6.5 GiB → 只有 q8_0（6.02 GiB）装得下，而它就是入口
        let advice = recommend_tier(&tiers_of(&model), &budget(13.0, 1.0));
        assert_eq!(advice.tier.as_deref(), Some("q8_0"), "{}", advice.note);
        assert!(!advice.note.contains("下载按钮"), "{}", advice.note);
    }

    /// 全都装不下：`tier` 必须是 `None`（界面不显示推荐档），理由里给出数字。
    ///
    /// 阳性对照：把 `per_model_budget` 改成不除（`physical_memory` 直接用）→ 本条红。
    #[test]
    fn recommend_tier_refuses_when_nothing_fits() {
        let model = model_with_tiers(
            "IndexTTS2.5-GGUF",
            &[
                ("q8_0", Some(3502955328), true),
                ("f16", Some(4547355072), false),
            ],
            0,
        );
        // 8 GiB 机器：预算 4 GiB，最小档也要 6.02 GiB
        let advice = recommend_tier(&tiers_of(&model), &budget(8.0, 1.0));
        assert_eq!(advice.tier, None);
        assert!(advice.note.contains("装不下任何一档"), "{}", advice.note);
        assert!(advice.note.contains("q8_0"), "{}", advice.note);
        assert!(advice.note.contains("6.02 GiB"), "{}", advice.note);
    }

    /// 体积未知：不许瞎推荐，也不许把未知档当成"装得下"。
    #[test]
    fn recommend_tier_reports_unknown_sizes_instead_of_guessing() {
        let model = model_with_tiers(
            "X-GGUF",
            &[("q8_0", Some(3000000000), true), ("f16", None, false)],
            0,
        );
        let tiers = tiers_of(&model);
        assert_eq!(tiers[1].footprint_bytes, None);
        let advice = recommend_tier(&tiers, &budget(16.0, 1.0));
        assert_eq!(advice.tier.as_deref(), Some("q8_0"));
        assert!(advice.note.contains("另有 1 档体积未知"), "{}", advice.note);
        assert!(advice.note.contains("未参与比较"), "{}", advice.note);

        // 全未知 → 连结论都不给；**绝不能出现"装得下"**（未知不是零）
        let all_unknown =
            model_with_tiers("Y-GGUF", &[("q8_0", None, true), ("f16", None, false)], 0);
        let advice = recommend_tier(&tiers_of(&all_unknown), &budget(16.0, 1.0));
        assert_eq!(advice.tier, None);
        assert!(advice.note.contains("体积未知"), "{}", advice.note);
        assert!(
            !advice.note.contains("—— 装得下"),
            "体积未知不能给出「装得下」的结论：{}",
            advice.note
        );

        // **单档 + 体积未知**：这条专门钉 `known.is_empty()` 那道守卫。
        // 少了它就会走到"只有一档"分支，把 `footprint_bytes` 的 0 当真实体积，
        // 于是输出「估算占用 0 B … —— 装得下」——把"不知道"说成"装得下"。
        // （多档全未知走的是另一条分支，抓不到这个 bug，所以必须单开一条。）
        let single_unknown = model_with_tiers("Z-GGUF", &[("q8_0", None, true)], 0);
        let advice = recommend_tier(&tiers_of(&single_unknown), &budget(16.0, 1.0));
        assert_eq!(advice.tier, None, "体积未知时不该给出结论");
        assert!(advice.note.contains("体积未知"), "{}", advice.note);
        assert!(
            !advice.note.contains("—— 装得下") && !advice.note.contains("估算占用 0"),
            "单档体积未知被说成装得下（known.is_empty() 守卫失效）：{}",
            advice.note
        );
    }

    /// 内存未知：不猜（`tier` 为 None，理由说清是"拿不到物理内存"）。
    ///
    /// 阳性对照：把 `physical_memory` 的 `None` 当成 0 兜底 → 会退化成"全都装不下"，本条红。
    #[test]
    fn recommend_tier_reports_unknown_memory_instead_of_guessing() {
        let model = model_with_tiers("X-GGUF", &[("q8_0", Some(1000), true)], 0);
        let advice = recommend_tier(
            &tiers_of(&model),
            &MachineBudget {
                physical_memory: None,
                headroom_bytes: DEFAULT_HEADROOM_BYTES,
            },
        );
        assert_eq!(advice.tier, None);
        assert!(
            advice.note.contains("拿不到本机物理内存"),
            "{}",
            advice.note
        );
        assert!(
            !advice.note.contains("装不下"),
            "别把未知说成装不下：{}",
            advice.note
        );
    }

    /// 只有一档：没有可比较的档，所以不给"推荐"，但要说清这一档装不装得下。
    #[test]
    fn recommend_tier_describes_a_single_tier_without_recommending() {
        let model = model_with_tiers("X-GGUF", &[("q8_0", Some(1000000000), true)], 0);
        let tiers = tiers_of(&model);
        assert_eq!(tiers.len(), 1);
        let fits = recommend_tier(&tiers, &budget(16.0, 1.0));
        assert_eq!(fits.tier, None, "只有一档就没有'推荐哪一档'");
        assert!(fits.note.contains("只有一档 q8_0"), "{}", fits.note);
        assert!(fits.note.contains("装得下"), "{}", fits.note);

        let too_small = recommend_tier(&tiers, &budget(1.0, 1.0));
        assert!(too_small.note.contains("装不下"), "{}", too_small.note);
    }

    /// 档位只收**同目录**里的**单文件**包：
    /// 别的目录 = 别的变体（ace-step 的 turbo / base / xl），多文件包 = 下载器还不支持。
    ///
    /// 阳性对照：去掉 `parent_dir(rel) == dir` 这一条 → 会把别的变体算成"另一档"，本条红。
    #[test]
    fn tiers_are_only_the_same_directory_single_file_packages() {
        let mut model = model_with_tiers(
            "Wanted-GGUF",
            &[("q8_0", Some(100), true), ("f16", Some(200), false)],
            0,
        );
        // 另一个变体：同 target_directory、不同子目录（ace-step 的真实形状）
        let mut other = pkg("Other-GGUF/x.gguf", "https://example.com/other.gguf");
        other.precision = "q8_0".into();
        other.bytes = Some(999);
        model.packages.push(other);
        // 多文件包：下载器不支持，不许列成"可下的档"
        let mut multi = pkg("Wanted-GGUF/a.json", "https://example.com/a.json");
        multi.local_paths = vec![
            "Wanted-GGUF/a.json".into(),
            "Wanted-GGUF/b.safetensors".into(),
        ];
        multi.files = vec![
            PackageFile {
                url: Some("https://example.com/a.json".into()),
                sha256: None,
            },
            PackageFile {
                url: Some("https://example.com/b.safetensors".into()),
                sha256: None,
            },
        ];
        model.packages.push(multi);

        let tiers = tiers_of(&model);
        assert_eq!(tiers.len(), 2, "只该有同目录的两个单文件档：{tiers:?}");
        assert_eq!(
            tiers
                .iter()
                .map(|t| t.precision.as_str())
                .collect::<Vec<_>>(),
            vec!["q8_0", "f16"]
        );
    }

    /// 辅助权重（如 qwen3-asr 的 forced aligner）必须算进占用估算，否则推荐会偏乐观。
    #[test]
    fn aux_weights_are_part_of_the_footprint() {
        let bare = model_with_tiers("X-GGUF", &[("q8_0", Some(1000000000), true)], 0);
        let with_aux = model_with_tiers("X-GGUF", &[("q8_0", Some(1000000000), true)], 500000000);
        assert_eq!(
            tiers_of(&bare)[0].footprint_bytes,
            Some(footprint_estimate(1000000000))
        );
        assert_eq!(
            tiers_of(&with_aux)[0].footprint_bytes,
            Some(footprint_estimate(1500000000))
        );
    }

    /// 没能核实的辅助权重会让估算**偏低**，界面必须如实说（不许悄悄乐观）。
    #[test]
    fn advice_carries_the_unresolved_aux_caveat() {
        let mut model = model_with_tiers("X-GGUF", &[("q8_0", Some(3000000000), true)], 0);
        model.aux_unresolved = vec!["x.vad_model_path".into()];
        let catalog = Catalog {
            models: vec![model],
        };
        let (_tiers, advice) = tiers_and_advice(Ok(&catalog), "m", &budget(16.0, 1.0));
        assert!(
            advice.note.contains("1 项辅助权重没能核实体积"),
            "{}",
            advice.note
        );
    }

    /// 清单里没有这个模型（或清单读不出来）→ 没有档位、也没有推荐文案。
    #[test]
    fn tiers_and_advice_is_empty_without_a_catalog_entry() {
        let catalog = Catalog { models: vec![] };
        let (tiers, advice) = tiers_and_advice(Ok(&catalog), "m", &budget(16.0, 1.0));
        assert!(tiers.is_empty());
        assert_eq!(advice, Advice::default());
        let (tiers, advice) = tiers_and_advice(Err("坏了"), "m", &budget(16.0, 1.0));
        assert!(tiers.is_empty());
        assert_eq!(advice, Advice::default());
    }

    /// 物理内存探测：三种平台的输出都要认（在任一平台上都可测）。
    ///
    /// 阳性对照：把 Linux 那支的 `* 1024` 去掉 → 第一条断言红（16 GB 变成 16 MB，推荐全乱）。
    #[test]
    fn physical_memory_parsing_covers_all_three_platforms() {
        // macOS `sysctl -n hw.memsize`
        assert_eq!(parse_physical_memory("17179869184\n"), Some(17179869184));
        // Windows PowerShell（可能带 BOM）
        assert_eq!(
            parse_physical_memory("\u{feff}17179869184\r\n"),
            Some(17179869184)
        );
        // Linux /proc/meminfo：kB → 字节
        assert_eq!(
            parse_physical_memory("MemTotal:       16777216 kB\nMemFree:  1 kB\n"),
            Some(17179869184)
        );
        // 认不出来 / 0 / 空 → None（不许拿 0 当"内存"）
        assert_eq!(parse_physical_memory(""), None);
        assert_eq!(parse_physical_memory("MemTotal:       0 kB\n"), None);
        assert_eq!(parse_physical_memory("not a number\n"), None);
    }

    /// 探测缝：命令起不来 / 输出读不到 → `None`（界面说"拿不到"，不是"内存是 0"）。
    #[test]
    fn physical_memory_with_reports_none_when_the_probe_fails() {
        assert_eq!(physical_memory_with(&|| None), None);
        assert_eq!(physical_memory_with(&|| Some(String::new())), None);
        assert_eq!(
            physical_memory_with(&|| Some("17179869184".to_string())),
            Some(17179869184)
        );
    }

    /// 真机自检：这台机器上必须**真的拿到**物理内存（探测链没断）。
    #[test]
    fn physical_memory_is_available_on_this_machine() {
        let total = physical_memory_bytes().expect("本机应能探测物理内存");
        assert!(
            total >= 2 * 1024 * 1024 * 1024,
            "物理内存 {} 字节小得可疑，探测可能解析错了",
            total
        );
    }

    /// 探测只做一次：下载面板每次重建行都会问物理内存，而 Windows 的探测要起一个
    /// PowerShell —— 每条进度快照都探一遍就是真机上"下载界面卡死"的成因。
    ///
    /// 这里用计数探针证明缓存真的生效（同一次探测的返回值也必须一致）。
    #[test]
    fn physical_memory_is_probed_once_per_process() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, Ordering::Relaxed);
            Some("17179869184".to_string())
        };
        let cache = OnceLock::new();
        assert_eq!(physical_memory_once(&cache, &probe), Some(17179869184));
        assert_eq!(physical_memory_once(&cache, &probe), Some(17179869184));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "第二次不该再探（探测要起 PowerShell / sysctl）"
        );

        // 探测失败也要缓存：不然拿不到的机器会每条进度都重试一次
        let fails = AtomicUsize::new(0);
        let failing = || {
            fails.fetch_add(1, Ordering::Relaxed);
            None
        };
        let cache = OnceLock::new();
        assert_eq!(physical_memory_once(&cache, &failing), None);
        assert_eq!(physical_memory_once(&cache, &failing), None);
        assert_eq!(fails.load(Ordering::Relaxed), 1);
    }
}
