#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""audio.model-fetch —— 基础模型下载器（M1 免费边界：上游源拉取 + 手动路径）。

不另造下载逻辑：包装上游 audio.cpp 的 tools/model_manager_v2.py
（model_specs/*.json 是下载链接的 source of truth，CHARTER §6）。
产品这层只做三件事：
  1) 产品模型 id → 上游 family/precision 的映射（读 config/models.schema.yaml）
  2) 下载落点对照 schema path 校验（缺文件如实报）
  3) 打印 server.json 手动路径配置片段（免费下载器的另一半交付）

用法:
  python3 tools/model_fetch.py --list                  # 产品登记的模型 + 本机落点状态
  python3 tools/model_fetch.py audio8-tts              # 下载（上游 family 默认档）
  python3 tools/model_fetch.py audio8-tts --precision q4_0
  python3 tools/model_fetch.py --all                   # 产品登记的全部下载

环境:
  AUDIOCPP_DIR   audio.cpp 上游 checkout 根
                 （默认：<repo>/../audio.cpp；引擎约定见 CHARTER §6）

红线（CHARTER §5）：只做下载器，不打包权重——权重始终从上游源在用户机器上拉。
"""
import argparse
import os
import re
import subprocess
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
SCHEMA = os.path.join(REPO_ROOT, "config", "models.schema.yaml")

try:
    import yaml
except ImportError:  # 与 audio_config.py 同一兜底
    sys.exit("需要 PyYAML：pip3 install pyyaml")


def load_schema():
    with open(SCHEMA, encoding="utf-8") as f:
        return yaml.safe_load(f)


def find_upstream():
    """上游 checkout：环境变量优先，其次 <repo>/../audio.cpp。
    repo 根用 config/models.schema.yaml 当锚点向上找（worktree 里 REPO_ROOT 会偏两级）。"""
    env = os.environ.get("AUDIOCPP_DIR")
    if env:
        mgr = os.path.join(env, "tools", "model_manager_v2.py")
        specs = os.path.join(env, "model_specs")
        if os.path.isfile(mgr) and os.path.isdir(specs):
            return env
        sys.exit(f"AUDIOCPP_DIR={env} 下没有 tools/model_manager_v2.py + model_specs/")

    # worktree 里 config/ 是共享的，锚点会逐层命中——取最外层命中的目录
    # （那才是真正与 audio.cpp 同级的仓库根）
    root, outer = REPO_ROOT, None
    while root != os.path.dirname(root):
        if os.path.isfile(os.path.join(root, "config", "models.schema.yaml")):
            outer = root
        root = os.path.dirname(root)
    if outer is None:
        sys.exit(f"锚点 config/models.schema.yaml 不在 {REPO_ROOT} 的任何祖先目录")
    cand = os.path.join(os.path.dirname(outer), "audio.cpp")
    mgr = os.path.join(cand, "tools", "model_manager_v2.py")
    specs = os.path.join(cand, "model_specs")
    if os.path.isfile(mgr) and os.path.isdir(specs):
        return cand
    sys.exit(
        "没找到 audio.cpp 上游 checkout（需要 tools/model_manager_v2.py + model_specs/）。\n"
        "  设置 AUDIOCPP_DIR=/path/to/audio.cpp，或把上游 clone 到本仓库同级目录。"
    )


def expand(path, models_root):
    return path.replace("${models_root}", models_root)


def cmd_list(schema):
    models_root = schema.get("runtime", {}).get("models_root", "")
    rows = []
    for mid, m in sorted(schema.get("models", {}).items()):
        p = expand(m.get("path", ""), models_root)
        rows.append((mid, m.get("family", "?"), m.get("task", "?"),
                     "有" if os.path.exists(p) else "缺", p))
    w = max(len(r[0]) for r in rows) if rows else 4
    print(f"{'模型 id':{w}}  {'family':14} {'task':6} 落点  schema path")
    for r in rows:
        print(f"{r[0]:{w}}  {r[1]:14} {r[2]:6} {r[3]:3} {r[4]}")
    print("\n下载单个：python3 tools/model_fetch.py <模型 id>")


def cmd_fetch(schema, upstream, model_ids, precision=None):
    models = schema.get("models", {})
    models_root = schema.get("runtime", {}).get("models_root", "")
    if not models_root:
        sys.exit("schema runtime.models_root 未配置")
    os.makedirs(models_root, exist_ok=True)

    for mid in model_ids:
        m = models.get(mid)
        if not m:
            sys.exit(f"schema 里没有模型 '{mid}'（可下载清单见 --list）")
        family = m.get("family")
        if not family:
            sys.exit(f"{mid}: schema 缺 family 字段，无法映射上游包")
        prec = precision or (m.get("precision_preference") or [None])[0]

        argv = [sys.executable, os.path.join(upstream, "tools", "model_manager_v2.py"),
                "--specs-dir", os.path.join(upstream, "model_specs"),
                "install", family, "--models-root", models_root]
        if prec:
            argv += ["--precision", str(prec)]
        print(f"==> {mid}: {' '.join(argv[1:])}", flush=True)
        rc = subprocess.run(argv).returncode
        if rc != 0:
            sys.exit(f"{mid}: 上游下载器退出码 {rc}（网络/磁盘/源问题，重跑幂等）")

        # 落点校验 + 手动路径片段
        p = expand(m.get("path", ""), models_root)
        ok = os.path.exists(p)
        print(f"  落点 {'✓' if ok else '✗'} {p}")
        if not ok:
            # 上游包布局可能和 schema path 差一级（repo 目录名），给出实际线索
            base = os.path.basename(p)
            hits = subprocess.run(["find", models_root, "-name", base, "-maxdepth", "3"],
                                  capture_output=True, text=True).stdout.strip()
            if hits:
                print(f"  提示：找到同名文件：\n{hits}\n  （schema path 与上游包布局不一致，以上面的实际路径为准）")
        snippet = {"id": mid, "family": family, "path": os.path.abspath(p) if ok else p,
                   "task": m.get("task", "")}
        if m.get("session_options"):
            snippet["session_options"] = m["session_options"]
        import json
        print("  server.json 手动路径片段（加进 ~/.local/opt/audio.cpp/server.json 的 models 数组）:")
        print("  " + json.dumps(snippet, ensure_ascii=False))


def main():
    ap = argparse.ArgumentParser(description="基础模型下载器（包装上游 model_manager_v2）")
    ap.add_argument("model", nargs="?", help="schema 里的模型 id（如 audio8-tts）")
    ap.add_argument("--list", action="store_true", help="列出产品登记的模型与落点状态")
    ap.add_argument("--all", action="store_true", help="下载产品登记的全部模型")
    ap.add_argument("--precision", help="覆盖 precision_preference（如 q4_0）")
    a = ap.parse_args()

    schema = load_schema()
    if a.list or (not a.model and not a.all):
        cmd_list(schema)
        return
    ids = list(schema.get("models", {})) if a.all else [a.model]
    cmd_fetch(schema, find_upstream(), ids, precision=a.precision)


if __name__ == "__main__":
    main()
