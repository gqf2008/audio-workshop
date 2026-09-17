# 模型能力清单：App 的兜底来源（`config/model-capabilities.json`）

`role` / `requires` / `known_issues` / `product_excluded` 这四个字段是**纯产品侧**的模型元数据：
服务端 (`app/server/config.cpp`) 只 `find` 它认识的键，**这四个一个都不读**，只有 App 在读。

> **`mode` 是例外，单列**：服务端**也读它**（`config.cpp` 里 `optional_string(item, "mode", …)`），
> 并在 `runtime.cpp` 强制「streaming / live 必须 `mode=streaming`」。这里把它一起兜底，
> 方向只会**更保守**——服务端缺省是 `offline`，随包清单说 `streaming` 时 App 选择把该模型
> 从可选引擎里藏起来；不会反过来放出服务端会拒的引擎。

这几个字段决定界面上的三件事：

- 哪些模型**不出现在**可选引擎里（`product_excluded`：选了会静默产出听不懂的音频）；
- 哪个引擎**必须**给参考音频才能开工（`requires.voice_ref`：不给就 500）；
- 引擎行下面那句**只读的已知问题**（`known_issues`）。

## 两条投递路径（同一份 schema）

`config/models.schema.yaml` 是这些决策的**唯一作者**，投递到 App 有两条路：

| 来源 | 谁写 | 优先级 | 什么时候需要 |
|---|---|---|---|
| `server.json` 里那几个键 | 用户 / `python3 tools/audio_config.py render --write` | **高**（显式给了就以它为准） | 想覆盖内网镜像、临时复测某个模型时 |
| `config/model-capabilities.json`（随包，`include_str!` 打进二进制） | `python3 tools/gen_model_capabilities.py` | 低（服务端没给才用） | **默认就有**，不需要用户做任何事 |

**`render --write` 因此是可选的**：它继续存在（想让服务配置也带上这些元数据时跑），
但不再是"看到能力提示"的前提。改前它必须跑一次，而它会**覆盖用户的服务配置**
（按 `RULE_未经授权禁止删除文件` 属于需要授权的操作）—— 不该把"界面说真话"变成
用户必须先手工做的一步。

## 逐字段回落，不是整条二选一

一份只补了 `mode` 的 `server.json`，其余四项仍要拿到兜底；所以判定是**按字段**做的：

```text
生效值(字段) = server.json 显式给了 ? 服务端的 : (随包清单有 ? 随包的 : 未声明默认)
```

判据必须来自**原文**：serde 的 `default` 会把"服务端写了 `false`"和"服务端没写"
抹成同一个值，所以 `ServerModel` 的已解析值推不出"服务端到底写没写"。
代码里因此是两个形状（`src/model_capabilities.rs`）：

- `RawCaps` —— 服务端原文的能力字段，全是 `Option`（`None` = 没写）；
- `Capability` —— **生效值**，由 `resolve(server, bundled)` 这**唯一一处**产出。

`parse_server_config` 是解析 `server.json` 的唯一入口（原始形状 → 生效形状），
`discover_engine` / `read_server_config` 都走它，不存在第二份合并判断
（见 `LESSON_同一语义两处实现必然漂移回显需与真实行为同源`）。

## 生成与校验

```sh
python3 tools/gen_model_capabilities.py            # 重新生成（纯离线，不联网、不读 server.json）
python3 tools/gen_model_capabilities.py --check    # 只校验盘上的清单与 schema 一致（不一致退出 1）
python3 tools/tests/test_gen_model_capabilities.py # 7 条：投影不漏键 / 键集 / 缺省口径 / 可复现 / --check
```

- 口径与 `tools/audio_config.py::to_server()` **同一份投影**（脚本直接调它，只剔掉
  `id`/`family`/`path`/`task`/`session_options` 这些结构性键），所以"render 会写什么"
  与"随包清单兜什么"不可能各说各话；
- 缺省值**物化**（`mode=offline` / `product_excluded=false` / `role=""` /
  `requires=null` / `known_issues=[]`），产物是一份**完整的能力声明**；
- 键序固定 + 固定缩进 + 行尾换行 → 同输入两次运行**逐字节相同**；模型顺序沿用 schema；
- 纯离线且**与 `models_root` 无关**（投影里没有任何路径，换机器产物不变）。

## 真机对照（本机实际 `server.json`，一个字节都没改）

`AW_UI_STATE=engines` 把实际解析结果打到 stderr。同一台机、同一份
`~/.local/opt/audio.cpp/server.json`（sha256 `2e317598…`，mtime 2026-09-15 18:08:01 全程未变）：

**改前（= 把随包兜底拆掉，复刻旧行为）**

```text
  [0] audio8-tts  · 要求参考音=false · 默认选中=true  · known_issues=""
      开工判据：Ready
  [1] index-tts2  · 要求参考音=false · 默认选中=false · known_issues=""
      开工判据：Ready            ← 不误禁，但也不提示；用户按 README 用内置音色合成 → 每句 500
```

