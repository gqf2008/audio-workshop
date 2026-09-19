#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""检查 Windows 可执行文件里**真的有图标资源**、且**不是控制台子系统**。

用法：python tools/check_windows_icon.py dist/windows-x64/audio-workshop.exe
      python tools/check_windows_icon.py dist/AudioWorkshop-0.1.3-windows-x64-setup.exe

为什么需要它：图标是**用户可见**的要求，而"没嵌上"不会让任何构建步骤失败 ——
build.rs 少跑一次、rc.exe 找不到、安装器忘了 SetupIconFile，产出的都是"能装能跑、
只是空白图标"的包。2026-09-19 真机反馈正是这两处（exe 没图标 + 桌面快捷方式没图标）。
所以发版前对**真产物**做一次结构断言：PE 资源目录里必须有 RT_GROUP_ICON（类型 14）与
RT_ICON（类型 3）。

同一条反馈里还有"启动多一个控制台黑窗"：Rust 默认 console 子系统，GUI 应用必须声明
`windows_subsystem = "windows"`。这条同样是"没人报错、只有用户看得见"，所以一并断言
（可选头里的 Subsystem 必须是 2 = WINDOWS_GUI，不是 3 = WINDOWS_CUI）。

为什么不用"能不能提取出图标"来判：Windows 的 `ExtractAssociatedIcon` 在**没有**图标时
会返回系统默认图标，照样"成功"——那是假绿。这里直接读 PE 的资源目录，没有就是红。

