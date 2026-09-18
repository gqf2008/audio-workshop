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
set -euo pipefail
cd "$(dirname "$0")"

APP_DISPLAY_NAME="音频作坊"
BIN_NAME="audio-workshop"
BUNDLE_ID="com.sqb.audio-workshop"   # 稳定值：改了等于换了一个 app，偏好/TCC 授权都会另起一份
MIN_MACOS="12.0"
DIST="dist"
APP="${DIST}/${APP_DISPLAY_NAME}.app"

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
[ -n "${VERSION}" ] || { echo "❌ 读不到 Cargo.toml 里的 version" >&2; exit 1; }
BUILD_NUMBER="${BUILD_NUMBER:-$(date +%Y%m%d%H%M)}"

echo "== [1/6] release 构建 =="
cargo build --release --bin "${BIN_NAME}"
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
DMG="${DIST}/${APP_DISPLAY_NAME}-${VERSION}.dmg"
packaging/make_dmg.sh "${APP}" "${DMG}"

echo "== [6/6] 完成 =="
ls -lh "${APP}" "${DMG}" | sed 's/^/   /'
echo
echo "   分发前先公证：./release.sh"
