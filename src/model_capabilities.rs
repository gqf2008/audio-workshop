//! 随包分发的模型**能力清单**（`config/model-capabilities.json`）。
//!
//! ## 为什么要有它
//! `role` / `requires` / `known_issues` / `product_excluded` 这四个是**纯产品侧**元数据
//! —— 服务端 `app/server/config.cpp` 只 `find` 它认识的键，这四个一个都不读，只有 App 在读。
//! 它们原本只能靠 `tools/audio_config.py render --write` 透传进 `server.json` 才能到界面，
//! 而那一步**覆盖用户的服务配置**：于是"看到能力提示"变成了用户必须先手工做一次的动作。
//!
//! **`mode` 是例外**：服务端也读它（`config.cpp` 的 `optional_string(item, "mode", …)`），
//! 并在 `runtime.cpp` 强制"streaming/live 必须 `mode=streaming`"。这里把它一起兜底，
//! 方向只会**更保守**（把 streaming-only 的模型藏起来），不会放出服务端会拒的引擎 ——
//! 服务端的缺省是 `offline`，而随包清单说 `streaming` 时 App 选择隐藏。
//!
//! 现在同一份 schema 走**两条投递路径**，App **逐字段**回落：
//!
//! | 来源 | 谁写 | 优先级 |
//! |---|---|---|
//! | `server.json` | 用户 / `audio_config.py render --write` | **高**（显式给了就以它为准） |
//! | `config/model-capabilities.json`（随包，`include_str!`） | `tools/gen_model_capabilities.py` | 低（服务端没给才用） |
//!
//! `render --write` 因此变成**可选**（想让服务配置也带上这些元数据时才做），不再是前提。
//!
//! ## 两个形状，职责不同（别合并）
//! - [`RawCaps`]：**服务端原文**里的能力字段，全是 `Option` —— 只有它能区分
//!   "服务端显式写了 `false`"与"服务端根本没写这一项"。serde 的 `default` 会把两者抹平，
//!   所以不能拿已解析的生效值反过来推"服务端给没给"。
//! - [`Capability`]：**生效值**（服务端显式 → 随包清单 → 未声明默认），由 [`resolve`]
//!   这**唯一一处**产出，调用方只做无条件赋值、不再自己写第二份判断。
//!
//! 合并只留一处，是因为"哪份清单说了算"只该有一个答案
//! （见 `LESSON_同一语义两处实现必然漂移回显需与真实行为同源`）。

use std::sync::OnceLock;

/// 打进二进制的随包能力清单（由 `tools/gen_model_capabilities.py` 生成，**别手改**）。
const CATALOG_JSON: &str = include_str!("../config/model-capabilities.json");

// ===========================================================================
// 随包能力清单（低优先级来源）
// ===========================================================================

#[derive(Debug, serde::Deserialize)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
}

impl Catalog {
    /// 按 id 找随包能力。**找不到不是错误**：清单里可能有服务端独有的模型，
    /// 那种模型就只按服务端声明（没有兜底）。
    pub fn find(&self, id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CatalogModel {
    pub id: String,
    /// 产品层已排除（schema 的 `product_excluded`）：仍注册，但不在界面暴露。
    #[serde(default)]
    pub product_excluded: bool,
    /// `offline` / `streaming`（schema 的 `mode`）。
    #[serde(default)]
    pub mode: String,
    /// 能力角色（schema 的 `role`）：`scoring` / `streaming` / `fast-asr` / …
    #[serde(default)]
    pub role: String,
    /// 后端硬要求（schema 的 `requires`）：如 index-tts2 的 `voice_ref: true`。
    #[serde(default)]
    pub requires: Option<Requires>,
    /// 已知缺陷（schema 的 `known_issues`）：只在选择处**只读**展示，不参与自动决策。
    #[serde(default)]
    pub known_issues: Vec<String>,
}

/// 随包清单。`Err` = **内置文件本身坏了**（读不出来）—— 那是我们自己产物的问题，
/// 不是"这个模型没有能力声明"，所以调用方要如实报，不许静默当成"没兜底"。
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
// 服务端原文形状（高优先级来源）：Option 才能表达"没写"
// ===========================================================================

/// 后端硬要求。目前只有一项（`index-tts2` 必须给参考音频），但按 schema 的结构声明。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub voice_ref: bool,
}

/// `server.json` 里一条记录的**原始**能力字段：`None` = 服务端没写这一项。
///
/// 用它而不是从已解析的 `ServerModel` 反推，是因为 serde 的 `default` 让
/// "没写"和"写了缺省值"长得一模一样，而回落的判据恰恰是"服务端到底写没写"。
#[derive(Debug, Default, serde::Deserialize)]
pub struct RawCaps {
    #[serde(default)]
    pub product_excluded: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub requires: Option<Requires>,
    #[serde(default)]
    pub known_issues: Option<Vec<String>>,
}

// ===========================================================================
// 生效能力 + 唯一的合并点
// ===========================================================================

/// 生效能力：服务端显式值 → 随包清单 → 未声明默认。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capability {
    pub product_excluded: bool,
    pub mode: String,
    pub role: String,
    pub requires: Option<Requires>,
    pub known_issues: Vec<String>,
}

