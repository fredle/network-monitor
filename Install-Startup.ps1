#requires -Version 5.1
#requires -RunAsAdministrator
# Registers a scheduled task that starts NetworkMonitor.vbs at logon with highest privileges.
# Re-run to update. Use Uninstall-Startup.ps1 to remove.

$ErrorActionPreference = 'Stop'
$taskName  = 'NetworkMonitor'
$here      = $PSScriptRoot
$launcher  = Join-Path $here 'Start-NetworkMonitor.vbs'

if (-not (Test-Path $launcher)) { throw "Launcher not found: $launcher" }

$action    = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument ('"{0}"' -f $launcher) -WorkingDirectory $here
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User ([Security.Principal.WindowsIdentity]::GetCurrent().Name)
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Write-Host "Registered scheduled task '$taskName' (runs at logon, highest privileges)."
Write-Host "To start now: Start-ScheduledTask -TaskName $taskName"
Write-Host "To inspect:  Get-ScheduledTask -TaskName $taskName"
