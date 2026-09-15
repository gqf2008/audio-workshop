#!/usr/bin/env python3
"""生成 aw-core 文本层的 parity 用例（Rust 与 Python 逐条对照的同一份期望值）。

为什么存在：Rust 侧此前的单测全是"同规则 happy path"，没有一条与 Python 对照，
规则一旦漂移（本次实测 20 例差异）无人发现。这里把输入-输出对**由 Python 权威实现
现场算出后固化成数据**，Rust 侧只做断言，不再手写期望值。

权威实现 = aw-eval 侧 tools/audio_config.py + tools/audio_dub.py（分支 fix/eval-tn-convergence，
含 fix/text-layer-and-dataloss）。注意 aw-core 仓库内 tools/ 下那份是**旧拷贝**，不要用它生成。

用法:
  python3 tools/gen_parity_cases.py                 # 写入 crates/aw-core/tests/parity_cases.txt
  python3 tools/gen_parity_cases.py --check         # 只校验与已固化文件是否一致（CI 用）
  AW_PY_TOOLS=/path/to/aw-eval/tools python3 tools/gen_parity_cases.py
"""
import argparse, hashlib, importlib.util, os, sys

HERE = os.path.dirname(os.path.realpath(__file__))
REPO = os.path.dirname(HERE)
OUT = os.path.join(REPO, "crates", "aw-core", "tests", "parity_cases.txt")

# ── 覆盖点 ──────────────────────────────────────────────────────────────
# 每条都对应一类真实踩过的差异；新增规则时必须在这里加正/反例，否则漂移不会被发现。
VERBALIZE = [
    # 千分位：只删真千分位（否则 共3,4人 → 共三十四人，语义错）
    "1,234", "12,345,678", "1,234.56", "3,000元", "共3,4人", "今天天气不错,我们去公园吧。",
    "1,2345", "12,34", "A,B", "1,2,3", "第1,234次",
    # 年份：无空格年份 / 非 19xx-20xx 年份
    "2026年", "2026 年", "2026　年", "今年2026年", "1999年", "1899年", "2100年",
    "2026年3月", "从2026年到2027年第3季度",
    # 小数
    "3.14", "1234.56", "0.5", "007.5", "3.5.1",
    # 百分比（含空格）
    "30%", "30 %", "百分之30", "增长了12.5%",
    # 金额
    "¥1234.56", "￥ 88", "1234.56元", "1234.56 元", "价格 1,234.56 元，占 30 %。",
    # 电话（幺）
    "13812345678", "13812345678号", "1381234567", "23456789012",
    # 长数字串（12/13/16 位：订单号/卡号逐位读，不按数量）
    "123456789012", "1234567890123", "1234567890123456", "订单号1234567890123",
    "[1234567890123456]", "12345678901234567.5",
    # 拉丁相邻（iPhone15 / MP3 / A4纸 / ISO9001）——改边界后必须显式排除
    "iPhone15", "MP3", "A4纸", "ISO9001", "USB3.0", "abc123def", "_123",
    # 汉字夹数字
    "第3次", "共3人", "3个", "我今年2026年3月5日出生", "温度3.5度",
    # 基数词边界
    "0", "10", "17", "100", "10000", "10086", "100000000", "1000000000000",
    # 规则优先级（顺序错就会漂移：金额 > 电话 > 小数 > 百分比 > 年份 > 长串 > 基数词）
    "13812345678元", "1234567890123元", "1234567890123%", "12.5%", "13812345678.5",
    "1234567890123.5", "12345678901234567890123",
    # 金额走基数词时 >u64::MAX 的串：旧实现 parse().unwrap_or(0) 会静默读成「零」
    "¥99999999999999999999", "99999999999999999999元",
    # 规则失败时的退化路径（右边界/左边界挡下后退回基数词或不改）
    "1234.56abc", "abc1234.5", "2026abc年", "20265年", "2026 5年", "1234abc",
]

