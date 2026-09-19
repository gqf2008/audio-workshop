#!/usr/bin/env bash
# 组装 macOS 应用包：音频作坊.app + 可分发的 DMG。
#
#   ./package.sh                 # 默认：release 构建 + 组 .app + 打 DMG（ad-hoc 签名，本机可跑）
#   BUILD_NUMBER=42 ./package.sh # 指定 CFBundleVersion（默认用时间戳，必须单调递增）
#
# 签名 / 公证不在这个脚本里：签名身份与公证凭据属于机器级秘密，走
# `~/scripts/notarize.sh`（通用版，凭据在 Keychain），见 ./release.sh 与 docs/release.md。
#
# 为什么 Info.plist 的版本要现读 Cargo.toml：写死必然漂移 —— 改了 Cargo.toml 忘了改
# plist，就会出现"关于本机显示 0.1.0、自动更新却按 0.2.0 比"这种最难查的错。
#
# 为什么构建前强制 LIBONNXRUNTIME_NO_PKG_CONFIG=1（人声分离的 ONNX Runtime）：
# ort-sys 的 build.rs 是「pkg-config 优先 → 失败才下载官方预编译包」。开发机装了
# homebrew onnxruntime 时 pkg-config 一命中，就会把 /opt/homebrew/.../libonnxruntime
# 链进发布包 —— 后果是用户必须自己 `brew install onnxruntime`，而且那份 dylib 还
# 拖着 86 个 homebrew 依赖（abseil/protobuf/onnx/re2…），全都得随包重签。
# 设成 1 之后 ort-sys 走「手工 setup」分支，改用官方静态库（实测 mac 二进制
# 从 19MB → 33MB，DMG 10.5MB → 16.3MB），发布包恢复成零第三方依赖。
set -euo pipefail
cd "$(dirname "$0")"

# 显示名与**分发文件名**是两回事，别用一个变量串起来：
#   · 显示名（.app 目录名 / CFBundleDisplayName）用中文，用户在 Finder 里看到的就是它；
#   · 上传到 Release 的文件名必须 ASCII —— v0.1.0 实测过：`音频作坊-0.1.0.dmg` 上传后
#     变成 `-0.1.0.dmg`（中文被剥掉），下载链接跟着坏。
APP_DISPLAY_NAME="音频作坊"
ARTIFACT_NAME="AudioWorkshop"
BIN_NAME="audio-workshop"
BUNDLE_ID="com.sqb.audio-workshop"   # 稳定值：改了等于换了一个 app，偏好/TCC 授权都会另起一份
MIN_MACOS="12.0"
DIST="dist"
APP="${DIST}/${APP_DISPLAY_NAME}.app"    # .app 目录名可以中文：它不进 HTTP 文件名

# 断言"要上传的文件名"是 ASCII。宁可在这里红，也别等上传完才发现名字被截断。
assert_ascii() {
  # 只看**文件名**：传进来的可能是 dist/xxx 这样的路径，路径分隔符不在允许集里，
  # 直接拿整个路径去匹配会把合法的 ASCII 名字也判红（写这条时第一版就是这么错的）。
  local name; name="$(basename "$1")"
  case "$name" in
    *[!A-Za-z0-9._-]*)
      echo "❌ 分发文件名含非 ASCII 字符：${name}" >&2
      echo "   （GitHub Release 上传会截断它 —— v0.1.0 的 '-0.1.0.dmg' 就是这么来的）" >&2
      exit 1 ;;
  esac
}

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
[ -n "${VERSION}" ] || { echo "❌ 读不到 Cargo.toml 里的 version" >&2; exit 1; }
BUILD_NUMBER="${BUILD_NUMBER:-$(date +%Y%m%d%H%M)}"

echo "== [1/6] release 构建（静态 ONNX Runtime，见文件头说明）=="
LIBONNXRUNTIME_NO_PKG_CONFIG=1 cargo build --release --bin "${BIN_NAME}"
# 用 cargo 自己报的 target 目录，不靠猜：CARGO_TARGET_DIR 与 .cargo/config.toml 都会影响它
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BIN_SRC="${TARGET_DIR}/release/${BIN_NAME}"
[ -x "${BIN_SRC}" ] || { echo "❌ 找不到产物：${BIN_SRC}" >&2; exit 1; }

echo "== [2/6] 组装 ${APP_DISPLAY_NAME}.app =="
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
cp -f "${BIN_SRC}" "${APP}/Contents/MacOS/${BIN_NAME}"
chmod +x "${APP}/Contents/MacOS/${BIN_NAME}"

# 硬门禁：发布包的二进制只允许链系统库。历史上 homebrew onnxruntime 会静默混进来，
# 表现是"发布包在开发者机器上好好的，用户机器一启动就 dyld 报错"。
# 这里在打包阶段就红，别等用户装。
leaked="$(otool -L "${APP}/Contents/MacOS/${BIN_NAME}" \
  | tail -n +2 | awk '{print $1}' \
  | grep -Ev '^(/usr/lib/|/System/)' || true)"
if [ -n "${leaked}" ]; then
  echo "❌ ${BIN_NAME} 链了非系统库，发布包会依赖用户机器上的第三方 dylib：" >&2
  echo "${leaked}" | sed 's/^/     /' >&2
  echo "   → 人声分离的 ONNX Runtime 必须走静态链接（LIBONNXRUNTIME_NO_PKG_CONFIG=1）。" >&2
  exit 1
fi
echo "   → 依赖检查通过：只链系统库"

if [ -f assets/icon.icns ]; then
  cp -f assets/icon.icns "${APP}/Contents/Resources/icon.icns"
else
  echo "⚠️  没有 assets/icon.icns —— 用系统默认图标（要生成：python3 tools/gen_app_icon.py）"
fi

echo "== [3/6] 写 Info.plist（版本 ${VERSION} / build ${BUILD_NUMBER}）=="
cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>${BIN_NAME}</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleName</key><string>${APP_DISPLAY_NAME}</string>
    <key>CFBundleDisplayName</key><string>${APP_DISPLAY_NAME}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${BUILD_NUMBER}</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>LSMinimumSystemVersion</key><string>${MIN_MACOS}</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint "${APP}/Contents/Info.plist" >/dev/null

echo "== [4/6] 签名（有 Developer ID 就正式签，否则 ad-hoc）=="
IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
  | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
ENTITLEMENTS="packaging/entitlements.plist"
[ -f "${ENTITLEMENTS}" ] || { echo "❌ 缺少 ${ENTITLEMENTS}" >&2; exit 1; }
if [ -n "${IDENTITY}" ]; then
  codesign --force --options runtime --timestamp \
    --entitlements "${ENTITLEMENTS}" --sign "${IDENTITY}" "${APP}"
  echo "   → ${IDENTITY}（附 entitlements：disable-library-validation，见文件里的说明）"
else
  codesign --force --options runtime --entitlements "${ENTITLEMENTS}" --sign - "${APP}"
  echo "   → ad-hoc（本机可跑，但别人下载会被 Gatekeeper 拦；要分发请走 ./release.sh）"
fi
codesign --verify --strict --verbose=1 "${APP}"

echo "== [5/6] 打 DMG =="
DMG="${DIST}/${ARTIFACT_NAME}-${VERSION}.dmg"
assert_ascii "${DMG}"
packaging/make_dmg.sh "${APP}" "${DMG}"

echo "== [6/6] 完成 =="
ls -lh "${APP}" "${DMG}" | sed 's/^/   /'
echo
echo "   分发前先公证：./release.sh"
