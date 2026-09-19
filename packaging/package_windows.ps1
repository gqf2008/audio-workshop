# 组装 Windows 发行包：可携 zip + Inno Setup 安装器。
#
#   pwsh -File packaging/package_windows.ps1            # 只出 zip
#   pwsh -File packaging/package_windows.ps1 -MakeInstaller  # 另出安装器（需 ISCC.exe）
#
# 形态选择：per-user 安装（免 UAC）+ 应用本地 VC 运行库（用户不用装 VC++ Redist）。
# 引擎与 macOS/Linux 同源（engine-lock.json），布局也一致：engine\audiocpp_server.exe。
#
# 安装器为什么从 NSIS 换成 Inno Setup（2026-09-19 真机反馈）：
#   · NSIS 那版中文乱码 —— .nsi 是不带 BOM 的 UTF-8，makensis 按系统 ANSI（GBK）解释源码；
#     Inno Setup 6 的 .iss 默认就是 UTF-8。理由与取舍写在 packaging/windows-installer.iss 头部。
#   · 与 `../abb` 的 installers 同一套做法（ISCC + .iss），两台机器一套习惯。
#
# 注意：脚本用 UTF-8 输出，Windows 控制台默认代码页会乱码 —— CI 里先 chcp 65001。
param(
    [switch]$MakeInstaller,
    [string]$InstallerVersion = "",
    # 中文消息文件（.isl）的现成路径；留空 = 自己找，找不到就联网取一份。
    # 离线环境 / 想固定某一份翻译时显式传它。
    [string]$InnoLangFile = ""
)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# Windows PowerShell 默认用系统代码页（中文机器上是 GBK）读子进程输出，而 `cargo metadata`
# 与 Python 都按 UTF-8 输出 —— 实测报 `UnicodeDecodeError: 'charmap' codec can't decode
# byte 0x8f`。三处一起设才干净：控制台代码页、PowerShell 的输出编码、以及传给子进程的
# 输入编码。少任何一处都还会在某个子进程上复发。
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
chcp 65001 | Out-Null

$BinName = "audio-workshop"
$ArtifactName = "AudioWorkshop"
$Dist = "dist"
$Stage = Join-Path $Dist "windows-x64"

# 版本从 Cargo.toml 现读（与 macOS/Linux 同一条理由：写死必然漂移）
$cargoToml = [System.IO.File]::ReadAllText((Join-Path (Get-Location) "Cargo.toml"), [System.Text.Encoding]::UTF8)
# MultiLine 必须有：`^` 默认只匹配整个字符串的开头，而 version 在第 3 行 —— 少了这个
# 标志会得到空版本号（CI 实测：读不到 Cargo.toml 的 version）。
$version = ([regex]'(?m)^version = "(.+)"').Match($cargoToml).Groups[1].Value
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

Write-Host "== [2/5] 组装 $Stage =="
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Path (Join-Path $Stage "engine") -Force | Out-Null
Copy-Item $binSrc (Join-Path $Stage "$BinName.exe") -Force
# 图标随包一份：zip（绿色版）里也有，用户手工建快捷方式时不至于没图标可用
$iconSrc = Join-Path (Get-Location) "assets\icon.ico"
if (-not (Test-Path $iconSrc)) { throw "缺少 $iconSrc（生成：python3 tools/gen_app_icon.py）" }
Copy-Item $iconSrc (Join-Path $Stage "$BinName.ico") -Force

Write-Host "== [3/5] 取随包引擎 =="
# Git Bash 里有 curl/shasum/tar，与三个平台共用同一份取件逻辑（含 sha256 校验）
bash packaging/fetch_engine.sh (Join-Path $Stage "engine") windows-x64
if ($LASTEXITCODE -ne 0) { throw "取引擎失败" }

Write-Host "== [4/5] 打 zip =="
$out = Join-Path $Dist "$ArtifactName-$version-windows-x64.zip"
if (Test-Path $out) { Remove-Item $out -Force }
Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $out
Write-Host "   $out"

