#!/usr/bin/env python3
"""随包引擎的**运行时依赖**门禁：引擎目录里必须凑齐它真正要加载的同级库。

    python3 tools/check_engine_deps.py <engine-dir>

为什么需要（2026-09-24 实测）：
`fetch_engine.sh` 原来只解出「引擎二进制 + LICENSE」，而上游产物还把**运行库**摆在
归档根、与二进制同级：

  · Windows：`audiocpp_server.exe` 的导入表里有 `MSVCP140.dll` / `VCRUNTIME140.dll` /
    `VCOMP140.DLL` …（PE 导入表实测，见下），上游把它们一起打进 zip —— 少了就
    "找不到 MSVCP140.dll"，而这正是 `package_windows.ps1` 声称"用户不用装 VC++ Redist"
    却兑现不了的地方（脚本里并没有复制这些 DLL）。
  · Linux：`audiocpp_server` 的 `DT_NEEDED` 含 `libggml.so.0` / `libggml-base.so.0`，
    且 `RUNPATH=$ORIGIN` —— 加载器只去**二进制自己所在目录**找；只解出二进制的话
    一启动就是 `error while loading shared libraries: libggml.so.0`。
  · macOS：目前那份产物只链系统框架（`otool -L` 全是 `/usr/lib`、`/System`），
    所以它是"没有同级依赖"的正例，不是"不用检查"。

判据是**二进制的真实依赖**，不是"归档里有哪些文件"：依赖列表从导入表/动态段里读，
凡是**不是操作系统自带**的依赖，就必须在引擎目录里找得到同名文件。这样上游换构建方式
（例如以后改成静态 CRT）不会误报，而"少拷了库"一定红。

退出码：0 = 依赖齐；1 = 缺依赖（列出缺哪些）；2 = 用法/不认识的格式（不静默放过）。
"""
from __future__ import annotations

import os
import struct
import subprocess
import sys
from pathlib import Path

# --- 系统自带、不随包分发 ------------------------------------------------

# Windows：OS 自带或 VC++ 运行库由系统提供时才略过 —— 这里全部**要求随包**，
# 只放行内核/系统 DLL 与 UCRT（api-ms-win-*）这一类。
WINDOWS_SYSTEM_PREFIXES = ("api-ms-win-", "ext-ms-win-")
WINDOWS_SYSTEM_DLLS = {
    "advapi32.dll", "bcrypt.dll", "cfgmgr32.dll", "combase.dll", "comctl32.dll",
    "comdlg32.dll", "crypt32.dll", "dwmapi.dll", "gdi32.dll", "iphlpapi.dll",
    "kernel32.dll", "kernelbase.dll", "mf.dll", "mfplat.dll", "mfreadwrite.dll",
    "msvcrt.dll", "ntdll.dll", "ole32.dll", "oleaut32.dll", "powrprof.dll",
    "psapi.dll", "rpcrt4.dll", "secur32.dll", "setupapi.dll", "shell32.dll",
    "shlwapi.dll", "user32.dll", "userenv.dll", "uxtheme.dll", "version.dll",
    "winmm.dll", "ws2_32.dll", "wtsapi32.dll", "nvcuda.dll",
}

# Linux：发行版自带的基础库（glibc / libstdc++ / OpenMP 运行时）。
# libggml* **不在**这里 —— 它是上游随包带的那几个 .so。
LINUX_SYSTEM_LIBS = (
    "libc.so", "libm.so", "libdl.so", "libpthread.so", "librt.so", "libutil.so",
    "libgcc_s.so", "libstdc++.so", "libgomp.so", "libatomic.so", "libz.so",
    "ld-linux", "libnuma.so",
)

MACHO_SYSTEM_PREFIXES = ("/usr/lib/", "/System/")


class DepError(Exception):
    """解析失败（格式不对/读不出来），调用方按"没检查"处理，不当成通过。"""


def force_utf8_stdio() -> None:
    """把 stdout/stderr 显式设成 UTF-8。

    为什么（2026-09-24 release 流水线 Windows job 实测）：Windows 上由 **Git Bash**
    拉起 python 时，stdout 是 cp1252 —— 打印中文/`✅` 直接 `UnicodeEncodeError`，
    整个 Windows 打包失败（`python3 - <<PY` 那段解包脚本先踩的，同一处修复）。
    bash 侧打印的中文本来就是 UTF-8 字节，GitHub Actions 日志也按 UTF-8 解码，
    所以这里改成 UTF-8 才是与其它输出一致的做法（不是"换个符号绕过"）。
    `reconfigure` 不可用时（老解释器/非文本流）保持原样，不因此报错。
    """
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, OSError, ValueError):  # pragma: no cover - 环境相关
            pass


