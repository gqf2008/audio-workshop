"""`tools/check_engine_deps.py` 的确定性单测：不联网、不依赖真产物。

夹具是**手工构造的最小 PE / ELF**（只保留解析器真正读的那几处结构：
DOS+PE 头、节表、导入表描述符；ELF 头、节表、.dynamic/.dynstr/.shstrtab），
所以它验证的是"解析器读得对不对"，不是"上游这个版本碰巧长什么样"。

两边都要有阳性与阴性对照：依赖齐必须绿、少一个库必须红（否则门禁可以静默失效）。
"""
from __future__ import annotations

import importlib.util
import os
import struct
import sys
import tempfile
import unittest
from pathlib import Path

# 与 tools/tests 下其它用例同一套做法：先把自己的上层目录（tools/）放进 sys.path。
# 少了这行，被载入的脚本内部那句 `import _console` 在 Windows 上会找不到模块
# （CI 门禁 Windows job 实测红过）。
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

TOOL = Path(__file__).resolve().parents[1] / "check_engine_deps.py"
spec = importlib.util.spec_from_file_location("check_engine_deps", TOOL)
ced = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ced)


# --- 夹具构造 ------------------------------------------------------------


def build_pe(imports: list[str]) -> bytes:
    """最小 64 位 PE：一个 .rdata 节，里面是导入表描述符 + DLL 名。"""
    dos = bytearray(0x40)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 0x40)

    rva_base = 0x1000
    raw_base = 0x200
    opt_size = 240
    sec_off = 0x40 + 4 + 20 + opt_size

    # .rdata 内容：N 个描述符 + 1 个全零描述符 + 名字
    body = bytearray()
    names_off = 20 * (len(imports) + 1)
    name_rvas = []
    names_blob = bytearray()
    for name in imports:
        name_rvas.append(rva_base + names_off + len(names_blob))
        names_blob += name.encode("ascii") + b"\0"
    for rva in name_rvas:
        body += struct.pack("<IIIII", 0, 0, 0, rva, 0)
    body += b"\0" * 20
    body += names_blob

    out = bytearray()
    out += bytes(dos)
    out += b"PE\0\0"
    out += struct.pack(
        "<HHIIIHH",
        0x8664,  # machine = x86-64
        1,  # sections
        0,
        0,
        0,
        opt_size,
        0x0022,
    )
    opt = bytearray(opt_size)
    struct.pack_into("<H", opt, 0, 0x20B)  # PE32+
    struct.pack_into("<I", opt, 108, 16)  # NumberOfRvaAndSizes
    struct.pack_into("<II", opt, 112 + 8, rva_base, len(body))  # import directory
    out += bytes(opt)
    out += b".rdata\0\0" + struct.pack(
        "<IIIIIIHHI", len(body), rva_base, len(body), raw_base, 0, 0, 0, 0, 0x40000040
    )
    assert len(out) == sec_off + 40, (len(out), sec_off + 40)
    out += bytes(raw_base - len(out))
    out += bytes(body)
    return bytes(out)


