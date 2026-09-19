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
# 离线/本地构建场景：
#   AW_ENGINE_TARBALL=/path/to/audio-....tar.gz  packaging/fetch_engine.sh <out-dir>
#   直接复用现成压缩包，但仍按 engine-lock.json 校验 sha256（锁里是 PENDING 时跳过校验）。
set -euo pipefail

OUT_DIR="${1:?用法: fetch_engine.sh <out-dir> [platform]}"
cd "$(dirname "$0")/.."

LOCK="engine-lock.json"
[ -f "${LOCK}" ] || { echo "❌ 缺少 ${LOCK}" >&2; exit 1; }

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
lock = json.load(open(sys.argv[1]))
art = lock["artifacts"].get(sys.argv[2])
if art is None:
    print(f"❌ engine-lock.json 里没有平台 {sys.argv[2]}", file=sys.stderr)
    sys.exit(1)
print(art[sys.argv[3]])
PY
}

ASSET="$(read_lock asset)"
WANT_SHA="$(read_lock sha256)"
BASE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["release_base"])' "$LOCK")"
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
  GOT_SHA="$(shasum -a 256 "${TARBALL}" | awk '{print $1}')"
  if [ "${GOT_SHA}" != "${WANT_SHA}" ]; then
    echo "❌ 引擎 sha256 不匹配（期望 ${WANT_SHA}，实际 ${GOT_SHA}）" >&2
    echo "   → 不匹配就不许装包：随包引擎被替换过，或下载被截断。" >&2
    exit 1
  fi
  echo "   → sha256 校验通过"
fi

# 解包只取需要的两样：服务二进制 + Apache-2.0 许可。
# 上游压缩包里还有 cli/gguf/tools/model_specs（model spec 已编进 server），随发布会白白变大。
rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}"
# 两种包内布局都要吃得下：
#   · 上游 release 资产：文件在归档根（`audiocpp_server` / `LICENSE`）
#   · GitHub Actions artifact 形态：多一层 `<artifact-name>/` 目录
case "${ASSET}" in
  *.zip)
    if ! unzip -q -j "${TARBALL}" "${BIN_NAME}" "LICENSE" -d "${OUT_DIR}" 2>/dev/null; then
      unzip -q -j "${TARBALL}" "*/${BIN_NAME}" "*/LICENSE" -d "${OUT_DIR}"
    fi ;;
  *)
    if ! tar xzf "${TARBALL}" -C "${OUT_DIR}" "${BIN_NAME}" "LICENSE" 2>/dev/null; then
      tar xzf "${TARBALL}" -C "${OUT_DIR}" --strip-components=1 \
        --wildcards "*/${BIN_NAME}" "*/LICENSE"
    fi ;;
esac

[ -f "${OUT_DIR}/${BIN_NAME}" ] || { echo "❌ 解包后找不到 ${BIN_NAME}" >&2; exit 1; }
[ -f "${OUT_DIR}/LICENSE" ] || { echo "❌ 解包后找不到 LICENSE" >&2; exit 1; }
chmod +x "${OUT_DIR}/${BIN_NAME}"
echo "   → 引擎就绪：${OUT_DIR}/${BIN_NAME} ($(du -h "${OUT_DIR}/${BIN_NAME}" | awk '{print $1}'))"
