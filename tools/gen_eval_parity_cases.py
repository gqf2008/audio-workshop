#!/usr/bin/env python3
"""生成 aw-core 质检模块的中文数字归一 parity 用例。

为什么单独一份：`tools/gen_parity_cases.py`（文本层）的权威实现在 aw-eval 仓库，
而**质检工具 audio_eval.py 本身就在本仓库**维护，两者来源不同、不能混在一个夹具里。

权威实现 = 本仓库 `tools/audio_eval.py::normalize_numerals`
（Rust 侧 `aw_core::eval::normalize_numerals` 必须逐条一致——ASR 常把 2026 回读成
「二零二六」，规则漂移就会把读法差异算成错误）。

用法:
  python3 tools/gen_eval_parity_cases.py          # 写入夹具
  python3 tools/gen_eval_parity_cases.py --check  # 只校验是否与权威实现一致
"""
import argparse, hashlib, importlib.util, os, sys

HERE = os.path.dirname(os.path.realpath(__file__))
REPO = os.path.dirname(HERE)
OUT = os.path.join(REPO, "crates", "aw-core", "tests", "eval_parity_cases.txt")
AUTHORITY = os.path.join(HERE, "audio_eval.py")

# 覆盖点：逐字读 / 带单位 / 小数 / 混合文本 / 非数字串 / 边界
CASES = [
    "二零二六",
    "二〇二六",
    "幺三八",
    "十七",
    "一百二十三",
    "三千五百万",
    "三点一四",
    "两点零五",
    "今年二零二六年，共 17 人。",
    "第 3 句",
    "abc123",
    "完全没有数字",
    "一万零五",
    "十亿",
    "五点",
    "点五",
    "O五",
    "o五",
    "三更半夜",     # 夜里没有数字字符，整串原样
    "他三十五岁",   # 前后是汉字，只替换数字串
    "十",           # 单位起头（起始字符类包含单位，别只认数字字符）
    "万",
    "十万",
    "二十三点五",
]


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def esc(s):
    return (s.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
             .replace("\r", "\\r").replace("|", "\\|"))


def build():
    if not os.path.isfile(AUTHORITY):
        sys.exit(f"找不到权威实现: {AUTHORITY}")
    mod = load_module(AUTHORITY, "aw_authority_eval")
    sha = hashlib.sha256(open(AUTHORITY, "rb").read()).hexdigest()
    lines = [
        "# aw-core 质检模块 parity 用例 —— 期望值由 Python 权威实现生成，请勿手改。",
        "# 重新生成: python3 tools/gen_eval_parity_cases.py",
        "# 来源: tools/audio_eval.py（质检工具在本仓库维护）",
        f"#   sha256={sha}",
        "# 格式: '[numerals]' 之后每行 '<输入>\\t<期望输出>'（\\t \\n \\r \\| \\\\ 转义）",
        "",
        "[numerals]",
    ]
    for s in CASES:
        out = mod.normalize_numerals(s)
        assert "\t" not in out and "\n" not in out, s
        lines.append(f"{esc(s)}\t{esc(out)}")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    text = build()
    if a.check:
        old = open(OUT, encoding="utf-8").read() if os.path.exists(OUT) else ""
        if old != text:
            sys.exit(f"❌ {OUT} 与权威实现不一致，请重跑 python3 tools/gen_eval_parity_cases.py")
        print("✅ 质检 parity 夹具与 tools/audio_eval.py 一致")
        return
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    open(OUT, "w", encoding="utf-8").write(text)
    n = sum(1 for l in text.splitlines() if "\t" in l and not l.startswith("#"))
    print(f"✅ 已写入 {OUT}（{n} 条用例）")


if __name__ == "__main__":
    main()