impl Capability {
    /// 只在流式通道上跑（`/v1/audio/speech/live`）：离线批量与评估不适用，不能当离线引擎选。
    ///
    /// `mode` 与 `role` 都可能承载这个信息（schema 里两种写法都出现过：`audio8-tts-stream`
    /// 是 `mode: streaming`，`audio8-tts-01b-stream` 同时有 `mode` 与 `role`），两个都认。
    pub fn is_streaming_only(&self) -> bool {
        self.mode.eq_ignore_ascii_case("streaming") || self.role.eq_ignore_ascii_case("streaming")
    }

    /// 该引擎是否**必须**提供参考音频（index-tts2：不接 voice_ref 直接报错）。
    pub fn requires_voice_ref(&self) -> bool {
        self.requires.as_ref().is_some_and(|r| r.voice_ref)
    }

    /// 已知缺陷的一句话只读说明（空 = 没登记）。
    pub fn known_issues_note(&self) -> String {
        self.known_issues.join("；")
    }
}

/// **唯一**的合并点：逐字段回落 —— 服务端写了就用服务端的，没写才用随包清单，
/// 两边都没写就按未声明默认。
///
/// **不做"整条记录二选一"**：服务端只补了 `mode` 时，其余四项仍要拿到兜底
/// （整条二选一会让一份只补了一个键的 `server.json` 把其它兜底全抹掉）。
pub fn resolve(server: &RawCaps, bundled: Option<&CatalogModel>) -> Capability {
    Capability {
        product_excluded: server
            .product_excluded
            .or_else(|| bundled.map(|b| b.product_excluded))
            .unwrap_or_default(),
        mode: server
            .mode
            .clone()
            .or_else(|| bundled.map(|b| b.mode.clone()))
            .unwrap_or_default(),
        role: server
            .role
            .clone()
            .or_else(|| bundled.map(|b| b.role.clone()))
            .unwrap_or_default(),
        requires: server
            .requires
            .clone()
            .or_else(|| bundled.and_then(|b| b.requires.clone())),
        known_issues: server
            .known_issues
            .clone()
            .or_else(|| bundled.map(|b| b.known_issues.clone()))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> CatalogModel {
        CatalogModel {
            id: "index-tts2".into(),
            product_excluded: true,
            mode: "streaming".into(),
            role: "streaming".into(),
            requires: Some(Requires { voice_ref: true }),
            known_issues: vec!["不接 voice_ref 会直接报错".into()],
        }
    }

    /// 随包清单必须能读出来 —— 它是 `include_str!` 进二进制的**我们自己的产物**，
    /// 坏了就是"能力提示集体消失"（正是本批要消灭的那种静默降级）。
    #[test]
    fn bundled_catalog_parses_and_has_the_capability_keys() {
        let cat = catalog().expect("随包能力清单必须能解析");
        assert!(!cat.models.is_empty(), "随包能力清单不该是空的");
        let index = cat.find("index-tts2").expect("index-tts2 要在清单里");
        assert!(
            index.requires.as_ref().is_some_and(|r| r.voice_ref),
            "index-tts2 的 requires.voice_ref 是产品层硬要求，必须随包带着 —— \
             丢了它，界面上就不会拦「该引擎必须提供参考音频」"
        );
        assert!(
            !index.known_issues.is_empty(),
            "known_issues 要随包带着（只读展示）"
        );
        assert!(
            cat.find("audio8-tts-01b")
                .is_some_and(|m| m.product_excluded),
            "被产品层排除的模型必须随包标记，否则它会被当成可选项"
        );
        // 判定走唯一入口 `Capability::is_streaming_only`，别在测试里另写一份口径
        let stream = resolve(&RawCaps::default(), cat.find("audio8-tts-stream"));
        assert!(stream.is_streaming_only(), "流式专用模型必须随包标记");
        assert!(
            cat.find("不存在的模型").is_none(),
            "不在清单里的 id 要如实返回 None（不是报错）"
        );
    }

    /// 服务端**没写**任何能力字段 → 全部回落随包清单。
    #[test]
    fn server_omitting_everything_falls_back_to_the_bundled_catalog() {
        let c = resolve(&RawCaps::default(), Some(&bundled()));
        assert!(c.product_excluded, "没写就回落随包清单");
        assert_eq!(c.mode, "streaming");
        assert_eq!(c.role, "streaming");
        assert!(c.requires_voice_ref(), "requires 必须回落");
        assert_eq!(c.known_issues_note(), "不接 voice_ref 会直接报错");
        assert!(c.is_streaming_only());
    }

    /// 服务端**逐字段**优先：写了的用服务端的，没写的仍旧回落。
    ///
    /// 阳性对照：把 `resolve` 改成"服务端任意一项存在就整条用服务端" → 本用例红
    /// （role / requires / known_issues 会掉成默认）。
    #[test]
    fn server_values_win_field_by_field_and_the_rest_still_falls_back() {
        let server = RawCaps {
            mode: Some("offline".into()),
            role: Some("fast-asr".into()),
            ..Default::default()
        };
        let c = resolve(&server, Some(&bundled()));
        assert_eq!(c.mode, "offline", "服务端显式给了 mode → 用它");
        assert_eq!(c.role, "fast-asr", "服务端显式给了 role → 用它");
        assert!(
            c.requires_voice_ref(),
            "服务端没写 requires → 仍要回落随包清单（整条二选一就会在这里丢）"
        );
        assert!(c.product_excluded, "服务端没写 product_excluded → 仍要回落");
        assert_eq!(c.known_issues_note(), "不接 voice_ref 会直接报错");
    }

    /// 服务端显式写了 `false` / 空数组，也算"写了" —— 不能被随包清单盖掉。
    ///
    /// 阳性对照：把 `Option` 换成带 `default` 的裸 `bool`/`Vec` → 本用例红
    /// （"显式 false"与"没写"分不开，随包清单的 true 会盖回来）。
    #[test]
    fn explicit_false_and_empty_are_still_explicit() {
        let server = RawCaps {
            product_excluded: Some(false),
            requires: Some(Requires { voice_ref: false }),
            known_issues: Some(Vec::new()),
            ..Default::default()
        };
        let c = resolve(&server, Some(&bundled()));
        assert!(!c.product_excluded, "显式 false 必须压过随包的 true");
        assert!(!c.requires_voice_ref(), "显式 false 必须压过随包的 true");
        assert!(
            c.known_issues.is_empty(),
            "显式空数组必须压过随包的非空（否则用户删不掉随包登记的提示）"
        );
        // 没写的两项照旧回落
        assert_eq!(c.mode, "streaming");
        assert_eq!(c.role, "streaming");
    }

    /// 随包清单里没有这个 id → 只按服务端声明，不编造。
    #[test]
    fn unknown_model_has_no_bundled_fallback() {
        let c = resolve(&RawCaps::default(), None);
        assert!(!c.product_excluded);
        assert_eq!(c.mode, "");
        assert_eq!(c.role, "");
        assert!(!c.requires_voice_ref());
        assert!(c.known_issues_note().is_empty());
        assert!(!c.is_streaming_only(), "空 mode/role 不能被当成流式");
    }

    /// `mode` 与 `role` 任一说 streaming 都算流式专用（与界面判据同一处）。
    #[test]
    fn streaming_is_recognised_from_either_mode_or_role() {
        let by_mode = resolve(
            &RawCaps {
                mode: Some("STREAMING".into()),
                ..Default::default()
            },
            None,
        );
        let by_role = resolve(
            &RawCaps {
                role: Some("Streaming".into()),
                ..Default::default()
            },
            None,
        );
        assert!(by_mode.is_streaming_only());
        assert!(by_role.is_streaming_only());
    }

    /// 键名契约：随包清单的键名与 `CatalogModel` 的 serde 字段名一旦漂移，
    /// **能力提示会静默消失**（正是本批要修的失败形态），所以钉住它。
    ///
    /// 阳性对照：把 `CatalogModel::requires` 改成 `#[serde(rename = "requiresX")]`
    /// → 本用例红。
    #[test]
    fn catalog_json_keys_match_the_serde_field_names() {
        let raw = r#"{
            "version": 1,
            "models": [{
                "id": "m",
                "product_excluded": true,
                "mode": "streaming",
                "role": "scoring",
                "requires": { "voice_ref": true },
                "known_issues": ["a", "b"]
            }]
        }"#;
        let cat: Catalog = serde_json::from_str(raw).expect("能力清单要能解析");
        let m = cat.find("m").expect("按 id 找得到");
        assert!(
            m.product_excluded,
            "product_excluded 漂移就会静默变成 false"
        );
        assert_eq!(m.mode, "streaming", "mode 漂移 → 流式模型会当成离线可选");
        assert_eq!(m.role, "scoring", "role 漂移 → 打分模型当普通模型用");
        assert!(
            m.requires.as_ref().is_some_and(|r| r.voice_ref),
            "requires.voice_ref 漂移 → 界面不再拦「必须提供参考音频」"
        );
        assert_eq!(
            m.known_issues,
            vec!["a", "b"],
            "known_issues 漂移 → 提示静默消失"
        );
        // requires 的**内层**键名也要钉：只有 `voice_ref` 这个拼法算数。
        // 换别的拼法必须表现为"没解析到"，不能被当成"有要求"（方向相反的一半）。
        let wrong_key = r#"{"models":[{"id":"m","requires":{"voice_refX":true}}]}"#;
        let m: CatalogModel = serde_json::from_str::<Catalog>(wrong_key)
            .unwrap()
            .models
            .remove(0);
        assert!(
            !m.requires.as_ref().is_some_and(|r| r.voice_ref),
            "requires 的内层键名必须逐字是 voice_ref：写错要「没解析到」，不能凭空变成有要求"
        );
    }

    /// 服务端原文的键名契约：只认 `requires` / `voice_ref` 的正确拼法。
    ///
    /// 阳性对照：把 `RawCaps::requires` 改成 `#[serde(rename = "requiresX")]`
    /// → 本用例红（服务端显式值被当成没写，回落会把结果改掉）。
    #[test]
    fn raw_caps_read_back_the_server_document_verbatim() {
        let raw = r#"{"models":[
            {"id":"a","requires":{"voice_ref":true}},
            {"id":"b","product_excluded":false,"known_issues":[]},
            {"id":"c"}
        ]}"#;
        let doc: RawServerDoc = serde_json::from_str(raw).expect("服务清单原文要能解析");
        assert_eq!(
            doc.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "id 也要一起读出来（RawCaps 是挂在每条记录上的）"
        );

        let a = &doc.models[0].caps;
        assert!(a.requires.is_some(), "服务端显式的 requires 必须被读到");
        assert!(resolve(a, None).requires_voice_ref());

        let b = &doc.models[1].caps;
        assert_eq!(
            b.product_excluded,
            Some(false),
            "显式 false 要读成 Some(false)"
        );
        assert_eq!(
            b.known_issues,
            Some(Vec::new()),
            "显式空数组要读成 Some([])（与「没写」不同）"
        );

        let c = &doc.models[2].caps;
        assert_eq!(c.product_excluded, None, "没写就读成 None");
        assert!(c.requires.is_none(), "没写就读成 None");
        assert_eq!(c.known_issues, None, "没写就读成 None");
    }

    /// `RawCaps` 真的能挂在模型记录上（用 `#[serde(flatten)]` 读同一层的键）。
    #[derive(serde::Deserialize)]
    struct RawServerDoc {
        #[serde(default)]
        models: Vec<RawServerModel>,
    }

    #[derive(serde::Deserialize)]
    struct RawServerModel {
        #[serde(default)]
        id: String,
        #[serde(flatten)]
        caps: RawCaps,
    }
}