**现在**

```text
  [0] audio8-tts  · 要求参考音=false · 默认选中=true
      known_issues="数字/电话/金额读法不稳定，必须依赖 text.normalization；…"
      开工判据：Ready
  [1] index-tts2  · 要求参考音=true · 默认选中=false
      known_issues="不接 voice_ref 会直接报错，需在 UI 层强制选音色"
      开工判据：EngineNeedsReference   ← 主按钮禁用 + 内置音色行置灰 + 点名原因
  [不出现在下拉] audio8-tts-01b        · product_excluded=true  · mode="offline"
  [不出现在下拉] audio8-tts-stream     · mode="streaming" · role="streaming"
  [不出现在下拉] audio8-tts-01b-stream · product_excluded=true  · mode="streaming" · role="streaming"
```

（最后两行的 `role="streaming"` 也来自随包清单：本机 `server.json` 里没有 `role` 这个键。）

**反向对照**：把同一份 schema 渲染到**临时副本**（`AW_SERVER_CONFIG` 指过去，用户文件不动）
再跑一次 `engines`，两份 dump **逐行无差异** —— 两条投递路径同源，不会各说各话。
改前这两份是不一致的（只有渲染过的那份才有 `requires`）。

`EngineNeedsReference` 就是界面上的：内置默认音色行置灰、主按钮禁用、提示
「这个引擎必须提供参考音频（不能只用内置音色）」。改前这里会放行，用户点下去才逐句吃 500。

## 回归与阳性对照

每条都实测过"故意改坏 → 转红 → 复原"（复原后四个文件 sha256 与基线逐字节一致）：

| 用例 | 阳性对照（故意改坏） | 结果 |
|---|---|---|
| `server_omitting_capabilities_still_gets_them_from_the_bundled_catalog` | `parse_server_config` 里把回落改成 `resolve(&caps, None)` | 红（`requires_voice_ref` 掉成 false） |
| `server_explicit_capabilities_win_field_by_field` | `resolve` 改成"服务端写了任意一项就整条用服务端" | 红（没补的 `requires` / `known_issues` 掉成默认） |
| `excluded_and_streaming_are_filtered_even_when_the_server_declares_nothing` | 同上（拆掉回落） | 红（`audio8-tts-01b` / `-stream` 回到下拉） |
| `server_json_keys_match_the_serde_field_names` | `RawCaps::requires` 改名 `requiresX` | 红（`requires.voice_ref` 解析不到） |
| `catalog_json_keys_match_the_serde_field_names` | `Requires::voice_ref` 改名 `voice_refX` | 红（随包清单的硬要求静默变 false） |
| `bundled_catalog_parses_and_has_the_capability_keys` | 同上 | 红（真文件里的 `requires` 解析不出来） |
| `tools/tests/test_gen_model_capabilities.py`（7 条） | 从 `CAPABILITY_KEYS` 漏掉 `known_issues` / 改缺省 `role` / 手改产物一个字节 | 分别红 4 / 3 / 3 条，`--check` 退出 1 |

**契约用例故意用不在随包清单里的 id**：用真 id 的话，键名漂移会退化成"服务端没写" →
落回随包清单 → 断言照样绿 —— 本批的兜底恰好会把漂移盖住，所以那条用例必须绕开兜底。

## 已知边界

1. **不自动改写 `server.json`**，也不改 `render --write` 本身（它继续存在，只是不再必需）。
2. **随包清单是生成物**：改了 `config/models.schema.yaml` 要重跑生成脚本；`--check` 与
   工具测试会喊 stale，忘跑不会被静默带进发布。
3. **服务端独有的模型没有兜底**（随包清单里没有那个 id）→ 只按服务端声明，不编造；
   未声明的字段就是"没声明"（不过滤、不额外要求）。
4. **`Capability` 是能力的唯一形状**：`ServerModel` 不再自己声明这几个字段，
   也删掉了原来挂在它上面的 `is_streaming_only` / `requires_voice_ref` / `known_issues_note`
   （实现搬进 `Capability`，留两份就会漂移）。
5. **随包清单坏了会如实报**（`parse_server_config` 返回带原因的 `Err`，界面显示"随包能力清单读不出来"），
   不静默降级成"没有兜底"——静默降级正是本批要消灭的失败形态。
6. **没有做**：`known_issues` 富文本/链接；App 直读 `models.schema.yaml`（仍以
   `server.json` + 随包清单两个入口为准）；按机器推荐量化档（那是
   `docs/model-download.md` 那条线）。
7. 屏幕锁着 → 没有像素级目视验证，界面部分是 `AW_UI_STATE=engines|drawer|selected` 冒烟 +
   上面那份真机 dump。
