//! 随包分发的模型下载清单（`config/model-downloads.json`）。
//!
//! ## 为什么要有它
//! P7 的下载队列（串行 / 断点续传 / 校验后提交）本身是好的，但**下载入口只在服务清单
//! 的模型带 `url` 时才出现**，而 `server.json` 是引擎写的，13 个模型一个 `url` 都没有——
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
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Package {
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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PackageFile {
    pub url: Option<String>,
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
                            // 服务清单里没有这个模型 → 没有服务侧校验信息可谈（空条目）
                            action: Some(action_for(
                                &ServerEntry::default(),
                                url,
                                dest,
                                Origin::Builtin,
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
        return Some(action_for(
            s,
            url.to_string(),
            dest,
            Origin::Server,
            model_dir,
        ));
    }
    let pkg = usable_builtin_package(cat)?;
    let rel = &pkg.local_paths[0];
    let url = pkg.files[0].url.clone()?;
    let dest = model_dir.join(rel);
    Some(action_for(s, url, dest, Origin::Builtin, model_dir))
}

/// 组装一条下载入口——**唯一一处**（两个分支共用，免得"服务侧校验信息"在其中一条上漂掉）。
///
/// 口径是**按字段**回落，不是"要么全用服务、要么全用内置"：
/// `url` 由调用方决定（服务清单有就用服务的），但 `sha256` / `size` **只要服务清单写了就用
/// 服务侧的**——内置清单里根本没有这两个字段可打（生成不联网），所以服务侧有就该用；
/// 丢掉它等于把校验静默降级成"只对长度"。
fn action_for(
    s: &ServerEntry,
    url: String,
    dest: PathBuf,
    origin: Origin,
    model_dir: &Path,
) -> Action {
    let sha = s.sha256.trim();
    Action {
        url,
        sha256: (!sha.is_empty()).then(|| sha.to_string()),
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
        CatalogModel {
            id: id.into(),
            spec: "f.json".into(),
            status: status.into(),
            note: note.into(),
            entry: pkg,
        }
    }

    fn pkg(local: &str, url: &str) -> Package {
        Package {
            local_paths: vec![local.into()],
            gated: false,
            files: vec![PackageFile {
                url: Some(url.into()),
            }],
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
    #[test]
    fn server_side_checksum_and_size_are_kept_when_the_url_comes_from_the_builtin_catalog() {
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg("M-GGUF/m.gguf", "https://builtin.example/m.gguf")),
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
            "服务侧 sha256 不能被丢掉"
        );
        assert_eq!(a.size, Some(123), "服务侧 size 不能被丢掉");
    }

    /// 三个字段**各自**独立回落（不搞"要么全用服务、要么全用内置"）：
    /// url 有服务侧的用服务侧的，没有才用内置；sha256 / size 各自"服务侧非空即用"。
    #[test]
    fn url_checksum_and_size_fall_back_field_by_field() {
        const BUILTIN: &str = "https://builtin.example/m.gguf";
        let catalog = Catalog {
            models: vec![cat_model(
                "m",
                "downloadable",
                "",
                Some(pkg("M-GGUF/m.gguf", BUILTIN)),
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
            // 服务什么都没给：全用内置（内置没有校验信息 → None）
            Case {
                server_url: "",
                server_sha256: "",
                server_size: None,
                want_url: BUILTIN,
                want_sha256: None,
                want_size: None,
            },
            // 服务只给了 size：size 用服务，sha 仍然是 None（不能凭 size 编一个 sha）
            Case {
                server_url: "",
                server_sha256: "",
                server_size: Some(7),
                want_url: BUILTIN,
                want_sha256: None,
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
            13,
            "本机 server.json 的 13 个模型都该在清单里"
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
    }

    /// 真实 13 个模型的规划结果：**8 个有入口、5 个如实标没有源**（issue 验收 ⑥ 的数字）。
    #[test]
    fn plan_for_the_real_machine_counts_eight_with_entry_and_five_without() {
        let c = catalog().unwrap();
        let server: Vec<ServerEntry> = c
            .models
            .iter()
            .map(|m| ServerEntry {
                id: m.id.clone(),
                // 真机 server.json 13/13 都没有 url
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
                "sortformer-diar",
                "stable-audio-small-music",
            ],
            "8 个有下载入口"
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
    }
}
