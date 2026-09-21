# AGENTS.md

## 项目简介
音频作坊 · audio-workshop —— 本地优先的音频工作台（配音 / BGM / 音乐制作三主线）。
立项依据、实测基线、风险与路线图见 `CHARTER.md`，用法与目录说明见 `README.md`。

## 通用规则
遵循 `~/.agents/rules/` 下的通用规则（开发流程、开发规范、提交规范、合并规范、经验沉淀等）；本文件为仓库级规则，冲突时以本文件为准。

## 构建与验证
- 构建/测试/打包脚本见 `package.sh` / `release.sh` 与 `README.md`；引擎随包版本以 `engine-lock.json` 为准。
- 改动涉及模型配置时同步 `config/models.schema.yaml` 与生成物 `config/model-capabilities.json`（`tools/gen_model_capabilities.py --check` 校验）。
- 关键决策先更新 `docs/` 对应文档再写代码，契约不漂移。
- 源码守卫读 `include_str!` 一律先 `src_lf` 归一、窗口用字符安全切片（`source_window`）；仓库以 `.gitattributes` 强制源码 LF——CRLF 检出下裸 `&s[at..at+N]` 会切进多字节字符（2026-09-21 windows-latest 实测）。

## 协作与记账（walgit 协同层）
本仓库 origin 为本机 walgit（`http://127.0.0.1:8081/gqf2008/audio-workshop.git`，以 `git remote get-url origin` 实测为准）。开发协作的 issue / PR / 评审 / 合并 / 收尾一律走 walgit 的 D1 协同层（`refs/collab/*` 追加式签名条目），**不使用 GitHub / `gh`**：

- 开线程 `issue`（`--parent ""`）→ 认领 `comment`（owner/worktree/branch）+ `status: in-progress` → 交补丁 `patch --base refs/heads/main --head refs/heads/<branch>` → 评审 `review`（decision/agent/note）→ 合并 `merge_result` → 收尾 `status: closed`。
- `--parent` 必须取**追加前重查的真实链尾**（`collab thread` 末条 oid；任务书里的 oid 只是派工快照，过期即弃）；`--key` 传路径不传内容；**不得用别人的 key 代签**。
- 同一 (线程, principal) 唯一写者；派发脚本幂等（唯一批次名 + pid 查重）；临时目录用 `/Volumes/DataExt/tmp/<batch>-<唯一后缀>/` 独占，见 `~/.agents/rules/RULE_walgit协同记账.md` 第 3/8/9 条。
- 查重/看板用 `walgit collab board` / `collab thread <id>`，不用 `gh issue list`。
- 命令形态与 principal/key 约定见 `~/.agents/rules/RULE_walgit协同记账.md`。