if ($MakeInstaller) {
    Write-Host "== [5/5] 打 Inno Setup 安装器 =="
    # `ISCC.exe` 常常不在 PATH 上（Inno 安装程序不默认加 PATH；choco 的 shim 又落在
    # C:\ProgramData\chocolatey\bin）。PATH 与三处常见安装目录都找一遍。
    # 不用 `?.`：那是 PS7 语法，脚本要在 Windows PowerShell 5.1 上也能跑。
    $isccCmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    $iscc = if ($isccCmd) { $isccCmd.Source } else { $null }
    if (-not $iscc) {
        $candidates = @()
        if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe") }
        if (${env:ProgramFiles(x86)}) { $candidates += (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe") }
        if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe") }
        $candidates += "C:\ProgramData\chocolatey\bin\ISCC.exe"
        $iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $iscc) {
        throw "-MakeInstaller 需要 ISCC.exe（CI 里 chocolatey install innosetup）—— 找过 PATH 与常见安装目录"
    }

    $stageAbs = (Resolve-Path $Stage).Path
    $distAbs = (Resolve-Path $Dist).Path
    $iconAbs = (Resolve-Path $iconSrc).Path

    # 向导的中文消息文件。**Inno 官方安装包不含中文**（2026-09-19 CI 实测：choco 装的
    # Inno Setup 6.7.1 里根本没有 Languages\ChineseSimplified.isl）；不补一份，ISCC 会
    # 直接失败在 [Languages] 那行（不是静默降级成英文向导）。
    #
    # 补到的是一份**绝对路径**、按 /DLangFile 传给脚本，而不是让 .iss 写
    # `compiler:Languages\...`：choco 装的 Inno 在 PATH 上的 ISCC.exe 是 shim，
    # "ISCC 旁边"不是安装目录 —— 第一版就是这么栽的（把 .isl 下进了 shim 目录，
    # ISCC 仍去 Program Files 找，报 Couldn't open include file）。
    $langFile = $InnoLangFile
    if (-not $langFile) {
        $langCandidates = @()
        if ($env:ProgramFiles) { $langCandidates += (Join-Path $env:ProgramFiles "Inno Setup 6\Languages\ChineseSimplified.isl") }
        if (${env:ProgramFiles(x86)}) { $langCandidates += (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\Languages\ChineseSimplified.isl") }
        if ($env:LOCALAPPDATA) { $langCandidates += (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\Languages\ChineseSimplified.isl") }
        $langFile = $langCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $langFile) {
        # 落到 dist 下：构建产物目录（已被 .gitignore 覆盖，也不会进发布资产）
        $langDir = Join-Path $distAbs "setup-lang"
        New-Item -ItemType Directory -Path $langDir -Force | Out-Null
        $langFile = Join-Path $langDir "ChineseSimplified.isl"
        if (-not (Test-Path $langFile)) {
            $url = "https://raw.githubusercontent.com/jrsoftware/issrc/main/Files/Languages/ChineseSimplified.isl"
            Write-Host "   没有现成的中文消息文件，从官方仓库取一份：$url"
            Invoke-WebRequest -Uri $url -OutFile $langFile
        }
    }
    $langFile = (Resolve-Path $langFile).Path

    $v = if ($InstallerVersion) { $InstallerVersion } else { $version }
    # 输入一律给**绝对路径**：安装器脚本的默认值是相对仓库的（给人手工跑用），
    # 显式传绝对路径就不会被"当前目录 / 脚本目录"的解释差异坑到。
    # 产物名由 .iss 的 OutputBaseFilename 决定（用同一个 $v，别一边 $version 一边 $v）
    $setup = Join-Path $distAbs "$ArtifactName-$v-windows-x64-setup.exe"
    & $iscc "/DMyAppVersion=$v" "/DSourceDir=$stageAbs" "/DIconFile=$iconAbs" `
        "/DOutDir=$distAbs" "/DLangFile=$langFile" "packaging\windows-installer.iss"
    if ($LASTEXITCODE -ne 0) { throw "ISCC 失败" }
    if (-not (Test-Path $setup)) { throw "ISCC 报成功但没产出 $setup" }
    Write-Host "   $setup"

    # 图标守卫：exe 图标 / 快捷方式图标都靠 PE 资源，"没嵌上"能让整条流水线照样绿，
    # 用户看到的却是空白图标（2026-09-19 真机反馈）。有 python 就当场查一遍；
    # 没有 python 时由发布流水线里的同一命令兜底（那里的 python 一定在）。
    # 路径里带 `\WindowsApps\` 的是"应用执行别名"的占位 exe（没装 Python 时也会命中
    # Get-Command），拿它跑检查会报一堆无关错误、看着像产物有问题 —— 按"没有 python"处理
    $py = Get-Command python -ErrorAction SilentlyContinue
    if ($py -and $py.Source -notlike "*\WindowsApps\*") {
        & $py.Source tools\check_windows_icon.py (Join-Path $Stage "$BinName.exe") $setup
        if ($LASTEXITCODE -ne 0) { throw "产物没有图标资源（见上面的检查输出）" }
    } else {
        Write-Host "   ⚠ 没找到 python，跳过图标资源检查（发布流水线会跑同一条）"
    }
}
