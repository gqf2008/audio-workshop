# 组装 Windows 发行包：可携 zip + NSIS 安装器。
#
#   pwsh -File packaging/package_windows.ps1            # 只出 zip
#   pwsh -File packaging/package_windows.ps1 -MakeInstaller  # 另出 NSIS 安装器（需 makensis）
#
# 形态选择：per-user 安装（免 UAC）+ 应用本地 VC 运行库（用户不用装 VC++ Redist）。
# 引擎与 macOS/Linux 同源（engine-lock.json），布局也一致：engine\audiocpp_server.exe。
#
# 注意：脚本用 UTF-8 输出，Windows 控制台默认代码页会乱码 —— CI 里先 chcp 65001。
param(
    [switch]$MakeInstaller,
    [string]$InstallerVersion = ""
)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$BinName = "audio-workshop"
$ArtifactName = "AudioWorkshop"
$Dist = "dist"
$Stage = Join-Path $Dist "windows-x64"

# 版本从 Cargo.toml 现读（与 macOS/Linux 同一条理由：写死必然漂移）
$version = (Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw "读不到 Cargo.toml 的 version" }

Write-Host "== [1/4] release 构建（静态 ONNX Runtime）=="
# 静态链接 ONNX Runtime：理由见 package.sh 头注释（避免把用户机器上的
# onnxruntime.dll 链进发布包）。Windows 上 ort-sys 同样先试 pkg-config/vcpkg。
$env:LIBONNXRUNTIME_NO_PKG_CONFIG = "1"
cargo build --release --bin $BinName
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

$targetDir = (cargo metadata --format-version 1 --no-deps |
    ConvertFrom-Json).target_directory
$binSrc = Join-Path $targetDir "release\$BinName.exe"
if (-not (Test-Path $binSrc)) { throw "找不到产物：$binSrc" }

Write-Host "== [2/4] 组装 $Stage =="
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Path (Join-Path $Stage "engine") -Force | Out-Null
Copy-Item $binSrc (Join-Path $Stage "$BinName.exe") -Force

Write-Host "== [3/4] 取随包引擎 =="
# Git Bash 里有 curl/shasum/tar，与三个平台共用同一份取件逻辑（含 sha256 校验）
bash packaging/fetch_engine.sh (Join-Path $Stage "engine") windows-x64
if ($LASTEXITCODE -ne 0) { throw "取引擎失败" }

Write-Host "== [4/4] 打 zip =="
$out = Join-Path $Dist "$ArtifactName-$version-windows-x64.zip"
if (Test-Path $out) { Remove-Item $out -Force }
Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $out
Write-Host "   $out"

if ($MakeInstaller) {
    if (-not (Get-Command makensis -ErrorAction SilentlyContinue)) {
        throw "-MakeInstaller 需要 makensis（CI 里 chocolatey install nsis）"
    }
    $v = if ($InstallerVersion) { $InstallerVersion } else { $version }
    Write-Host "== 打 NSIS 安装器 =="
    $setup = Join-Path $Dist "$ArtifactName-$version-windows-x64-setup.exe"
    makensis /DVERSION=$v /DSRCDIR=$Stage /DOUTFILE=$setup packaging/windows-installer.nsi
    if ($LASTEXITCODE -ne 0) { throw "makensis 失败" }
    Write-Host "   $setup"
}
