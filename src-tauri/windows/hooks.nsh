; Corelume installer hooks (Tauri NSIS). The installer runs elevated (perMachine).

!macro NSIS_HOOK_POSTINSTALL
  ; PawnIO reads CPU temperatures. Install it only when missing: other tools share it.
  SetRegView 64
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Services\PawnIO" "ImagePath"
  SetRegView 32
  ${If} $0 == ""
    DetailPrint "Installing the PawnIO driver (CPU temperatures)..."
    ExecWait '"$INSTDIR\PawnIO_setup.exe" -install -silent' $1
    DetailPrint "PawnIO setup exit code: $1"
  ${Else}
    DetailPrint "PawnIO is already installed."
  ${EndIf}
  ; Elevated start at logon and from the Start menu, without UAC prompts afterwards.
  ExecWait '"$INSTDIR\corelume.exe" --install-tasks' $1
  DetailPrint "Scheduled tasks exit code: $1"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; PawnIO stays: other tools may use it.
  ExecWait '"$INSTDIR\corelume.exe" --remove-tasks'
!macroend
