' Hidden launcher - starts NetworkMonitor.ps1 with no visible console window.
' Run elevated (right-click -> Run as administrator) or invoke from a Task Scheduler
' task with "Run with highest privileges".

Dim fso, ws, scriptDir, psPath, ps1
Set fso = CreateObject("Scripting.FileSystemObject")
Set ws  = CreateObject("WScript.Shell")
scriptDir = fso.GetParentFolderName(WScript.ScriptFullName)
ps1 = scriptDir & "\NetworkMonitor.ps1"

' Prefer pwsh.exe if present, fall back to powershell.exe
psPath = "powershell.exe"
If fso.FileExists("C:\Program Files\PowerShell\7\pwsh.exe") Then
    psPath = "C:\Program Files\PowerShell\7\pwsh.exe"
End If

ws.Run """" & psPath & """ -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File """ & ps1 & """", 0, False
