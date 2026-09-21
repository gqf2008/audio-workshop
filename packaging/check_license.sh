#!/usr/bin/env bash
# 许可门禁：LICENSE 全文到位，且与仓库包元数据（Cargo.toml 各 crate 的 license 字段）一致。
# 缺失或不一致即红。
#
# 为什么要有这条：仓库已 public 并发了 v0.1.0~v0.1.4 五个 Release，但当时没有 LICENSE 文件，
# 且根 crate 写 `license = "MIT"`、crates/aw-core 写 `license = "Apache-2.0"`、CHARTER §11 写
# 「协议待定」——对外实际是"保留所有权利"。见
# ~/.agents/rules/LESSON_公开仓库发版前须落LICENSE且与包元数据一致.md。
#
# 单一实现：package.sh 与 release.sh 都调本脚本（release.sh 也会先跑 package.sh），
# 断言只写一份，避免两处漂移。
set -euo pipefail
cd "$(dirname "$0")/.."   # 仓库根

LICENSE_FILE="LICENSE"
EXPECTED="Apache-2.0"

# 1) LICENSE 全文到位
[ -f "${LICENSE_FILE}" ] || {
  echo "❌ 缺少 ${LICENSE_FILE}（许可证全文）" >&2
  echo "   仓库已对外发布，缺 LICENSE 等于「保留所有权利」。" >&2
  exit 1
}
[ -s "${LICENSE_FILE}" ] || { echo "❌ ${LICENSE_FILE} 为空" >&2; exit 1; }

# 2) LICENSE 正文确实是 Apache-2.0 全文：抓三处锚点（标题、版本行、许可网址）。
#    只要元数据写 Apache-2.0 而正文是别的协议，这里就会红。
grep -q 'Apache License' "${LICENSE_FILE}" || {
  echo "❌ ${LICENSE_FILE} 不是 Apache-2.0 正文（缺 'Apache License' 标题）" >&2; exit 1; }
grep -q 'Version 2.0, January 2004' "${LICENSE_FILE}" || {
  echo "❌ ${LICENSE_FILE} 缺 'Version 2.0, January 2004' 版本行" >&2; exit 1; }
grep -q 'http://www.apache.org/licenses/' "${LICENSE_FILE}" || {
  echo "❌ ${LICENSE_FILE} 缺 'http://www.apache.org/licenses/' 网址" >&2; exit 1; }

# 3) 根 crate + 每个 workspace member 的 license 字段都与 EXPECTED 一致。
#    多 crate 许可不同必须显式处理（本仓库统一 Apache-2.0，所以这里直接逐一比对）。
tomls=(Cargo.toml)
while IFS= read -r t; do tomls+=("$t"); done < <(find crates -name Cargo.toml | sort)
for t in "${tomls[@]}"; do
  [ -f "$t" ] || { echo "❌ 找不到 $t" >&2; exit 1; }
  got="$(awk -F'"' '/^license[[:space:]]*=/{print $2; exit}' "$t")"
  [ -n "${got}" ] || { echo "❌ ${t} 缺 license 字段（应与 ${LICENSE_FILE} 一致）" >&2; exit 1; }
  [ "${got}" = "${EXPECTED}" ] || {
    echo "❌ ${t} license = \"${got}\"，应为 \"${EXPECTED}\"（与 ${LICENSE_FILE} 不一致）" >&2
    exit 1
  }
done

echo "✅ 许可门禁通过：${LICENSE_FILE} 全文到位，${#tomls[@]} 处 Cargo.toml 元数据 license = ${EXPECTED}"