def _find_binary(engine_dir: Path) -> Path:
    for name in ("audiocpp_server.exe", "audiocpp_server"):
        p = engine_dir / name
        if p.is_file():
            return p
    raise DepError(f"引擎目录里找不到 audiocpp_server(.exe)：{engine_dir}")


# --- PE（Windows）--------------------------------------------------------


def pe_imports(data: bytes) -> list[str]:
    """读 PE 导入表的 DLL 名；不依赖第三方库（CI 与 macOS 上都要能跑）。"""
    if data[:2] != b"MZ":
        raise DepError("不是 PE（缺 MZ）")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe : pe + 4] != b"PE\0\0":
        raise DepError("不是 PE（缺 PE\\0\\0）")
    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    opt_size = struct.unpack_from("<H", data, pe + 20)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic == 0x20B:
        dd_off = opt + 112
    elif magic == 0x10B:
        dd_off = opt + 96
    else:
        raise DepError(f"不认识的 PE 可选头 magic：0x{magic:x}")
    imp_rva, _imp_size = struct.unpack_from("<II", data, dd_off + 8)
    if imp_rva == 0:
        return []

    secs = []
    so = opt + opt_size
    for i in range(nsec):
        b = so + 40 * i
        vsize = struct.unpack_from("<I", data, b + 8)[0]
        vaddr = struct.unpack_from("<I", data, b + 12)[0]
        psize = struct.unpack_from("<I", data, b + 16)[0]
        praddr = struct.unpack_from("<I", data, b + 20)[0]
        secs.append((vaddr, vsize, praddr, psize))

    def rva_to_off(rva: int) -> int:
        for vaddr, vsize, praddr, psize in secs:
            if vaddr <= rva < vaddr + max(vsize, psize):
                return praddr + (rva - vaddr)
        raise DepError(f"RVA 0x{rva:x} 落在任何节之外")

    names: list[str] = []
    off = rva_to_off(imp_rva)
    while True:
        desc = data[off : off + 20]
        if len(desc) < 20 or desc == b"\0" * 20:
            break
        name_rva = struct.unpack_from("<I", data, off + 12)[0]
        if name_rva == 0:
            break
        no = rva_to_off(name_rva)
        end = data.index(b"\0", no)
        names.append(data[no:end].decode("ascii", "replace"))
        off += 20
    return names


# --- ELF（Linux）---------------------------------------------------------


def elf_needed(data: bytes) -> list[str]:
    """读 ELF 的 DT_NEEDED（含 RUNPATH，只为让报错信息能指向 $ORIGIN）。"""
    if data[:4] != b"\x7fELF":
        raise DepError("不是 ELF")
    if data[4] != 2:
        raise DepError("只支持 64 位 ELF")
    e_shoff = struct.unpack_from("<Q", data, 0x28)[0]
    e_shentsize = struct.unpack_from("<H", data, 0x3A)[0]
    e_shnum = struct.unpack_from("<H", data, 0x3C)[0]
    e_shstrndx = struct.unpack_from("<H", data, 0x3E)[0]

    def section(i: int) -> dict:
        off = e_shoff + i * e_shentsize
        name, typ, _flags, _addr, sh_off, size, _link, _info, _align, _entsize = struct.unpack_from(
            "<IIQQQQIIQQ", data, off
        )
        return {"name": name, "type": typ, "off": sh_off, "size": size}

    sections = [section(i) for i in range(e_shnum)]
    strtab = sections[e_shstrndx]

    def sname(offset: int) -> str:
        end = data.index(b"\0", strtab["off"] + offset)
        return data[strtab["off"] + offset : end].decode("ascii", "replace")

    dynamic = dynstr = None
    for sec in sections:
        n = sname(sec["name"])
        if n == ".dynamic":
            dynamic = sec
        elif n == ".dynstr":
            dynstr = sec
    if dynamic is None or dynstr is None:
        raise DepError("ELF 缺 .dynamic / .dynstr（静态链接？那就没有 DT_NEEDED）")

    def dstr(offset: int) -> str:
        end = data.index(b"\0", dynstr["off"] + offset)
        return data[dynstr["off"] + offset : end].decode("ascii", "replace")

    out: list[str] = []
    for off in range(dynamic["off"], dynamic["off"] + dynamic["size"], 16):
        tag, val = struct.unpack_from("<QQ", data, off)
        if tag == 0:
            break
        if tag == 1:
            out.append(dstr(val))
        elif tag == 0x1D:
            out.append("RUNPATH=" + dstr(val))
    return out


