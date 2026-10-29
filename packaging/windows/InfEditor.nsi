; InfEditor Windows EXE installer (NSIS 3).
;
; Build (from the repo root, see .github/workflows/release.yml):
;   makensis -DVERSION=0.1.0 -DBINDIR=target\x86_64-pc-windows-msvc\release ^
;            -DOUTDIR=dist packaging\windows\InfEditor.nsi
; OUTDIR is where the setup exe lands — dist/ at the repo root, the one place
; every package goes (MSI, zips, DEB, see release.yml).
;
; makensis changes its working directory to the directory of this script
; (`/NOCD` disables that), so a bare relative path such as
; "packaging\windows\infeditor.ico" is resolved *inside* packaging/windows/
; and fails. `${__FILEDIR__}` does not help here: it keeps the directory
; **as written on the command line** (relative when the script is passed as
; `packaging\windows\InfEditor.nsi`). Instead, move up to the repo root at
; compile time and keep every path repo-relative, matching the command above.
; (Do not invoke makensis with /NOCD when using this script.)
;
; MSI vs EXE: same payload, different deployment story. The MSI is the
; managed/enterprise path (per-machine, major upgrade); this NSIS setup is the
; consumer path with a friendlier wizard. Only x64 is built (aarch64 ships as
; a zip; NSIS has no official arm64 toolchain).
;
; Both installers put the two decisions the user actually cares about on
; **pages of their own** rather than folding them into a component list:
;   License -> Install folder (changeable) -> File associations -> Installing
; The associations page is a plain checkbox (checked by default): it writes
; the same per-user (HKCU) keys the editor's own Preferences -> File Types
; flow writes — ProgID `InfEditor_file`, `Applications\infeditor.exe` +
; Capabilities, and one `OpenWithProgids` entry per extension in
; COMMON_EXTS (keep that list in sync with packaging/windows/InfEditor.wxs).
; It deliberately does NOT take the default handler: Windows 10+ keeps the
; effective default in a per-user UserChoice hash no installer can set
; honestly; per-group take-over (with PrevDefault archive/restore) stays in
; the editor.
;
; The MSI (InfEditor.wxs) mirrors this with WixUI_InstallDir + a custom
; `AssocDlg`; keep the wording and the extension list of the two in step.
;
; A silent install (/S) skips every page; $AssocState is seeded to "1" in
; .onInit, so /S installs the associations exactly like the default answer.

Unicode true
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

; makensis already changed into packaging/windows/ (see the header). Step up to
; the repo root once, so every path below is repo-relative, BINDIR/OUTDIR from
; the command line included, no matter whether the caller passed the script by
; an absolute or a relative path.
!cd ..\..

!ifndef VERSION
  !define VERSION "0.0.0-dev"
!endif
!ifndef BINDIR
  !define BINDIR "target\release"
!endif
!ifndef OUTDIR
  !define OUTDIR "dist"
!endif
; makensis does not create the output directory; make sure it exists before
; OutFile opens the file (this runs at compile time, CWD is the repo root).
!system 'if not exist "${OUTDIR}" mkdir "${OUTDIR}"'

!define APPNAME "InfEditor"
!define COMPANY "InfEditor Developers"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}"

