; 音频作坊 · per-user 安装器（免 UAC）
;
; 为什么 per-user：装到 %LOCALAPPDATA%\Programs 不需要管理员权限，用户双击即装，
; 也让"卸载/升级"只动自己的目录（不在 Program Files 里和别的软件抢）。
; 引擎随包安装到同一目录的 engine\ 下，壳按 exe 同级 engine\ 发现它。
;
; 变量由 package_windows.ps1 传入：VERSION / SRCDIR / OUTFILE
Unicode true
Name "音频作坊"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\AudioWorkshop"
RequestExecutionLevel user
SetCompressor /SOLID lzma

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "install"
  SetOutPath "$INSTDIR"
  ; 壳 + 引擎 + 许可，整体拷贝（含 engine\ 子目录）
  File /r "${SRCDIR}\*.*"
  CreateDirectory "$SMPROGRAMS\音频作坊"
  CreateShortcut "$SMPROGRAMS\音频作坊\音频作坊.lnk" "$INSTDIR\audio-workshop.exe"
  CreateShortcut "$DESKTOP\音频作坊.lnk" "$INSTDIR\audio-workshop.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ; 卸载信息进"应用和功能"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioWorkshop" \
    "DisplayName" "音频作坊"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioWorkshop" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioWorkshop" \
    "UninstallString" "$INSTDIR\uninstall.exe"
SectionEnd

Section "uninstall"
  ; 只删程序目录；用户的工程/模型在 Documents 下，绝不碰
  Delete "$DESKTOP\音频作坊.lnk"
  RMDir /r "$SMPROGRAMS\音频作坊"
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioWorkshop"
SectionEnd