# --- Mach-O（macOS）------------------------------------------------------


def macho_deps(binary: Path) -> list[str]:
    try:
        out = subprocess.run(
            ["otool", "-L", str(binary)], capture_output=True, text=True, check=True
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:  # pragma: no cover - 环境相关
        raise DepError(f"otool -L 跑不起来：{exc}") from exc
    deps: list[str] = []
    for line in out.splitlines()[1:]:
        line = line.strip()
        if not line:
            continue
        deps.append(line.split(" (compatibility")[0].strip())
    return deps


# --- 判定 ----------------------------------------------------------------


def _present(name: str, files_lower: dict[str, str]) -> bool:
    return name.lower() in files_lower


def check(engine_dir: Path) -> tuple[list[str], str]:
    """返回 (缺失依赖列表, 说明)。"""
    binary = _find_binary(engine_dir)
    files_lower = {p.name.lower(): p.name for p in engine_dir.iterdir() if p.is_file()}
    data = binary.read_bytes()

    if data[:2] == b"MZ":
        missing = [
            d
            for d in pe_imports(data)
            if not d.lower().startswith(WINDOWS_SYSTEM_PREFIXES)
            and d.lower() not in WINDOWS_SYSTEM_DLLS
            and not _present(d, files_lower)
        ]
        return missing, f"PE 导入表（{binary.name}）"

    if data[:4] == b"\x7fELF":
        missing = []
        for dep in elf_needed(data):
            if dep.startswith("RUNPATH="):
                continue
            if dep.startswith(LINUX_SYSTEM_LIBS):
                continue
            if not _present(dep, files_lower):
                missing.append(dep)
        return missing, f"ELF DT_NEEDED（{binary.name}，RUNPATH=$ORIGIN → 只认同级目录）"

    # Mach-O：魔数 0xFEEDFACF（64 位，含小端）等
    if data[:4] in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        missing = []
        for dep in macho_deps(binary):
            if dep.startswith(MACHO_SYSTEM_PREFIXES):
                continue
            if dep.startswith("@rpath/") or dep.startswith("@loader_path/") or dep.startswith(
                "@executable_path/"
            ):
                name = os.path.basename(dep)
                if not _present(name, files_lower):
                    missing.append(f"{dep}（同级缺 {name}）")
                continue
            # 绝对路径的非系统库：本机跑得起来、用户机器必炸
            missing.append(f"{dep}（绝对路径的非系统库，不能分发）")
        return missing, f"Mach-O 依赖（{binary.name}）"

    raise DepError(f"认不出的二进制格式：{binary}（前 4 字节 {data[:4]!r}）")


def main(argv: list[str]) -> int:
    force_utf8_stdio()
    if len(argv) != 2:
        print("用法: check_engine_deps.py <engine-dir>", file=sys.stderr)
        return 2
    engine_dir = Path(argv[1])
    if not engine_dir.is_dir():
        print(f"❌ 引擎目录不存在：{engine_dir}", file=sys.stderr)
        return 2
    try:
        missing, source = check(engine_dir)
    except DepError as exc:
        print(f"❌ 引擎依赖门禁没跑成：{exc}", file=sys.stderr)
        return 2
    if missing:
        print(f"❌ 引擎缺运行时依赖（{source}）：", file=sys.stderr)
        for dep in missing:
            print(f"     {dep}", file=sys.stderr)
        print(
            "   → 上游把这些库摆在归档根、与二进制同级；取件脚本必须一起解出来"
            "（见 packaging/fetch_engine.sh）。",
            file=sys.stderr,
        )
        return 1
    print(f"✅ 引擎运行时依赖齐（{source}）")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
