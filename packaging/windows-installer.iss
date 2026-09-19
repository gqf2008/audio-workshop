; 音频作坊 · Windows 安装器（Inno Setup 6，per-user 免 UAC）
;
; 为什么是 Inno Setup、而不是之前的 NSIS（2026-09-19 真机反馈后换的）：
;   · NSIS 那版的中文是乱码 —— .nsi 存成不带 BOM 的 UTF-8，而 makensis 对无 BOM 的脚本
;     按**系统 ANSI 代码页**（中文 Windows 上是 GBK）解释源码，应用名/快捷方式名/卸载条目
;     全变成"锟斤拷"。Inno Setup 6 起 .iss 默认按 UTF-8 读（有无 BOM 都行），不再踩这条。
;   · 托盘/安装器图标、桌面图标、开始菜单图标在 Inno 里是一等公民（SetupIconFile +
;     [Icons].IconFilename），不必手写注册表与快捷方式。
; 形态与 `../abb` 的 `app-assets/ABB.iss` 一致：装到 %LOCALAPPDATA%\Programs，免 UAC、
; 不动系统目录、卸载只删自己（用户的工程与模型在 Documents 下，绝不碰）。
;
; ⚠ 本文件必须保持 **UTF-8**（不要"另存为 ANSI"），否则下面那些中文又会变成乱码。
;
; 变量由 packaging/package_windows.ps1 传入（/D），默认值是给手工跑留的：
;   MyAppVersion  版本号（从 Cargo.toml 现读，不写死 —— 写死必然漂移）
;   SourceDir     已组装好的发行目录（壳 + engine\ + 图标）
;   IconFile      assets/icon.ico（安装器图标 + 装进 {app} 给快捷方式用）
;   OutDir        产物目录（默认仓库根的 dist\）
;   LangFile      中文消息文件（.isl）的**绝对路径**。默认值是"Inno 安装目录里的那份"，
;                 但 Inno 官方安装包与 choco 包都**不带中文**（2026-09-19 CI 实测），
;                 实际由 package_windows.ps1 找/下载一份再传进来 —— 别手工直接跑 ISCC，
;                 除非你自己给 LangFile（`ISCC /DLangFile=... windows-installer.iss`）。

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\windows-x64"
#endif
#ifndef IconFile
  #define IconFile "..\assets\icon.ico"
#endif
#ifndef OutDir
  #define OutDir "..\dist"
#endif
#ifndef LangFile
  #define LangFile "compiler:Languages\ChineseSimplified.isl"
#endif

#define MyAppName "音频作坊"
#define MyAppPublisher "SQB"
#define MyAppExeName "audio-workshop.exe"
#define MyAppIconName "audio-workshop.ico"

[Setup]
; AppId 是"同一个应用"的身份：改了它等于换了一个应用，装过的用户会看到两份、升级也认不出来
AppId={{8828E570-A85A-4325-A3EA-6423AE4DF680}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
; 与之前 NSIS 那版同一个目录：升级时原地覆盖，不会在机器上留两套
DefaultDirName={localappdata}\Programs\AudioWorkshop
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
AllowNoIcons=yes
; per-user：免 UAC，卸载/升级只动自己的目录
PrivilegesRequired=lowest
OutputDir={#OutDir}
OutputBaseFilename=AudioWorkshop-{#MyAppVersion}-windows-x64-setup
; 安装器自己的图标（用户在"下载"文件夹里看到的就是它）
SetupIconFile={#IconFile}
; 应用和功能里卸载条目显示的图标
UninstallDisplayIcon={app}\{#MyAppIconName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
; 中文放第一个 = 向导默认中文（本产品面向中文用户）；英文保留给排障
Name: "chinesesimplified"; MessagesFile: "{#LangFile}"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; 桌面图标默认勾上：上一版 NSIS 是无条件创建，用户反馈里就是"桌面图标"这一条
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; 壳单独列一条：文件不在时**编译期就报错**（含通配符的那条匹配不到会是静默少文件）
Source: "{#SourceDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
; 图标单独装一份：快捷方式/卸载条目都指它（exe 里那份是 Explorer 显示用的）
Source: "{#IconFile}"; DestDir: "{app}"; DestName: "{#MyAppIconName}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppIconName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppIconName}"; Tasks: desktopicon

[InstallDelete]
; 上一版（NSIS）的卸载器叫 uninstall.exe，Inno 的是 unins000.exe —— 不删就会留一个
; 指向"已经不存在的旧安装"的孤儿卸载器
Type: files; Name: "{app}\uninstall.exe"

[Registry]
; 上一版 NSIS 手写的"应用和功能"条目。不删的话用户会看到两个「音频作坊」
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioWorkshop"; Flags: deletekey

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
