; rd agent for Windows: a per-user installer (no admin rights), built by
; .github/workflows/desktop.yml:
;
;   makensis /DVERSION=1.2.0 /DSRC=<folder with rd-agent.exe, rd-agentw.exe and
;            THIRD_PARTY_NOTICES.md> /DOUTFILE=<setup.exe> packaging/windows/rd-agent.nsi
;
; - Installs into %LOCALAPPDATA%\Programs\rd-agent, with Start menu shortcuts and an entry in
;   Settings > Apps.
; - A running agent is stopped first (with the new rd-agent.exe, so it works whatever version or
;   folder the agent runs from). When the agent is already configured (an update), the
;   autostart is installed again from the new folder, which also starts the agent.
; - Otherwise the finish page offers `rd-agent setup` (paste the setup code from the portal's
;   Add device, or enter the gateway, machine id and token), which turns the autostart on.
; - Silent: `setup.exe /S` (add `/D=<folder>` last to pick the folder); uninstall with
;   `uninstall.exe /S`. Silent uninstalls keep the configuration.
; - Uninstalling stops the agent and removes the autostart. The configuration (with the agent
;   token) and the logs in %LOCALAPPDATA%\rd-agent stay unless the user removes them.

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "pass /DVERSION=X.Y.Z"
!endif
!ifndef SRC
  !error "pass /DSRC=<folder with rd-agent.exe, rd-agentw.exe and THIRD_PARTY_NOTICES.md>"
!endif
!ifndef OUTFILE
  !define OUTFILE "rd-agent-${VERSION}-windows-x64-setup.exe"
!endif

!define NAME "rd agent"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rd-agent"
; makensis resolves relative paths from this script's folder.
!define ICON "..\..\clients\desktop\src-tauri\icons\icon.ico"
!define DATA "$LOCALAPPDATA\rd-agent"
!define SHORTCUTS "$SMPROGRAMS\rd agent"

Name "${NAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\rd-agent"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
BrandingText "${NAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${NAME} setup"
VIAddVersionKey "LegalCopyright" "rd"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "nsDialogs.nsh"
!include "x64.nsh"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TEXT "This installs ${NAME} ${VERSION}, the host side of rd: it lets you reach this PC from a Mac or a browser through your rd gateway.$\r$\n$\r$\nIt installs for your Windows account only and needs no administrator rights. For the setup at the end, add this PC in your rd portal (Add device) and keep its setup code at hand."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Set up ${NAME} now (paste the setup code)"
!define MUI_FINISHPAGE_RUN_FUNCTION RunSetup
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Open the setup guide"
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION OpenGuide
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; The agent was configured before this install (an update or a reinstall).
Var Configured

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${NAME} needs 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  ; Without Media Foundation rd-agent.exe does not start (Windows N editions, Windows Server).
  ${DisableX64FSRedirection}
  ${IfNot} ${FileExists} "$WINDIR\System32\mfplat.dll"
    ${EnableX64FSRedirection}
    MessageBox MB_ICONEXCLAMATION|MB_OKCANCEL "${NAME} needs Media Foundation, which this Windows does not have.$\r$\n$\r$\nOn an N edition, install the Media Feature Pack first (Settings > Apps > Optional features); on Windows Server, the Server Media Foundation feature.$\r$\n$\r$\nInstall anyway?" /SD IDOK IDOK +2
    Abort
  ${EndIf}
  ${EnableX64FSRedirection}
FunctionEnd

Section "${NAME}" Main
  SectionIn RO

  ${If} ${FileExists} "${DATA}\config.toml"
    StrCpy $Configured "1"
  ${EndIf}

  ; Stop a running agent and remove its autostart, so its files can be replaced. Uses the new
  ; rd-agent.exe from a temporary folder: the agent may run from an older install or elsewhere.
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File "${SRC}\rd-agent.exe"
  DetailPrint "Stopping a running rd agent"
  nsExec::ExecToLog '"$PLUGINSDIR\rd-agent.exe" uninstall'
  Pop $0

  SetOutPath "$INSTDIR"
  File "${SRC}\rd-agent.exe"
  File "${SRC}\rd-agentw.exe"
  File "${SRC}\THIRD_PARTY_NOTICES.md"
  File "/oname=rd-agent.ico" "${ICON}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; rd-agent.exe is a console program: the shortcuts keep its window open afterwards.
  CreateDirectory "${DATA}\logs"
  CreateDirectory "${SHORTCUTS}"
  CreateShortcut "${SHORTCUTS}\rd agent setup.lnk" "$WINDIR\System32\cmd.exe" '/k ""$INSTDIR\rd-agent.exe" setup"' "$INSTDIR\rd-agent.ico"
  CreateShortcut "${SHORTCUTS}\rd agent status.lnk" "$WINDIR\System32\cmd.exe" '/k ""$INSTDIR\rd-agent.exe" status"' "$INSTDIR\rd-agent.ico"
  CreateShortcut "${SHORTCUTS}\rd agent logs.lnk" "${DATA}\logs"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "rd"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\rd-agent.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/Thijmen/remoted"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"

  ; An update: turn the autostart on again (from this folder), which also starts the agent.
  ${If} $Configured == "1"
    DetailPrint "Starting rd agent"
    nsExec::ExecToLog '"$INSTDIR\rd-agent.exe" install'
    Pop $0
  ${EndIf}
SectionEnd

Function FinishShow
  ; Already configured: the agent is running again; setup stays available from the Start menu.
  ${If} $Configured == "1"
    ${NSD_Uncheck} $mui.FinishPage.Run
  ${EndIf}
FunctionEnd

Function RunSetup
  Exec '"$WINDIR\System32\cmd.exe" /k ""$INSTDIR\rd-agent.exe" setup"'
FunctionEnd

Function OpenGuide
  ExecShell "open" "https://github.com/Thijmen/remoted/blob/main/docs/setup-windows.md"
FunctionEnd

Section "Uninstall"
  DetailPrint "Stopping rd agent and removing its autostart"
  nsExec::ExecToLog '"$INSTDIR\rd-agent.exe" uninstall'
  Pop $0

  Delete "$INSTDIR\rd-agent.exe"
  Delete "$INSTDIR\rd-agentw.exe"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
  Delete "$INSTDIR\rd-agent.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "${SHORTCUTS}\rd agent setup.lnk"
  Delete "${SHORTCUTS}\rd agent status.lnk"
  Delete "${SHORTCUTS}\rd agent logs.lnk"
  RMDir "${SHORTCUTS}"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"

  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also remove the rd agent configuration (with the agent token) and its logs?$\r$\n$\r$\n${DATA}" /SD IDNO IDNO keep
    RMDir /r "${DATA}"
  keep:
SectionEnd
