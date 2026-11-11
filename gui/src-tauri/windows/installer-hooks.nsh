!define ALLMYSTUFF_VC_REDIST "${__FILEDIR__}\vc_redist.x64.exe"

!macro NSIS_HOOK_PREINSTALL
  ; The pinned mesh sidecar can still import VCRUNTIME140.dll even when our
  ; own binaries link the CRT statically. Bundle the signed prerequisite so
  ; installation also works offline on a fresh Windows PC.
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File /oname=vc_redist.x64.exe "${ALLMYSTUFF_VC_REDIST}"
  DetailPrint "Installing the Microsoft Visual C++ runtime..."
  ClearErrors
  ExecWait '"$PLUGINSDIR\vc_redist.x64.exe" /install /quiet /norestart' $0
  ${If} ${Errors}
    MessageBox MB_OK|MB_ICONSTOP "Could not start the Microsoft Visual C++ runtime installer. Please run setup again." /SD IDOK
    Abort
  ${EndIf}
  ${If} $0 == 3010
    SetRebootFlag true
  ${ElseIf} $0 != 0
  ${AndIf} $0 != 1638
    MessageBox MB_OK|MB_ICONSTOP "Microsoft Visual C++ runtime installation failed (code $0). Please run setup again." /SD IDOK
    Abort
  ${EndIf}
  SetOutPath "$INSTDIR"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Always On remains disabled until it is enabled in AllMyStuff settings."
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  StrCpy $0 "$INSTDIR\\allmystuff-gui.exe"
  ${IfNot} ${FileExists} "$0"
    StrCpy $0 "$INSTDIR\\AllMyStuff.exe"
  ${EndIf}
  ${If} ${FileExists} "$0"
    ExecWait '"$0" --service-do uninstall' $1
  ${EndIf}
!macroend