# 切句：Python audio_dub.split_sentences(text, {"punctuation":…, "max_chars":…})
SPLIT = [
    ("。！？；…", 80, [
        "第一句。第二句！第三句？",
        "只有一句没有句末标点",
        "多句连排。第二句；第三句…",
        "含  \t 多余空白。第二句。",
    ]),
    # 超长按逗号断；句首逗号不得切出空句（cut<=0 → 保留整句）
    ("。！？；…", 12, [
        "这是一个很长的句子，需要按逗号断开，才能保证每次请求不会太长。",
        "，开头就是逗号而且很长很长很长很长很长的句子。",
        "没有逗号可以断的超长句子于是只能整句保留下来。",
        "短，但有顿号、和逗号，按最靠后的断点切。",
        "第一句。第二句很长很长很长，长到必须断开才行。",
    ]),
    ("。！？", 8, [
        "逗号，在第八个字符之后，才会出现。",
        "短句，切。",
    ]),
]


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def esc(s):
    """转义分隔符与反斜杠/空白，保证 '<输入>\\t<期望>' 可被逐行精确还原"""
    return (s.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
             .replace("\r", "\\r").replace("|", "\\|"))


def build(py_dir):
    cfg_path = os.path.join(py_dir, "audio_config.py")
    dub_path = os.path.join(py_dir, "audio_dub.py")
    for p in (cfg_path, dub_path):
        if not os.path.isfile(p):
            sys.exit(f"找不到权威实现: {p}（用 AW_PY_TOOLS 指定 aw-eval/tools 目录）")
    cfg = load_module(cfg_path, "aw_authority_config")
    dub = load_module(dub_path, "aw_authority_dub")
    sha = hashlib.sha256(open(cfg_path, "rb").read()).hexdigest()

    lines = [
        "# aw-core 文本层 parity 用例 —— 期望值由 Python 权威实现生成，请勿手改。",
        "# 重新生成: python3 tools/gen_parity_cases.py",
        "# 来源: aw-eval tools/audio_config.py",
        f"#   sha256={sha}",
        "# 格式: '[section]' 开头，其后每行 '<输入>\\t<期望输出>'；\\t \\n \\r \\| \\\\ 为转义。",
        "#   [verbalize]                    → aw_core::verbalize",
        "#   [split|标点|max_chars]          → aw_core::split_sentences，期望句以 '|' 连接",
    ]
    lines.append("")
    lines.append("[verbalize]")
    for s in VERBALIZE:
        out = cfg.verbalize(s)
        assert "\t" not in out and "\n" not in out, s
        lines.append(f"{esc(s)}\t{esc(out)}")
    for punct, max_chars, texts in SPLIT:
        lines.append("")
        lines.append(f"[split|{punct}|{max_chars}]")
        for t in texts:
            sents = dub.split_sentences(t, {"punctuation": punct, "max_chars": max_chars})
            assert "|" not in punct, punct
            lines.append(f"{esc(t)}\t{esc('|'.join(sents))}")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--py-dir", default=os.environ.get("AW_PY_TOOLS",
                    os.path.join(os.path.dirname(REPO), "aw-eval", "tools")))
    ap.add_argument("--check", action="store_true", help="只校验已固化文件与权威实现是否一致")
    a = ap.parse_args()
    text = build(a.py_dir)
    if a.check:
        old = open(OUT, encoding="utf-8").read() if os.path.exists(OUT) else ""
        if old != text:
            sys.exit(f"❌ {OUT} 与权威实现不一致，请重跑 python3 tools/gen_parity_cases.py")
        print(f"✅ parity 用例与权威实现一致（{a.py_dir}）")
        return
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    open(OUT, "w", encoding="utf-8").write(text)
    n = sum(1 for l in text.splitlines() if "\t" in l and not l.startswith("#"))
    print(f"✅ 已写入 {OUT}（{n} 条用例，来源 {a.py_dir}）")


if __name__ == "__main__":
    main()
