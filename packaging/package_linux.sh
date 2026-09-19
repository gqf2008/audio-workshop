#!/usr/bin/env bash
# 组装 Linux 发行包：tar.gz（可携）+ .desktop + 引擎。
#
#   packaging/package_linux.sh              # 产物落 dist/
#   BUILD_NUMBER=42 packaging/package_linux.sh
#
# 形态选择：先给 tar.gz（解压即可跑，不依赖发行版包管理器），deb 见 make_deb.sh。
# AppImage 需要额外工具链（linuxdeploy/appimagetool），等 tar.gz 这条稳定了再加 —— 先跑通
# 再上更强封装的顺序，避免一上来就同时调三样东西。
#
# 布局（与 macOS 的 Contents/Resources/engine 对齐，只是 Linux 没有 bundle 概念）：
#   audioshop/
#     ├─ audio-workshop          壳
#     ├─ engine/audiocpp_server  随包引擎
#     ├─ engine/LICENSE
#     └─ audio-workshop.desktop  桌面入口
set -euo pipefail
cd "$(dirname "$0")/.."

ARTIFACT_NAME="AudioWorkshop"
BIN_NAME="audio-workshop"
DIST="dist"
STAGE="${DIST}/linux-x64"
VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
[ -n "${VERSION}" ] || { echo "❌ 读不到 Cargo.toml 的 version" >&2; exit 1; }

echo "== [1/4] release 构建（静态 ONNX Runtime，见 package.sh 头注释）=="
LIBONNXRUNTIME_NO_PKG_CONFIG=1 cargo build --release --bin "${BIN_NAME}"
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BIN_SRC="${TARGET_DIR}/release/${BIN_NAME}"
[ -x "${BIN_SRC}" ] || { echo "❌ 找不到产物：${BIN_SRC}" >&2; exit 1; }

echo "== [2/4] 组装 ${STAGE} =="
rm -rf "${STAGE}"
mkdir -p "${STAGE}/engine"
cp -f "${BIN_SRC}" "${STAGE}/${BIN_NAME}"
chmod +x "${STAGE}/${BIN_NAME}"

# 硬门禁：发布包的壳只允许依赖系统库。与 package.sh 同一条理由 ——
# homebrew/自装的 onnxruntime 会静默混进来，等用户机器上才发现就是 dyld/ld 报错。
# `ldd` 只对当前平台的 ELF 有意义：脚本在 macOS/Linux 上都能跑，但依赖门禁只在 Linux 生效
# （macOS 上 ldd 不存在，会得到空结果 —— 那等于没有门禁，所以显式跳过并说明）。
if command -v ldd >/dev/null 2>&1; then
  leaked="$(ldd "${STAGE}/${BIN_NAME}" 2>/dev/null | awk '{print $3}' \
    | grep -Ev '^$|^/lib|^/usr/lib' || true)"
  if [ -n "${leaked}" ]; then
    echo "❌ ${BIN_NAME} 链了非系统库（发布包会依赖用户机器上的第三方 .so）：" >&2
    echo "${leaked}" | sed 's/^/     /' >&2
    echo "   → 人声分离的 ONNX Runtime 必须走静态链接（LIBONNXRUNTIME_NO_PKG_CONFIG=1）。" >&2
    exit 1
  fi
  echo "   → 依赖检查通过：只链系统库"
else
  # 不静默放过：门禁没跑就得说出来，否则"没检查"会被读成"检查通过"。
  echo "   ⚠️ 本机没有 ldd，跳过依赖门禁（正式发布在 Linux runner 上跑，那里会真检查）"
fi

packaging/fetch_engine.sh "${STAGE}/engine" linux-x64

echo "== [3/4] 桌面入口 =="
cat > "${STAGE}/audio-workshop.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=音频作坊
Comment=本地优先的音频工作台（配音 / BGM / 人声分离 / 音乐制作 / 音色设计）
Exec=audio-workshop
Terminal=false
Categories=AudioVideo;Audio;
DESKTOP

echo "== [4/4] 打 tar.gz =="
OUT="${DIST}/${ARTIFACT_NAME}-${VERSION}-linux-x64.tar.gz"
# 分发文件名必须 ASCII（GitHub Release 会剥掉非 ASCII，见 package.sh 的同名断言）
# 归档根的目录名保持 ASCII，解压后在终端里也不会出现乱码路径。
tar czf "${OUT}" -C "${STAGE}" .
echo "   ${OUT}"
ls -lh "${OUT}" | awk '{print "   "$5" "$9}'