Name "${APPNAME} ${VERSION}"
OutFile "${OUTDIR}\InfEditor-${VERSION}-x64-setup.exe"
InstallDir "$PROGRAMFILES64\${APPNAME}"
; Reuse a previous install location (set at first install below).
InstallDirRegKey HKCU "Software\${APPNAME}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!define MUI_ABORTWARNING
!define MUI_ICON "packaging\windows\infeditor.ico"
!define MUI_UNICON "packaging\windows\infeditor.ico"
!insertmacro MUI_PAGE_LICENSE "packaging\windows\license.rtf"
; Step 1: where to install (MUI's own directory page, with a Browse... button).
!insertmacro MUI_PAGE_DIRECTORY
; Step 2: file associations — a page of its own instead of a component
; checkbox, so it is read as one question with one answer. The page function
; only records the answer in $AssocState; the section below does the writing.
Page custom AssocPageShow AssocPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

; ---- Localized strings for the associations page ----------------------------
LangString AssocPageTitle ${LANG_ENGLISH} "File associations"
LangString AssocPageTitle ${LANG_SIMPCHINESE} "文件关联"
LangString AssocPageSub ${LANG_ENGLISH} "Choose how ${APPNAME} registers itself for common document types."
LangString AssocPageSub ${LANG_SIMPCHINESE} "选择 ${APPNAME} 对常见文档类型的注册方式。"
LangString AssocCheck ${LANG_ENGLISH} "Add ${APPNAME} to the Open with menu for common document types"
LangString AssocCheck ${LANG_SIMPCHINESE} "把 ${APPNAME} 加入常见文档类型的「打开方式」菜单"
LangString AssocNote ${LANG_ENGLISH} "Only the Open with menu entry is added — no default handler is taken over. Per-type defaults can be changed later in the editor under Preferences, File Types.$\r$\n$\r$\n.txt  .md  .markdown  .log  .csv  .tsv  .json  .jsonl  .ndjson  .xml  .yml  .yaml  .toml  .ini"
LangString AssocNote ${LANG_SIMPCHINESE} "只添加「打开方式」菜单项，不接管任何默认打开方式。之后可以在 首选项 - 文件类型 里改为按类型接管默认程序。$\r$\n$\r$\n.txt  .md  .markdown  .log  .csv  .tsv  .json  .jsonl  .ndjson  .xml  .yml  .yaml  .toml  .ini"

; The checkbox state across Back/Next, and the control handle the page
; callback hands to nsDialogs. Seeded to "1" (checked) in .onInit.
Var AssocCheckbox
Var AssocState

; ---- Common file associations ------------------------------------------------
;
; The registry footprint mirrors platform::assoc (HKCU, ProgID
; `InfEditor_file`): no default is taken here, only "Open with" +
; capability-manifest entries. Keep the extension list in sync with
; packaging/windows/InfEditor.wxs.
;
; `OpenWithProgids` is written as an empty REG_SZ: the editor writes REG_NONE,
; but the shell and the editor both test *existence*, never the type.

!macro AssocWrite EXT
  WriteRegStr HKCU "Software\Classes\${EXT}\OpenWithProgids" "InfEditor_file" ""
  WriteRegStr HKCU "Software\InfEditor\Capabilities\FileAssociations" "${EXT}" "InfEditor_file"
  WriteRegStr HKCU "Software\Classes\Applications\infeditor.exe\Capabilities\FileAssociations" "${EXT}" "InfEditor_file"
!macroend

!macro AssocUnwrite EXT
  ; If the installer's ProgID is still the extension's default handler, give
  ; the extension back (mirroring the editor's PrevDefault restore protocol).
  ReadRegStr $0 HKCU "Software\Classes\${EXT}" ""
  ${If} $0 == "InfEditor_file"
    ReadRegStr $1 HKCU "Software\InfEditor\PrevDefault" "${EXT}"
    ${If} $1 == ""
      DeleteRegValue HKCU "Software\Classes\${EXT}" ""
    ${Else}
      WriteRegStr HKCU "Software\Classes\${EXT}" "" $1
    ${EndIf}
  ${EndIf}
  DeleteRegValue HKCU "Software\Classes\${EXT}\OpenWithProgids" "InfEditor_file"
  DeleteRegValue HKCU "Software\InfEditor\Capabilities\FileAssociations" "${EXT}"
  DeleteRegValue HKCU "Software\Classes\Applications\infeditor.exe\Capabilities\FileAssociations" "${EXT}"
!macroend

; Make explorer re-read associations immediately.
!macro NotifyShell
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0x1000, p 0, p 0)'
!macroend

; x64-only build (see header note).
Function .onInit
  ; Associations are on unless the user unticks the box on their page; /S
  ; never shows that page, so the seed is what a silent install gets.
  StrCpy $AssocState "1"
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "${APPNAME} requires a 64-bit Windows."
    Abort
  ${EndIf}
FunctionEnd

; ---- File associations page (called by `Page custom` above) -----------------
;
; Callbacks only read/write $AssocState; every registry write is in the
; association section below, so "page cancelled / silent install / section
; skipped by condition" all have exactly one code path.

Function AssocPageShow
  !insertmacro MUI_HEADER_TEXT "$(AssocPageTitle)" "$(AssocPageSub)"
  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    ; Should never happen; a blank page beats a half-built one.
    Abort
  ${EndIf}

  ${NSD_CreateCheckbox} 0 0 100% 12u "$(AssocCheck)"
  Pop $AssocCheckbox
  ${If} $AssocState == "1"
    ${NSD_Check} $AssocCheckbox
  ${Else}
    ${NSD_Uncheck} $AssocCheckbox
  ${EndIf}

  ; Extension list + the "no default handler" note, on an unselectable label.
  ${NSD_CreateLabel} 0 26u 100% 60u "$(AssocNote)"
  Pop $0

  nsDialogs::Show
FunctionEnd

Function AssocPageLeave
  ${NSD_GetState} $AssocCheckbox $0
  StrCpy $AssocState $0
FunctionEnd

Section "InfEditor" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${BINDIR}\infeditor.exe"
  File "/oname=infeditor.ico" "packaging\windows\infeditor.ico"
  File "README.md"
  File "README.zh-CN.md"
  File "LICENSE"

  ; Remember the chosen directory for InstallDirRegKey.
  WriteRegStr HKCU "Software\${APPNAME}" "InstallDir" "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" \
    "$INSTDIR\infeditor.exe" "" "$INSTDIR\infeditor.ico" 0
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" \
    "$INSTDIR\infeditor.exe" "" "$INSTDIR\infeditor.ico" 0

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Add/Remove Programs entry.
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME} ${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\infeditor.ico"
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINSTKEY}" "HelpLink" "https://github.com/luckyvermeil/infeditor"
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1

  ; Rough installed size (KB) for the ARP listing.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${UNINSTKEY}" "EstimatedSize" $0
