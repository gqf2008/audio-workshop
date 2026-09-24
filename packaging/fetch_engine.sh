#!/usr/bin/env bash
# 取随包推理引擎（audio.cpp 的 audiocpp_server），校验 sha256 后落到目标目录。
#
#   packaging/fetch_engine.sh <out-dir> [platform]
#
# platform 省略时按本机推断（macos-arm64 / macos-x64 / linux-x64 / windows-x64）。
# 来源与校验值都读 engine-lock.json —— 换引擎只能改那个文件，不能在这里写死。
#
# 为什么不在 package.sh 里内联：三个平台都用同一份取件逻辑，各写一遍必然漂移。
#
# Windows（CI 里走 Git Bash）：脚本里的 python 调用一律显式 encoding="utf-8" ——
# 默认代码页是 cp1252，读 engine-lock.json 里的中文注释会直接 UnicodeDecodeError（实测）。
#
# 离线/本地构建场景：
#   AW_ENGINE_TARBALL=/path/to/audio-....tar.gz  packaging/fetch_engine.sh <out-dir>
#   直接复用现成压缩包，但仍按 engine-lock.json 校验 sha256（锁里是 PENDING 时跳过校验）。
set -euo pipefail

OUT_DIR="${1:?用法: fetch_engine.sh <out-dir> [platform]}"
cd "$(dirname "$0")/.."

LOCK="engine-lock.json"
[ -f "${LOCK}" ] || { echo "❌ 缺少 ${LOCK}" >&2; exit 1; }

# 跨平台算 sha256：Windows 的 Git Bash **没有 shasum**（CI 实测 `shasum: command not found`），
# 所以按可用性依次尝试。python3 排第一：三平台的 CI 与开发机上都有（本脚本其它部分也依赖它）。
sha256_of() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import hashlib,sys
h=hashlib.sha256()
f=open(sys.argv[1],"rb")
for chunk in iter(lambda: f.read(1048576), b""):
    h.update(chunk)
print(h.hexdigest())' "$1"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    echo "找不到任何 sha256 工具（python3 / shasum / sha256sum）" >&2
    return 1
  fi
}

detect_platform() {
  case "$(uname -s)" in
    Darwin)
      case "$(uname -m)" in
        arm64) echo "macos-arm64" ;;
        x86_64) echo "macos-x64" ;;
      esac ;;
    Linux) echo "linux-x64" ;;
    MINGW*|MSYS*|CYGWIN*) echo "windows-x64" ;;
  esac
}

PLATFORM="${2:-$(detect_platform)}"
[ -n "${PLATFORM}" ] || { echo "❌ 无法推断平台，请显式传第二个参数" >&2; exit 1; }

read_lock() { python3 - "$LOCK" "$PLATFORM" "$1" <<'PY'
import json, sys
lock = json.load(open(sys.argv[1], encoding="utf-8"))
art = lock["artifacts"].get(sys.argv[2])
if art is None:
    print(f"❌ engine-lock.json 里没有平台 {sys.argv[2]}", file=sys.stderr)
    sys.exit(1)
print(art[sys.argv[3]])
PY
}

ASSET="$(read_lock asset)"
WANT_SHA="$(read_lock sha256)"
BASE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["release_base"])' "$LOCK")"
BIN_NAME="$(read_lock binary)"

CACHE_DIR="${TMPDIR:-/tmp}/audio-workshop-engine"
mkdir -p "${CACHE_DIR}"
TARBALL="${AW_ENGINE_TARBALL:-${CACHE_DIR}/${ASSET}}"

if [ ! -f "${TARBALL}" ]; then
  echo "== 下载引擎 ${ASSET} =="
  curl -fL --retry 3 --retry-delay 2 -o "${TARBALL}.part" "${BASE}/${ASSET}"
  mv "${TARBALL}.part" "${TARBALL}"
else
  echo "== 复用已缓存引擎 ${TARBALL} =="
fi

if [ "${WANT_SHA}" = "PENDING_CI" ]; then
  echo "   ⚠️  engine-lock.json 里 ${PLATFORM} 的 sha256 还是 PENDING_CI —— 跳过校验（仅限发版前）"