本脚本只看结构、不依赖 Windows：在任何平台上都能对拿到的 .exe 跑（对照见
tools/tests/test_check_windows_icon.py）。
"""

import argparse
import struct
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import _console  # noqa: F401  副作用 import：stdout→UTF-8（Windows 上必须）

# 资源类型：windows.h 里的 RT_ICON / RT_GROUP_ICON
RT_ICON = 3
RT_GROUP_ICON = 14
RT_NAMES = {RT_ICON: "RT_ICON", RT_GROUP_ICON: "RT_GROUP_ICON"}

# 可选头里的 Subsystem 字段（PE32/PE32+ 都在同一偏移）
SUBSYSTEM_GUI = 2
SUBSYSTEM_CUI = 3
SUBSYSTEM_NAMES = {SUBSYSTEM_GUI: "GUI(2)", SUBSYSTEM_CUI: "控制台(3)"}


class CheckError(Exception):
    """产物不合格（不是脚本自身用错）。"""


def _u16(b, off):
    return struct.unpack_from("<H", b, off)[0]


def _u32(b, off):
    return struct.unpack_from("<I", b, off)[0]


def _rva_to_offset(sections, rva):
    for va, vsize, raw_size, raw_ptr in sections:
        if va <= rva < va + max(vsize, raw_size):
            return raw_ptr + (rva - va)
    return None


def _pe_layout(data):
    """校验 PE 头并返回 (可选头偏移, 节表偏移, 资源目录文件偏移)。

    抛 CheckError：不是 PE。
    """
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise CheckError("不是 PE 文件（缺 MZ 头）")
    pe_off = _u32(data, 0x3C)
    if data[pe_off : pe_off + 4] != b"PE\0\0":
        raise CheckError("不是 PE 文件（缺 PE\\0\\0 签名）")
    n_sections = _u16(data, pe_off + 6)
    opt_size = _u16(data, pe_off + 20)
    opt_off = pe_off + 24
    magic = _u16(data, opt_off)
    if magic == 0x10B:  # PE32
        dir_off = opt_off + 96
    elif magic == 0x20B:  # PE32+
        dir_off = opt_off + 112
    else:
        raise CheckError(f"未知的可选头 magic 0x{magic:04x}")
    # DataDirectory[2] = 资源表
    res_rva = _u32(data, dir_off + 2 * 8)
    res_size = _u32(data, dir_off + 2 * 8 + 4)
    sec_off = opt_off + opt_size
    sections = []
    for i in range(n_sections):
        s = sec_off + i * 40
        vsize = _u32(data, s + 8)
        va = _u32(data, s + 12)
        raw_size = _u32(data, s + 16)
        raw_ptr = _u32(data, s + 20)
        sections.append((va, vsize, raw_size, raw_ptr))
    res_off = None
    if res_rva != 0 and res_size != 0:
        res_off = _rva_to_offset(sections, res_rva)
        if res_off is None:
            raise CheckError("资源目录的 RVA 落不进任何节")
    return opt_off, res_off


def subsystem(data):
    """可选头里的 Subsystem（PE32 与 PE32+ 都在可选头 +68）。"""
    opt_off, _res = _pe_layout(data)
    return _u16(data, opt_off + 68)


def icon_resource_counts(data):
    """返回 {资源类型: 该类型下叶子条目数}（只统计图标相关的类型）。

    抛 CheckError：不是 PE，或没有资源目录。
    """
    _opt_off, res_off = _pe_layout(data)
    if res_off is None:
        raise CheckError("PE 里没有资源目录（DataDirectory[2] 为空）")

    def subdir_entries(off):
        """目录里的条目：[(id 或 None, 名字或 None, 子偏移)]。"""
        n_named = _u16(data, off + 12)
        n_id = _u16(data, off + 14)
        out = []
        for i in range(n_named + n_id):
            e = off + 16 + i * 8
            name, offset = _u32(data, e), _u32(data, e + 4)
            if name & 0x80000000:
                # 字符串名：资源里图标都是 ID，这里如实记下名字但不当图标用
                str_off = res_off + (name & 0x7FFFFFFF)
                n = _u16(data, str_off)
                out.append((None, data[str_off + 2 : str_off + 2 + n * 2].decode("utf-16-le", "replace"), offset))
            else:
                out.append((name, None, offset))
        return out

    counts = {}
    for type_id, _type_name, type_off in subdir_entries(res_off):
        if type_id not in RT_NAMES or not (type_off & 0x80000000):
            continue
        # 类型 → 名字/ID → 语言：叶子数量就是这个类型真正占了几条资源
        leaves = 0
        for _nid, _nname, name_off in subdir_entries(res_off + (type_off & 0x7FFFFFFF)):
            if not (name_off & 0x80000000):
                continue  # 叶子（直接是数据条目）也算一条
            leaves += len(subdir_entries(res_off + (name_off & 0x7FFFFFFF)))
        counts[type_id] = leaves
    return counts


def check(path, expect_gui=True):
    """返回 (是否通过, 说明)。

    `expect_gui`：产物必须是 GUI 子系统（双击启动不该弹控制台黑窗）。
    """
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as e:
        return False, f"读不到 {path}：{e}"
    try:
        sub = subsystem(data)
        counts = icon_resource_counts(data)
    except CheckError as e:
        return False, f"{path}：{e}"
    groups = counts.get(RT_GROUP_ICON, 0)
    images = counts.get(RT_ICON, 0)
    sub_text = SUBSYSTEM_NAMES.get(sub, str(sub))
    detail = "、".join(f"{RT_NAMES[k]} ×{v}" for k, v in sorted(counts.items())) or "一个图标资源都没有"
    if groups == 0 or images == 0:
        return False, f"{path}：{detail}、子系统 {sub_text} —— 缺图标资源（exe 图标/快捷方式图标都会是空白）"
    if expect_gui and sub != SUBSYSTEM_GUI:
        return False, (
            f"{path}：子系统 {sub_text} —— 不是 GUI 子系统，双击/开始菜单启动会多一个"
            "控制台黑窗（Rust 要 `#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]`）"
        )
    return True, f"{path}：{detail}、子系统 {sub_text}"


def main():
    ap = argparse.ArgumentParser(description="检查 Windows exe 的图标资源与子系统")
    ap.add_argument("exe", nargs="+", help="要检查的 .exe（可给多个）")
    ap.add_argument(
        "--allow-console",
        action="store_true",
        help="允许控制台子系统（默认不允许：release 产物不该弹控制台）",
    )
    args = ap.parse_args()
    bad = 0
    for path in args.exe:
        ok, note = check(path, expect_gui=not args.allow_console)
        print(("✅ " if ok else "❌ ") + note)
        if not ok:
            bad += 1
    if bad:
        print(
            f"\n{bad} 个产物没通过。修法：图标 —— 确认 build.rs 的 embed_windows_icon 真的跑了"
            "（Windows 目标）、assets/icon.ico 存在，安装器侧还要 SetupIconFile + [Icons] 的"
            " IconFilename；子系统 —— 确认 src/main.rs 顶部的 windows_subsystem 属性还在、"
            "且这次是 release 构建。",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
