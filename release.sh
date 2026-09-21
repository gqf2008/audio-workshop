#!/usr/bin/env bash
# 一键发版：打包 → 签名 → 公证 → 装订（.app 与 DMG 都要）。
#
#   ./release.sh
#   NOTARY_PROFILE=xxx ./release.sh        # 换公证 profile（默认 audio-workshop-notary）
#
# 公证逻辑**不在本仓库复制一份**：`~/scripts/notarize.sh` 是机器级的通用实现
# （签名 → 校验 → zip → `notarytool submit --wait` → `stapler staple` → `spctl` 终检），
# 项目里再来一份就会有两处要同步改。这里只负责"调它、传对 profile 与 entitlements"。
#
# 凭据（Apple ID / App 专用密码 / Team ID）在 macOS Keychain 里，**不进仓库**。
# profile 怎么建、叫什么，记在 `gqf2008/sqb-private` 的 accounts/05-ssh-certs.md。
#
# 为什么 DMG 要单独走一遍公证：用户拿到的是 DMG，双击挂载时 Gatekeeper 判的是 **DMG 自己**
# 的签名/票据。只公证 .app 的话实测 `spctl -a -t open` 会说 `rejected / no usable signature`。
# 而且 DMG 必须在 .app **staple 之后**重建 —— 否则装进去的是没票据的那份 .app。
set -euo pipefail
cd "$(dirname "$0")"

APP_DISPLAY_NAME="音频作坊"
ARTIFACT_NAME="AudioWorkshop"      # ASCII：见 package.sh 与 docs/release.md 的说明
PROFILE="${NOTARY_PROFILE:-audio-workshop-notary}"
NOTARIZE_SH="${NOTARIZE_SH:-$HOME/scripts/notarize.sh}"
APP="dist/${APP_DISPLAY_NAME}.app"
ENTITLEMENTS="packaging/entitlements.plist"

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
DMG="dist/${ARTIFACT_NAME}-${VERSION}.dmg"

# 许可门禁（断言实现只有 packaging/check_license.sh 一份；package.sh 也会跑它）。
# 发版是对外行为：LICENSE 缺失、或与 Cargo.toml 元数据不一致，在这里就红。
packaging/check_license.sh

./package.sh

[ -d "${APP}" ] || { echo "❌ 缺少 ${APP}" >&2; exit 1; }
[ -x "${NOTARIZE_SH}" ] || {
  echo "❌ 找不到通用公证脚本：${NOTARIZE_SH}" >&2
  echo "   （可以用 NOTARIZE_SH=/path/to/notarize.sh 覆盖）" >&2
  exit 1
}

echo
echo "===== 1/2：.app 签名 + 公证 + 装订 ====="
# --entitlements 必须一路带着：通用脚本会**重新签名**，不带就把 package.sh 里
# 那份 disable-library-validation 洗掉了 —— 结果是"公证过了但一启动就 dyld 报错"。
"${NOTARIZE_SH}" "${APP}" --profile "${PROFILE}" \
  --entitlements "${ENTITLEMENTS}" \
  --zip "dist/${ARTIFACT_NAME}-${VERSION}-notarize.zip"

echo
echo "===== 2/2：DMG 重建 + 签名 + 公证 + 装订 ====="
packaging/make_dmg.sh "${APP}" "${DMG}"

IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
  | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
[ -n "${IDENTITY}" ] || { echo "❌ 没有 Developer ID Application 身份，DMG 无法签名" >&2; exit 1; }

codesign --force --timestamp --sign "${IDENTITY}" "${DMG}"
xcrun notarytool submit "${DMG}" --keychain-profile "${PROFILE}" --wait
xcrun stapler staple "${DMG}"
echo "== DMG 终检 =="
spctl -a -t open --context context:primary-signature -v "${DMG}"

echo
echo "✅ 发版产物就绪"
ls -lh "${DMG}" | sed 's/^/   /'
