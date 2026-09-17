#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从 config/models.schema.yaml 生成随包分发的**能力清单** config/model-capabilities.json。

## 为什么要有它（issue cc-ai-audio-workshop-capability-fallback）
`role` / `requires` / `known_issues` / `product_excluded` / `mode` 是**产品侧**的模型元数据：
服务端 `app/server/config.cpp` 只 `find` 它认识的键，从不读这几个 —— 只有 App 在读。
`tools/audio_config.py render --write` 会把它们透传进 server.json，但那一步**覆盖用户的
服务配置**（按规则要授权），不能把"看到能力提示"变成用户必须先做的手工步骤。

于是同一份 schema 走两条投递路径：

- `tools/audio_config.py render --write` → 用户自己的 server.json（显式、可覆盖，可选动作）；
- 本脚本 → 随包 `config/model-capabilities.json`（`include_str!` 打进二进制，兜底）。

App 读 server.json 时**逐字段**回落：服务端显式给了就以它为准，没给才用随包清单。

## 口径：同一份投影，不另写一份
这里产出的 5 个键就是 `tools/audio_config.py::to_server()` 会写进 server.json 的那 5 个 ——
本脚本**直接调 `to_server()`**，只把结构性键（id/family/path/task/session_options）剔掉，
所以"render 会写什么"与"随包清单兜什么"不可能各说各话。
缺省值在这里物化（`mode=offline` / `product_excluded=false` / `role=""` /
`requires=null` / `known_issues=[]`），产物是一份**完整的能力声明**，而不是"没写的自己猜"。

## 约定（与 tools/gen_model_downloads.py 一致）
- 键序固定（`sort_keys`）+ 固定缩进 + 行尾换行 → 同输入两次运行逐字节相同；
- 模型顺序沿用 schema（= server.json 的顺序：默认引擎取"第一个不要参考音的"）；
- `--check` 只重算不写盘，与盘上的文件比对，不一致退出 1（改了 schema 忘了重生成）；
- **纯离线**：不联网、不读 server.json、不碰用户配置、不依赖 models_root
  （能力投影里没有任何路径，所以换机器产物不变）。
"""

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
sys.path.insert(0, str(HERE))

import audio_config  # noqa: E402  （唯一的能力投影来源，见模块开头）

OUT_PATH = REPO_ROOT / "config" / "model-capabilities.json"

# to_server() 里属于"结构性字段"的键（服务加载模型要用），不进能力清单。
STRUCTURAL_KEYS = ("id", "family", "path", "task", "session_options")
# 能力字段：schema → server.json → App，也是 server.json 里**只有 App 在读**的那几个。
CAPABILITY_KEYS = ("mode", "product_excluded", "role", "requires", "known_issues")
# 缺省物化：与 Rust 侧 `ServerModel` 的 serde 缺省**同义**（没声明 = 这些值）。
DEFAULTS = {
    "mode": "offline",
    "product_excluded": False,
    "role": "",
    "requires": None,
    "known_issues": [],
}


def capability_of(entry: dict) -> dict:
    """一条 `to_server()` 记录 → 5 个能力字段（缺省物化）。"""
    return {k: entry.get(k, DEFAULTS[k]) for k in CAPABILITY_KEYS}


def build(cfg: dict) -> dict:
    return {
        "version": 1,
        "models": [
            {"id": e["id"], **capability_of(e)} for e in audio_config.to_server(cfg)["models"]
        ],
    }


def render(payload: dict) -> str:
    """固定键序 + 固定缩进 + 行尾换行 = 逐字节稳定。"""
    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description="生成随包分发的模型能力清单")
    ap.add_argument("--check", action="store_true", help="只校验盘上的清单是否与 schema 一致，不写盘")
    ap.add_argument("--config", default=audio_config.CFG_DEFAULT, help="schema 路径")
    ap.add_argument("--out", default=str(OUT_PATH), help=f"输出路径（默认 {OUT_PATH}）")
    a = ap.parse_args()

    text = render(build(audio_config.load(a.config)))
    out = Path(a.out)

    if a.check:
        current = out.read_text(encoding="utf-8") if out.exists() else ""
        if current == text:
            print(f"OK：{out} 与 {a.config} 一致")
            return 0
        print(
            f"STALE：{out} 与 {a.config} 不一致，请重跑 {Path(__file__).name}",
            file=sys.stderr,
        )
        return 1

    out.write_text(text, encoding="utf-8")
    payload = json.loads(text)
    n_excluded = sum(1 for m in payload["models"] if m["product_excluded"])
    n_required = sum(1 for m in payload["models"] if m["requires"])
    print(
        f"已写出 {out}：{len(payload['models'])} 个产品模型，"
        f"其中 {n_excluded} 个被产品层排除、{n_required} 个有硬要求"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