def build_elf(needed: list[str]) -> bytes:
    """最小 64 位 ELF：.dynamic（DT_NEEDED 若干）+ .dynstr + .shstrtab。"""
    header_size = 64
    shstr = b"\0.shstrtab\0.dynstr\0.dynamic\0"
    # 1 = 跳过开头那个 NUL；每个名字后面还有自己的 NUL（写死偏移最容易差一）
    off_shstr, off_dynstr, off_dynamic = 1, 11, 19
    assert shstr[off_shstr : off_shstr + 9] == b".shstrtab"
    assert shstr[off_dynstr : off_dynstr + 7] == b".dynstr"
    assert shstr[off_dynamic : off_dynamic + 8] == b".dynamic"

    dynstr = bytearray(b"\0")
    dyn = bytearray()
    for name in needed:
        idx = len(dynstr)
        dynstr += name.encode("ascii") + b"\0"
        dyn += struct.pack("<QQ", 1, idx)  # DT_NEEDED
    dyn += struct.pack("<QQ", 0, 0)  # DT_NULL

    f_off_shstr = header_size
    f_off_dynstr = f_off_shstr + len(shstr)
    f_off_dynamic = f_off_dynstr + len(dynstr)
    sh_off = f_off_dynamic + len(dyn)

    e = bytearray(header_size)
    e[0:4] = b"\x7fELF"
    e[4] = 2  # 64-bit
    e[5] = 1  # little-endian
    e[6] = 1  # EV_CURRENT
    struct.pack_into("<HHI", e, 16, 3, 0x3E, 1)  # ET_DYN / EM_X86_64 / version
    struct.pack_into("<Q", e, 0x28, sh_off)  # e_shoff
    struct.pack_into("<H", e, 0x34, header_size)  # e_ehsize
    struct.pack_into("<H", e, 0x3A, 64)  # e_shentsize
    struct.pack_into("<H", e, 0x3C, 4)  # e_shnum: null + 3
    struct.pack_into("<H", e, 0x3E, 3)  # e_shstrndx = .shstrtab

    def section(name_off: int, typ: int, off: int, size: int) -> bytes:
        return struct.pack("<IIQQQQIIQQ", name_off, typ, 0, 0, off, size, 0, 0, 1, 0)

    shs = b"".join(
        [
            section(0, 0, 0, 0),
            section(off_dynstr, 3, f_off_dynstr, len(dynstr)),  # SHT_STRTAB
            section(off_dynamic, 6, f_off_dynamic, len(dyn)),  # SHT_DYNAMIC
            section(off_shstr, 3, f_off_shstr, len(shstr)),  # SHT_STRTAB（节名表）
        ]
    )
    return bytes(e) + shstr + bytes(dynstr) + bytes(dyn) + shs


class EngineDepsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    # --- PE（Windows）---

    def test_pe_missing_vc_runtime_is_red(self) -> None:
        (self.dir / "audiocpp_server.exe").write_bytes(
            build_pe(["KERNEL32.dll", "MSVCP140.dll", "VCRUNTIME140.dll"])
        )
        missing, source = ced.check(self.dir)
        self.assertIn("PE", source)
        self.assertEqual(missing, ["MSVCP140.dll", "VCRUNTIME140.dll"])

    def test_pe_with_sibling_dlls_is_green_and_case_insensitive(self) -> None:
        (self.dir / "audiocpp_server.exe").write_bytes(
            build_pe(["KERNEL32.dll", "MSVCP140.dll"])
        )
        (self.dir / "msvcp140.dll").write_bytes(b"stub")
        self.assertEqual(ced.check(self.dir)[0], [])

    def test_ucrt_api_ms_win_is_system(self) -> None:
        (self.dir / "audiocpp_server.exe").write_bytes(
            build_pe(["api-ms-win-crt-runtime-l1-1-0.dll", "KERNEL32.dll"])
        )
        self.assertEqual(ced.check(self.dir)[0], [])

    # --- ELF（Linux）---

    def test_elf_missing_sibling_so_is_red(self) -> None:
        (self.dir / "audiocpp_server").write_bytes(
            build_elf(["libggml.so.0", "libggml-base.so.0", "libc.so.6", "libgomp.so.1"])
        )
        missing, source = ced.check(self.dir)
        self.assertIn("ELF", source)
        self.assertEqual(missing, ["libggml.so.0", "libggml-base.so.0"])

    def test_elf_with_sibling_so_is_green(self) -> None:
        (self.dir / "audiocpp_server").write_bytes(build_elf(["libggml.so.0", "libc.so.6"]))
        (self.dir / "libggml.so.0").write_bytes(b"stub")
        self.assertEqual(ced.check(self.dir)[0], [])

    # --- 边界 ---

    def test_missing_binary_is_not_green(self) -> None:
        with self.assertRaises(ced.DepError):
            ced.check(self.dir)

    def test_unknown_format_is_not_green(self) -> None:
        (self.dir / "audiocpp_server").write_bytes(b"not a binary")
        with self.assertRaises(ced.DepError):
            ced.check(self.dir)


if __name__ == "__main__":
    unittest.main()
