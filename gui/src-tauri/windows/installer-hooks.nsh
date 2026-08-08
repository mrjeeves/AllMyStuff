!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Installing the AllMyStuff privileged desktop host..."
  StrCpy $0 "$INSTDIR\\allmystuff-gui.exe"
  ${IfNot} ${FileExists} "$0"
    StrCpy $0 "$INSTDIR\\AllMyStuff.exe"
  ${EndIf}
  ${If} ${FileExists} "$0"
    ExecWait '"$0" --service-bootstrap install' $1
    ${If} $1 != 0
      MessageBox MB_ICONEXCLAMATION|MB_OK "AllMyStuff was installed, but its privileged desktop host could not be enabled. Open AllMyStuff to try again."
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  StrCpy $0 "$INSTDIR\\allmystuff-gui.exe"
  ${IfNot} ${FileExists} "$0"
    StrCpy $0 "$INSTDIR\\AllMyStuff.exe"
  ${EndIf}
  ${If} ${FileExists} "$0"
    ExecWait '"$0" --service-bootstrap uninstall' $1
  ${EndIf}
!macroend
