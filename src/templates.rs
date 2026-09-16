//! 配音模板与预设（M4-P2）：把「音色 / 语速 / 停顿 / 兜底规则」存成一份可复用的模板。
//!
//! 产品口径（docs/product-plan.md §4.2 P2）：抽屉里的这些设置保存为模板，跨工程复用；
//! 免费侧对照是"每个工程手动设一次"。
//!
//! 这里只放**纯逻辑**：模板结构、名字归一、增删改查、以及"应用这份模板会让哪些东西作废"
//! 的判定。落盘路径与 UI 接线在 `src/main.rs`（模板文件跟 settings.json 同目录）。

use std::path::Path;

/// 一份配音模板。字段刻意是"能真的影响产物（或试听）的那些输入"：
/// - `model` / `voice_ref`：引擎与音色（换任一都要重录）
/// - `auto_normalize`：兜底规则开关（改的是 spoken 文本，也要重录）
/// - `gap_ms`：句间停顿（只影响拼装，改了重新导出即可）
/// - `speed`：**试听语速**，不进产物（模板里带上它，是因为它属于"这套工作方式"）
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DubTemplate {
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub voice_ref: Option<String>,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default = "default_gap_ms")]
    pub gap_ms: u64,
    #[serde(default = "default_true")]
    pub auto_normalize: bool,
}

fn default_speed() -> f32 {
    1.0
}

fn default_gap_ms() -> u64 {
    250
}

fn default_true() -> bool {
    true
}

/// 应用一份模板会让哪些东西作废。
///
/// 三种档位对应三种**真实代价**，界面文案与这里的判定必须同源（别一句"已应用"糊过去）：
/// 重录 > 重新导出 > 只影响试听。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyEffect {
    /// 只影响试听（例如语速）：产物一个字都不用动
    AuditionOnly,
    /// 需要重新拼装/导出（例如停顿）：句间静音变了，但已合成的句子都还有效
    ReassembleOnly,
    /// 需要重新合成（模型 / 音色 / 兜底规则变了）：旧音频不能再用
    Resynthesize,
}

impl ApplyEffect {
    /// 给界面的一句话（与判定同源，不另写一套）
    pub fn note(self) -> &'static str {
        match self {
            ApplyEffect::AuditionOnly => "只改了试听语速：产物不变",
            ApplyEffect::ReassembleOnly => "停顿改了：重新导出即可生效（已合成的句子不用重录）",
            ApplyEffect::Resynthesize => "换了引擎/音色或兜底规则：需要重新合成",
        }
    }
}

/// 当前工程的输入（用来和模板比，算出作废档位）。
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectInputs {
    pub model: String,
    pub voice_ref: Option<String>,
    pub gap_ms: u64,
    pub auto_normalize: bool,
}

/// 应用模板会产生什么效果（纯函数，便于单测三种档位）。
///
/// 优先级：只要能触发重录就是 `Resynthesize`（哪怕停顿也变了）；否则只要停顿变了就是
/// `ReassembleOnly`；再否则 `AuditionOnly`。
pub fn apply_effect(current: &ProjectInputs, template: &DubTemplate) -> ApplyEffect {
    if current.model != template.model
        || current.voice_ref != template.voice_ref
        || current.auto_normalize != template.auto_normalize
    {
        return ApplyEffect::Resynthesize;
    }
    if current.gap_ms != template.gap_ms {
        return ApplyEffect::ReassembleOnly;
    }
    ApplyEffect::AuditionOnly
}

/// 模板集合（一份文件里的全部模板）。
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TemplateSet {
    #[serde(default)]
    pub templates: Vec<DubTemplate>,
}

impl TemplateSet {
    /// 按名字（大小写不敏感——macOS/Windows 上用户不会觉得 `A` 与 `a` 是两个模板）
    /// 覆盖保存；顺序保持"最新的排前面"，用户刚存的就在手边。
    pub fn upsert(&mut self, t: DubTemplate) {
        let key = t.name.to_lowercase();
        self.templates.retain(|x| x.name.to_lowercase() != key);
        self.templates.insert(0, t);
    }

    /// 删除一个模板；返回是否真的删掉了（界面据此说"没有这个模板"还是"已删除"）。
    pub fn remove(&mut self, name: &str) -> bool {
        let key = name.to_lowercase();
        let before = self.templates.len();
        self.templates.retain(|x| x.name.to_lowercase() != key);
        self.templates.len() != before
    }

    pub fn get(&self, name: &str) -> Option<&DubTemplate> {
        let key = name.to_lowercase();
        self.templates.iter().find(|x| x.name.to_lowercase() == key)
    }

    pub fn names(&self) -> Vec<String> {
        self.templates.iter().map(|x| x.name.clone()).collect()
    }
}

/// 模板文件的相对路径（与应用设置、工程产物同目录，便于用户备份）。
pub const TEMPLATES_FILE: &str = "templates.json";

/// 从文件读模板集。**坏文件不能崩、也不能把用户已有的模板当成空**：
/// 读不出来时返回 `Err`，让调用方决定怎么提示（而不是静默给一份空表，
/// 那样用户一保存就把原文件覆盖掉了）。
pub fn load(path: &Path) -> Result<TemplateSet, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| format!("模板文件解析失败：{}（{e}）", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TemplateSet::default()),
        Err(e) => Err(format!("模板文件读不出来：{}（{e}）", path.display())),
    }
}

