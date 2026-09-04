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
