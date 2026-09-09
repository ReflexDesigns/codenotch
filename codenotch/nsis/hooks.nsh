; Installer hooks.
;
; A running codenotch.exe holds its own file open, so an install that lands on top of it cannot replace
; the binary: NSIS rolls the copy back, relaunches the old build and reports success. That is how 0.5.0
; spent an afternoon impersonating the version that had just been installed. Close the app first — the
; updater already does this before it hands over, so this only covers installs run by hand.
!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /T /IM codenotch.exe'
  Pop $0 ; discarded: "not running" is the normal case, and a first install must not fail on it
  Sleep 1000 ; the file handle outlives the process by a moment
!macroend