else
  GOT_SHA="$(sha256_of "${TARBALL}")"
  if [ "${GOT_SHA}" != "${WANT_SHA}" ]; then
    echo "❌ 引擎 sha256 不匹配（期望 ${WANT_SHA}，实际 ${GOT_SHA}）" >&2
    echo "   → 不匹配就不许装包：随包引擎被替换过，或下载被截断。" >&2
    exit 1
  fi
  echo "   → sha256 校验通过"
fi

# 解包只取需要的三类：服务二进制 + Apache-2.0 许可 + **与二进制同级的运行库**。
# 上游压缩包里还有 cli/gguf/tools/model_specs（model spec 已编进 server），随发布会白白变大
# （audiocpp_cli 一个就 ~29 MB）。
#
# 为什么必须带上同级运行库（2026-09-24 实测，判据与实现见 tools/check_engine_deps.py）：
#   · Windows：`audiocpp_server.exe` 的导入表里有 MSVCP140 / VCRUNTIME140 / VCOMP140…，
#     上游把对应 DLL 摆在归档根；只解出 exe 就是"找不到 MSVCP140.dll"，
#     而 package_windows.ps1 对外承诺的是"用户不用装 VC++ Redist"。
#   · Linux：`audiocpp_server` 的 DT_NEEDED 含 libggml.so.0 / libggml-base.so.0，
#     且 RUNPATH=$ORIGIN —— 加载器只去**二进制所在目录**找，缺了就起不来。
#   · macOS：当前产物只链系统框架，属于"没有同级依赖"的正例。
#
# 用 python3 解包（而不是 unzip/tar 命令行）的原因：三种归档、两种包内布局
# （上游 release 资产 = 文件在归档根；GitHub Actions artifact 形态 = 多一层目录）
# 都要吃得下，而 `tar`/`unzip` 在"某个模式一个都没匹配上"时行为各不相同
# （GNU tar 直接报错退出、bsdtar 给警告、unzip 返回 11），拼命令行容易变成假失败。
rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}"
python3 - "${TARBALL}" "${OUT_DIR}" "${BIN_NAME}" <<'PY'
import os, re, sys, tarfile, zipfile

tarball, out_dir, bin_name = sys.argv[1], sys.argv[2], sys.argv[3]
want = {bin_name, "LICENSE"}
runtime_re = re.compile(r"^(lib.*\.so(\.\d+)*|.*\.dll|.*\.dylib)$", re.IGNORECASE)


def wanted(name: str) -> bool:
    base = os.path.basename(name)
    return base in want or bool(runtime_re.match(base))


def copy_members(members, reader):
    got = []
    for name in members:
        base = os.path.basename(name)
        with open(os.path.join(out_dir, base), "wb") as fh:
            fh.write(reader(name))
        got.append(base)
    return got


if tarball.lower().endswith(".zip"):
    with zipfile.ZipFile(tarball) as zf:
        names = [n for n in zf.namelist() if wanted(n)]
        got = copy_members(names, zf.read)
else:
    with tarfile.open(tarball, "r:*") as tf:
        names = [m.name for m in tf.getmembers() if m.isfile() and wanted(m.name)]

        def read(name):
            return tf.extractfile(name).read()

        got = copy_members(names, read)

print("   解出：" + ", ".join(sorted(got)))
PY

[ -f "${OUT_DIR}/${BIN_NAME}" ] || { echo "❌ 解包后找不到 ${BIN_NAME}" >&2; exit 1; }
[ -f "${OUT_DIR}/LICENSE" ] || { echo "❌ 解包后找不到 LICENSE" >&2; exit 1; }
chmod +x "${OUT_DIR}/${BIN_NAME}" "${OUT_DIR}"/*.so* 2>/dev/null || chmod +x "${OUT_DIR}/${BIN_NAME}"

# 运行时依赖门禁：二进制的**真实依赖**（PE 导入表 / ELF DT_NEEDED / otool -L）必须都在这一层。
# 放在这里而不是各平台打包脚本里：三平台共用一份取件逻辑，门禁也就只有一处。
python3 tools/check_engine_deps.py "${OUT_DIR}"

echo "   → 引擎就绪：${OUT_DIR}/${BIN_NAME} ($(du -sh "${OUT_DIR}" | awk '{print $1}'))"
