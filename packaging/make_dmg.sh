#!/usr/bin/env bash
# 从 .app 造一个可分发的 DMG。
#
# 单独抽出来是因为 release.sh 需要**再造一次**：DMG 必须在 .app 被 staple
# **之后**重建，否则发出去的是"app 有公证票据、DMG 里那份没有"的包。
#
#   packaging/make_dmg.sh <App.app> <out.dmg>
set -euo pipefail
APP="${1:?用法: make_dmg.sh <App.app> <out.dmg>}"
DMG="${2:?用法: make_dmg.sh <App.app> <out.dmg>}"
[ -d "${APP}" ] || { echo "❌ 找不到 .app：${APP}" >&2; exit 1; }

VOLNAME="$(basename "${APP}" .app)"
rm -f "${DMG}"
STAGE="$(mktemp -d)"
trap 'rm -rf "${STAGE}"' EXIT
cp -R "${APP}" "${STAGE}/"
ln -sf /Applications "${STAGE}/Applications"          # 拖进 Applications 的惯例
hdiutil create -quiet -volname "${VOLNAME}" -srcfolder "${STAGE}" \
  -fs HFS+ -format UDZO -imagekey zlib-level=9 "${DMG}"
echo "   ${DMG}  $(du -h "${DMG}" | cut -f1)"