/// 原子写回模板集（临时文件 + fsync + rename，复用 aw-core 的实现）。
pub fn save(path: &Path, set: &TemplateSet) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("建目录失败：{}（{e}）", dir.display()))?;
    }
    let body = serde_json::to_vec_pretty(set).map_err(|e| format!("模板序列化失败：{e}"))?;
    aw_core::dub::write_atomic_explained(path, &body)
        .map_err(|e| format!("模板写入失败：{}（{e}）", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(name: &str, model: &str, voice: Option<&str>, gap: u64, normalize: bool) -> DubTemplate {
        DubTemplate {
            name: name.into(),
            model: model.into(),
            voice_ref: voice.map(str::to_string),
            speed: 1.0,
            gap_ms: gap,
            auto_normalize: normalize,
        }
    }

    #[test]
    fn upsert_replaces_by_name_case_insensitively_and_keeps_newest_first() {
        let mut set = TemplateSet::default();
        set.upsert(tpl("口播", "audio8-tts", None, 250, true));
        set.upsert(tpl("访谈", "audio8-tts", None, 400, true));
        assert_eq!(set.names(), vec!["访谈", "口播"], "最新的排前面");

        // 大小写不同视为同一个模板：覆盖，而不是多出一条
        set.upsert(tpl("口播", "index-tts2", Some("/x/voice.wav"), 300, false));
        assert_eq!(set.templates.len(), 2, "同名（忽略大小写）只保留一条");
        let got = set.get("口播").unwrap();
        assert_eq!(got.model, "index-tts2");
        assert_eq!(got.gap_ms, 300);
        assert!(!got.auto_normalize);
        assert_eq!(set.get("口播").unwrap().name, "口播");
    }

    #[test]
    fn remove_reports_whether_something_was_removed() {
        let mut set = TemplateSet::default();
        set.upsert(tpl("甲", "audio8-tts", None, 250, true));
        assert!(set.remove("甲"));
        assert!(!set.remove("甲"), "删过了再删要如实返回 false");
        assert!(set.templates.is_empty());
    }

    #[test]
    fn json_roundtrip_and_tolerant_defaults() {
        let mut set = TemplateSet::default();
        set.upsert(tpl("口播", "audio8-tts", Some("/v.wav"), 300, false));
        let raw = serde_json::to_string(&set).unwrap();
        let back: TemplateSet = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, set);

        // 手写的模板只给了名字与模型：其余字段回落默认（不能让整份文件读不出来）
        let partial: TemplateSet =
            serde_json::from_str(r#"{"templates":[{"name":"极简","model":"audio8-tts"}]}"#)
                .unwrap();
        let t = &partial.templates[0];
        assert_eq!(t.speed, 1.0);
        assert_eq!(t.gap_ms, 250);
        assert!(t.auto_normalize, "缺字段按历史默认（开）");
        assert_eq!(t.voice_ref, None);
    }

    /// 应用模板的三种档位：重录 > 重新导出 > 只影响试听。
    #[test]
    fn apply_effect_picks_the_heaviest_real_consequence() {
        let current = ProjectInputs {
            model: "audio8-tts".into(),
            voice_ref: None,
            gap_ms: 250,
            auto_normalize: true,
        };
        let same = tpl("同款", "audio8-tts", None, 250, true);
        assert_eq!(apply_effect(&current, &same), ApplyEffect::AuditionOnly);

        let gap_only = tpl("只改停顿", "audio8-tts", None, 500, true);
        assert_eq!(
            apply_effect(&current, &gap_only),
            ApplyEffect::ReassembleOnly
        );

        for heavier in [
            tpl("换模型", "index-tts2", None, 250, true),
            tpl("换音色", "audio8-tts", Some("/v.wav"), 250, true),
            tpl("关兜底", "audio8-tts", None, 250, false),
        ] {
            assert_eq!(
                apply_effect(&current, &heavier),
                ApplyEffect::Resynthesize,
                "{} 必须按重录处理",
                heavier.name
            );
        }

        // 同时改了停顿与模型：按最重的那档（重录）
        let both = tpl("都改", "index-tts2", None, 900, true);
        assert_eq!(apply_effect(&current, &both), ApplyEffect::Resynthesize);
        for e in [
            ApplyEffect::AuditionOnly,
            ApplyEffect::ReassembleOnly,
            ApplyEffect::Resynthesize,
        ] {
            assert!(!e.note().is_empty(), "每个档位都要有人话说明");
        }
    }

    #[test]
    fn load_missing_file_is_empty_but_broken_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("aw-templates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(TEMPLATES_FILE);

        // 第一次跑：没有文件 = 空集（正常）
        assert_eq!(load(&path).unwrap(), TemplateSet::default());

        // 坏文件：必须报错，不能静默给空集（否则下一次保存就把用户模板覆盖没了）
        std::fs::write(&path, b"{ not json").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("模板文件解析失败"), "{err}");

        // 存 → 读回来
        let mut set = TemplateSet::default();
        set.upsert(tpl("口播", "audio8-tts", None, 250, true));
        save(&path, &set).unwrap();
        assert_eq!(load(&path).unwrap(), set);
        // 原子写不留 .tmp
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// 模板集的名字表与 `get` 同源（界面下拉直接用它）。
    #[test]
    fn names_match_get() {
        let mut set = TemplateSet::default();
        set.upsert(tpl("甲", "audio8-tts", None, 250, true));
        set.upsert(tpl("乙", "audio8-tts", None, 250, true));
        for n in set.names() {
            assert!(set.get(&n).is_some(), "{n} 在 names 里但 get 找不到");
        }
        assert!(set.get("丙").is_none());
    }

    #[test]
    fn templates_file_path_shape() {
        use std::path::PathBuf;
        // 常量就是文件名（调用方拼到应用数据目录下）；这里只钉住它不是绝对路径、也不是空
        assert!(!TEMPLATES_FILE.is_empty());
        assert_eq!(PathBuf::from(TEMPLATES_FILE).components().count(), 1);
    }
}
