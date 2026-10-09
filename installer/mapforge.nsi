; MapForge Windows installer. Build with:
;   makensis /DVERSION=0.1.0 installer\mapforge.nsi
; It expects dist\ to hold MapForge-Player.exe, MapForge-Producer.exe,
; ffmpeg.exe, ffprobe.exe and FFmpeg-LICENSE.txt.
;
; With /DUPDATE_ONLY it makes MapForge-Update-<version>.exe instead: only the
; MapForge programs, without FFmpeg, for PCs that already have MapForge.

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.1.0"
!endif
; makensis works from this script's folder, so dist\ is one level up.
!define DIST "..\dist"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\MapForge"

Name "MapForge ${VERSION}"
!ifdef UPDATE_ONLY
  OutFile "${DIST}\MapForge-Update-${VERSION}.exe"
!else
  OutFile "${DIST}\MapForge-Setup-${VERSION}.exe"
!endif
InstallDir "$PROGRAMFILES64\MapForge"
InstallDirRegKey HKLM "Software\MapForge" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "This installs MapForge on this PC.$\r$\n$\r$\nOn every show PC, install the Player. On the PC where you design the show, also install the Producer.$\r$\n$\r$\nThe first time the Player opens, it asks whether this PC is the Master or a Sub."
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_FINISHPAGE_RUN "$INSTDIR\MapForge-Player.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open MapForge Player now"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
!ifdef UPDATE_ONLY
  IfFileExists "$INSTDIR\ffmpeg.exe" +3
    MessageBox MB_ICONSTOP "MapForge is not installed on this PC yet.$\r$\n$\r$\nRun MapForge-Setup first; this update only replaces the MapForge programs."
    Abort
!endif
  ; The programs can't be replaced while they run. The Player keeps its show
  ; on disk, so it is simply closed; the Producer may hold unsaved work.
  nsExec::Exec 'taskkill /F /IM MapForge-Player.exe'
  check_producer:
  ; `find` exits with 0 when the Producer is in the task list.
  nsExec::Exec 'cmd /c tasklist /NH | find /I "MapForge-Producer.exe"'
  Pop $0
  ${If} $0 == 0
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "MapForge Producer is open. Save your show and close it, then click OK." IDOK check_producer
    Abort
  ${EndIf}
FunctionEnd

Section "MapForge Player (every show PC)" SecPlayer
  SectionIn RO
  SetShellVarContext all
  SetOutPath "$INSTDIR"
  File "${DIST}\MapForge-Player.exe"
!ifndef UPDATE_ONLY
  File "${DIST}\ffmpeg.exe"
  File "${DIST}\ffprobe.exe"
  File "${DIST}\FFmpeg-LICENSE.txt"
!endif

  CreateDirectory "$SMPROGRAMS\MapForge"
  CreateShortcut "$SMPROGRAMS\MapForge\MapForge Player.lnk" "$INSTDIR\MapForge-Player.exe"
  CreateShortcut "$DESKTOP\MapForge Player.lnk" "$INSTDIR\MapForge-Player.exe"

  ; Let Producer, the other show PCs and the iPad reach the Player
  ; (TCP 4777 and 8080). A show LAN with no internet often counts as a
  ; public network in Windows, so the rule covers every profile.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="MapForge Player"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="MapForge Player" dir=in action=allow program="$INSTDIR\MapForge-Player.exe" enable=yes profile=any'

  WriteUninstaller "$INSTDIR\Uninstall MapForge.exe"
  CreateShortcut "$SMPROGRAMS\MapForge\Uninstall MapForge.lnk" "$INSTDIR\Uninstall MapForge.exe"
  WriteRegStr HKLM "Software\MapForge" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "MapForge"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "MapForge"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\MapForge-Player.exe"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall MapForge.exe"'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "MapForge Producer (design PC)" SecProducer
  SetShellVarContext all
  SetOutPath "$INSTDIR"
  File "${DIST}\MapForge-Producer.exe"
  nsExec::Exec 'netsh advfirewall firewall delete rule name="MapForge Producer"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="MapForge Producer" dir=in action=allow program="$INSTDIR\MapForge-Producer.exe" enable=yes profile=any'
  CreateShortcut "$SMPROGRAMS\MapForge\MapForge Producer.lnk" "$INSTDIR\MapForge-Producer.exe"
  CreateShortcut "$DESKTOP\MapForge Producer.lnk" "$INSTDIR\MapForge-Producer.exe"
SectionEnd

Section "Open the Player when Windows starts (show PCs)" SecStartup
  SetShellVarContext all
  CreateShortcut "$SMSTARTUP\MapForge Player.lnk" "$INSTDIR\MapForge-Player.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecPlayer} "Plays the show on this PC's projectors. Needed on every show PC."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecProducer} "Designs the show: media, timeline, cues, loops and projectors."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartup} "Show PCs: the Player opens and starts the show by itself after the PC turns on."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  SetShellVarContext all
  nsExec::Exec 'netsh advfirewall firewall delete rule name="MapForge Player"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="MapForge Producer"'
  Delete "$SMSTARTUP\MapForge Player.lnk"
  Delete "$DESKTOP\MapForge Player.lnk"
  Delete "$DESKTOP\MapForge Producer.lnk"
  RMDir /r "$SMPROGRAMS\MapForge"
  Delete "$INSTDIR\MapForge-Player.exe"
  Delete "$INSTDIR\MapForge-Producer.exe"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\ffprobe.exe"
  Delete "$INSTDIR\FFmpeg-LICENSE.txt"
  Delete "$INSTDIR\Uninstall MapForge.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
  DeleteRegKey HKLM "Software\MapForge"
  ; Show media and the Master/Sub setup in "MapForge Media" are kept.
SectionEnd
