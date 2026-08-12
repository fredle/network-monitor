@echo off
REM Foreground/debug launcher - keeps a console window for output.
REM For silent background use, run Start-NetworkMonitor.vbs or install the scheduled task.
setlocal
set "DIR=%~dp0"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%DIR%NetworkMonitor.ps1"
endlocal