SectionEnd

; File associations: written only when the associations page left the box
; ticked ($AssocState == "1"; it is "0" only after an explicit unticks).
Section "FileAssociations" SecAssoc
  ${If} $AssocState != "0"
  !insertmacro AssocWrite ".txt"
  !insertmacro AssocWrite ".md"
  !insertmacro AssocWrite ".markdown"
  !insertmacro AssocWrite ".log"
  !insertmacro AssocWrite ".csv"
  !insertmacro AssocWrite ".tsv"
  !insertmacro AssocWrite ".json"
  !insertmacro AssocWrite ".jsonl"
  !insertmacro AssocWrite ".ndjson"
  !insertmacro AssocWrite ".xml"
  !insertmacro AssocWrite ".yml"
  !insertmacro AssocWrite ".yaml"
  !insertmacro AssocWrite ".toml"
  !insertmacro AssocWrite ".ini"

  ; The ProgID / Applications / Capabilities scaffolding (same keys the
  ; editor's own registration flow writes).
  WriteRegStr HKCU "Software\Classes\InfEditor_file\shell\open\command" "" '"$INSTDIR\infeditor.exe" "%1"'
  WriteRegStr HKCU "Software\Classes\InfEditor_file" "FriendlyTypeName" "InfEditor Document"
  WriteRegStr HKCU "Software\Classes\InfEditor_file" "DefaultIcon" '"$INSTDIR\infeditor.ico"'
  WriteRegStr HKCU "Software\Classes\Applications\infeditor.exe\shell\open\command" "" '"$INSTDIR\infeditor.exe" "%1"'
  WriteRegStr HKCU "Software\Classes\Applications\infeditor.exe" "FriendlyAppName" "${APPNAME}"
  WriteRegStr HKCU "Software\Classes\Applications\infeditor.exe" "DefaultIcon" '"$INSTDIR\infeditor.ico"'
  WriteRegStr HKCU "Software\Classes\Applications\infeditor.exe\Capabilities" "ApplicationName" "${APPNAME}"
  WriteRegStr HKCU "Software\InfEditor" "Capabilities" "Software\InfEditor\Capabilities"
  WriteRegStr HKCU "Software\InfEditor\Capabilities" "ApplicationName" "${APPNAME}"
  WriteRegStr HKCU "Software\InfEditor\Capabilities" "ApplicationDescription" "WYSIWYG Markdown / text editor"

  !insertmacro NotifyShell
  ${EndIf}
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\infeditor.exe"
  Delete "$INSTDIR\infeditor.ico"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\README.zh-CN.md"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"

  ; Undo the optional associations (and anything the editor added on top of
  ; them) before the Software\InfEditor key below is wiped — PrevDefault
  ; restoration reads it.
  !insertmacro AssocUnwrite ".txt"
  !insertmacro AssocUnwrite ".md"
  !insertmacro AssocUnwrite ".markdown"
  !insertmacro AssocUnwrite ".log"
  !insertmacro AssocUnwrite ".csv"
  !insertmacro AssocUnwrite ".tsv"
  !insertmacro AssocUnwrite ".json"
  !insertmacro AssocUnwrite ".jsonl"
  !insertmacro AssocUnwrite ".ndjson"
  !insertmacro AssocUnwrite ".xml"
  !insertmacro AssocUnwrite ".yml"
  !insertmacro AssocUnwrite ".yaml"
  !insertmacro AssocUnwrite ".toml"
  !insertmacro AssocUnwrite ".ini"
  DeleteRegKey HKCU "Software\Classes\InfEditor_file"
  DeleteRegKey HKCU "Software\Classes\Applications\infeditor.exe"
  DeleteRegKey HKCU "Software\InfEditor\Capabilities"
  !insertmacro NotifyShell

  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKCU "Software\${APPNAME}"
  ; User data (settings, recovery sessions under %APPDATA%) is deliberately
  ; kept: a reinstall or an upgrade must not lose documents' metadata.
SectionEnd
