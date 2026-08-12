#requires -Version 5.1
# Network Monitor - tray app
# - Continuously pings 8.8.8.8 (configurable)
# - Watches the System log for Netwtw14/16 driver faults and notifies
# - Re-roams the Wi-Fi adapter when a significantly better BSSID is available
#
# Run elevated. See README.md for auto-start setup.

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# ----- Config (override via config.json next to this script) ---------------
$script:Config = @{
    Target              = '8.8.8.8'   # ping target
    PingIntervalMs      = 1000        # ping every Nms
    WifiStateIntervalMs = 5000        # refresh Wi-Fi state
    ScanIntervalMs      = 30000       # scan BSSIDs + decide roam
    EventIntervalMs     = 60000       # check Netwtw events
    AutoRoam            = $true       # master switch for automatic re-roaming (manual is always allowed)
    RoamThresholdDb     = 12          # roam if best BSSID is >= N dB stronger than current
    MinRssiToConsider   = -72         # only auto-roam if current RSSI is below this (dBm)
    SSIDOverride        = ''          # blank = follow whatever SSID we're connected to
    PingHistoryMax      = 3600        # in-memory ping samples (hard cap)
    PingHistoryMinutes  = 10          # drop samples older than this many minutes
    LogDir              = "$PSScriptRoot\logs"
}

$cfgFile = Join-Path $PSScriptRoot 'config.json'
if (Test-Path $cfgFile) {
    try {
        $loaded = Get-Content $cfgFile -Raw | ConvertFrom-Json
        foreach ($p in $loaded.PSObject.Properties) {
            if ($script:Config.ContainsKey($p.Name)) { $script:Config[$p.Name] = $p.Value }
        }
    } catch {}
}

if (-not (Test-Path $script:Config.LogDir)) {
    New-Item -ItemType Directory -Path $script:Config.LogDir -Force | Out-Null
}

# ----- State --------------------------------------------------------------
$script:State = @{
    LastPingMs       = $null
    PingHistory      = New-Object System.Collections.Generic.Queue[object]
    Drops            = 0
    HighLatency      = 0
    Total            = 0

    SSID             = $null
    BSSID            = $null
    Band             = $null
    Channel          = $null
    SignalPct        = $null
    RSSI             = $null
    RxRate           = $null
    TxRate           = $null
    LinkLossCount    = 0

    NetwtwLastSeen   = (Get-Date)
    NetwtwCount      = 0

    Reroaming        = $false
    ReroamProc       = $null
    ReroamCheckTimer = $null
    LastReroamAt     = [DateTime]::MinValue
    Roams            = 0

    AdapterName      = $null
    CurrentIconState = ''
    LastIconHandle   = [IntPtr]::Zero
}

# ----- Logging ------------------------------------------------------------
function Get-LogFile {
    Join-Path $script:Config.LogDir ("netmon-{0}.log" -f (Get-Date -Format 'yyyyMMdd'))
}
function Write-Log {
    param([string]$Message, [string]$Level = 'INFO')
    $line = "{0} {1,-5} {2}" -f (Get-Date -Format 'HH:mm:ss.fff'), $Level, $Message
    try { Add-Content -Path (Get-LogFile) -Value $line -ErrorAction Stop } catch {}
}

# ----- Single-instance guard ----------------------------------------------
# Held for the life of the process; a second copy fails to acquire and exits.
# Stored at script scope so the mutex is not garbage-collected.
$createdNew = $false
$script:SingleInstanceMutex = New-Object System.Threading.Mutex($true, 'Local\NetworkMonitor.TrayApp.SingleInstance', [ref]$createdNew)
if (-not $createdNew) {
    Write-Log 'Another instance is already running; exiting.' 'WARN'
    exit 0
}

# ----- Wi-Fi helpers ------------------------------------------------------
function Get-WifiAdapter {
    Get-NetAdapter -ErrorAction SilentlyContinue |
        Where-Object { $_.MediaType -like '*802.11*' } |
        Sort-Object { if ($_.Status -eq 'Up') { 0 } else { 1 } } |
        Select-Object -First 1
}

function Get-WifiStatus {
    $out = & netsh.exe wlan show interfaces 2>$null
    $s = [ordered]@{
        State=$null; SSID=$null; BSSID=$null; Band=$null; Channel=$null
        Signal=$null; RSSI=$null; RxRate=$null; TxRate=$null
    }
    foreach ($l in $out) {
        if     ($l -match '^\s+State\s+:\s+(.+?)\s*$')                       { $s.State    = $matches[1] }
        elseif ($l -match '^\s+SSID\s+:\s+(.+?)\s*$')                        { $s.SSID     = $matches[1] }
        elseif ($l -match '^\s+AP BSSID\s+:\s+(.+?)\s*$')                    { $s.BSSID    = $matches[1] }
        elseif ($l -match '^\s+Band\s+:\s+(.+?)\s*$')                        { $s.Band     = $matches[1] }
        elseif ($l -match '^\s+Channel\s+:\s+(\d+)')                         { $s.Channel  = [int]$matches[1] }
        elseif ($l -match '^\s+Signal\s+:\s+(\d+)%')                         { $s.Signal   = [int]$matches[1] }
        elseif ($l -match '^\s+Rssi\s+:\s+(-?\d+)')                          { $s.RSSI     = [int]$matches[1] }
        elseif ($l -match '^\s+Receive rate \(Mbps\)\s+:\s+([\d.]+)')        { $s.RxRate   = [double]$matches[1] }
        elseif ($l -match '^\s+Transmit rate \(Mbps\)\s+:\s+([\d.]+)')       { $s.TxRate   = [double]$matches[1] }
    }
    [pscustomobject]$s
}

function Get-VisibleBSSIDs([string]$ssid) {
    $out = & netsh.exe wlan show networks mode=bssid 2>$null
    $inSsid = $false
    $bssid = $null; $signal = $null; $band = $null; $ch = $null
    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($line in $out) {
        if ($line -match '^SSID \d+\s+:\s+(.*?)\s*$') {
            if ($inSsid -and $bssid) { $rows.Add([pscustomobject]@{BSSID=$bssid;Signal=$signal;Band=$band;Channel=$ch}) }
            $inSsid = ($matches[1] -eq $ssid)
            $bssid = $null; $signal = $null; $band = $null; $ch = $null
            continue
        }
        if (-not $inSsid) { continue }
        if ($line -match '^\s+BSSID \d+\s+:\s+(.+?)\s*$') {
            if ($bssid) { $rows.Add([pscustomobject]@{BSSID=$bssid;Signal=$signal;Band=$band;Channel=$ch}) }
            $bssid = $matches[1]; $signal = $null; $band = $null; $ch = $null
        }
        elseif ($line -match '^\s+Signal\s+:\s+(\d+)%') { $signal = [int]$matches[1] }
        elseif ($line -match '^\s+Band\s+:\s+(.+?)\s*$') { $band = $matches[1] }
        elseif ($line -match '^\s+Channel\s+:\s+(\d+)') { $ch = [int]$matches[1] }
    }
    if ($inSsid -and $bssid) { $rows.Add([pscustomobject]@{BSSID=$bssid;Signal=$signal;Band=$band;Channel=$ch}) }
    $rows
}

# netsh signal % -> approximate RSSI dBm. Microsoft: 0%=-100, 100%=-50.
function ConvertTo-Rssi([int]$pct) { [int](-100 + ($pct / 2.0)) }

# ----- Pinger -------------------------------------------------------------
$script:Pinger = New-Object System.Net.NetworkInformation.Ping
function Test-Latency {
    try {
        $r = $script:Pinger.Send($script:Config.Target, 1500)
        if ($r.Status -eq [System.Net.NetworkInformation.IPStatus]::Success) { return [int]$r.RoundtripTime }
    } catch {}
    return $null
}

# ----- Ping history persistence --------------------------------------------
# Every sample is appended to a CSV so a restart restores the full chart
# window. The file is pruned to PingHistoryMinutes at startup and every minute.
$script:HistoryFile  = Join-Path $script:Config.LogDir 'ping-history.csv'
$script:HistoryStamp = 'yyyy-MM-ddTHH:mm:ss.fff'   # fixed-width, lexicographically sortable

function Save-PingSample([DateTime]$Time, $Ms) {
    $line = '{0},{1}' -f $Time.ToString($script:HistoryStamp), $(if ($null -eq $Ms) { '' } else { $Ms })
    try { Add-Content -Path $script:HistoryFile -Value $line -Encoding ASCII -ErrorAction Stop } catch {}
}

function Prune-PingHistoryFile {
    if (-not (Test-Path $script:HistoryFile)) { return }
    $cutoff = (Get-Date).AddMinutes(-$script:Config.PingHistoryMinutes).ToString($script:HistoryStamp)
    try {
        $keep = @(Get-Content $script:HistoryFile -ErrorAction Stop |
                  Where-Object { $_.Length -gt 23 -and $_.Substring(0, 23) -ge $cutoff })
        Set-Content -Path $script:HistoryFile -Value $keep -Encoding ASCII
    } catch {
        Write-Log "History prune error: $($_.Exception.Message)" 'ERR'
    }
}

function Restore-PingHistory {
    if (-not (Test-Path $script:HistoryFile)) { return }
    $cutoff = (Get-Date).AddMinutes(-$script:Config.PingHistoryMinutes)
    $restored = 0
    foreach ($line in @(Get-Content $script:HistoryFile -ErrorAction SilentlyContinue)) {
        $parts = $line.Split(',')
        if ($parts.Count -ne 2) { continue }
        $t = [DateTime]::MinValue
        if (-not [DateTime]::TryParseExact($parts[0], $script:HistoryStamp,
                [System.Globalization.CultureInfo]::InvariantCulture,
                [System.Globalization.DateTimeStyles]::None, [ref]$t)) { continue }
        if ($t -lt $cutoff) { continue }
        $ms = if ($parts[1] -eq '') { $null } else { [int]$parts[1] }
        $script:State.PingHistory.Enqueue([pscustomobject]@{ Time = $t; Ms = $ms })
        $restored++
    }
    while ($script:State.PingHistory.Count -gt $script:Config.PingHistoryMax) {
        $null = $script:State.PingHistory.Dequeue()
    }
    if ($restored) { Write-Log "Restored $restored ping samples from previous run" }
}

# ----- Icon (colored dot) -------------------------------------------------
function New-StatusIcon([string]$state) {
    $bmp = New-Object System.Drawing.Bitmap 16, 16
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $color = switch ($state) {
        'good'    { [System.Drawing.Color]::FromArgb(255, 40, 180, 80) }
        'warn'    { [System.Drawing.Color]::FromArgb(255, 220, 160, 30) }
        'bad'     { [System.Drawing.Color]::FromArgb(255, 210, 50, 50) }
        'roaming' { [System.Drawing.Color]::FromArgb(255, 80, 130, 230) }
        default   { [System.Drawing.Color]::FromArgb(255, 130, 130, 130) }
    }
    $brush = New-Object System.Drawing.SolidBrush $color
    $g.FillEllipse($brush, 1, 1, 14, 14)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 30, 30, 30)), 1
    $g.DrawEllipse($pen, 1, 1, 13, 13)
    $g.Dispose(); $brush.Dispose(); $pen.Dispose()
    $hicon = $bmp.GetHicon()
    $icon  = [System.Drawing.Icon]::FromHandle($hicon)
    $bmp.Dispose()
    [pscustomobject]@{ Icon = $icon; Handle = $hicon }
}

# Native: destroy old hicon to avoid leak
Add-Type -Namespace Native -Name User32 -MemberDefinition '
[DllImport("user32.dll", SetLastError=true)] public static extern bool DestroyIcon(IntPtr hIcon);
' -ErrorAction SilentlyContinue

# Give this process its own taskbar identity. Without it the status window is
# grouped under powershell.exe/pwsh.exe and the taskbar shows the PowerShell
# icon regardless of what Form.Icon is set to. Must run before any window or
# tray icon is created.
Add-Type -Namespace Native -Name Shell32 -MemberDefinition '
[DllImport("shell32.dll", SetLastError=true)]
public static extern int SetCurrentProcessExplicitAppUserModelID([MarshalAs(UnmanagedType.LPWStr)] string AppID);
' -ErrorAction SilentlyContinue
try { [Native.Shell32]::SetCurrentProcessExplicitAppUserModelID('NetworkMonitor.TrayApp') | Out-Null } catch {}

# App icon: Wi-Fi arcs on a dark rounded square. Used for the status window's
# title bar / taskbar / Alt-Tab; the tray keeps its colored status dot.
function New-AppIcon {
    $s = 64
    $bmp = New-Object System.Drawing.Bitmap $s, $s
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias

    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $r = 14
    $path.AddArc(0, 0, $r*2, $r*2, 180, 90)
    $path.AddArc($s-$r*2, 0, $r*2, $r*2, 270, 90)
    $path.AddArc($s-$r*2, $s-$r*2, $r*2, $r*2, 0, 90)
    $path.AddArc(0, $s-$r*2, $r*2, $r*2, 90, 90)
    $path.CloseFigure()
    $bg = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 24, 26, 32))
    $g.FillPath($bg, $path)

    $green = [System.Drawing.Color]::FromArgb(255, 60, 200, 100)
    $pen = New-Object System.Drawing.Pen $green, 5
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap   = [System.Drawing.Drawing2D.LineCap]::Round
    $cx = 32; $cy = 50
    foreach ($rad in 12, 21, 30) {
        $g.DrawArc($pen, $cx-$rad, $cy-$rad, $rad*2, $rad*2, 225, 90)
    }
    $dot = New-Object System.Drawing.SolidBrush $green
    $g.FillEllipse($dot, $cx-4, $cy-4, 8, 8)

    $g.Dispose(); $bg.Dispose(); $pen.Dispose(); $dot.Dispose(); $path.Dispose()
    $hicon = $bmp.GetHicon()   # handle kept alive for the life of the process
    $icon = [System.Drawing.Icon]::FromHandle($hicon)
    $bmp.Dispose()
    $icon
}
$script:AppIcon = New-AppIcon

# Double-buffered Panel — eliminates the flash on Invalidate().
if (-not ('NetMon.BufferedPanel' -as [type])) {
    # System.ComponentModel.Primitives is required under PowerShell 7 (pwsh) — without it
    # this fails to compile (CS0012), the type never exists, and the chart panel is silently
    # never created, leaving a blank area below the header. Harmless extra ref under PS 5.1.
    Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing, System.ComponentModel.Primitives -TypeDefinition @'
using System.Windows.Forms;
namespace NetMon {
    public class BufferedPanel : Panel {
        public BufferedPanel() {
            SetStyle(ControlStyles.OptimizedDoubleBuffer
                   | ControlStyles.AllPaintingInWmPaint
                   | ControlStyles.UserPaint, true);
            UpdateStyles();
        }
    }
}
'@ 2>$null
}

# Returns a double-buffered chart panel. Falls back to a plain Panel (double-buffered
# via reflection) if the compiled type is unavailable, so the chart is never lost.
function New-ChartPanel {
    if ('NetMon.BufferedPanel' -as [type]) { return New-Object NetMon.BufferedPanel }
    Write-Log 'BufferedPanel type unavailable; using reflection-double-buffered Panel' 'WARN'
    $p = New-Object System.Windows.Forms.Panel
    $prop = [System.Windows.Forms.Control].GetProperty('DoubleBuffered', [System.Reflection.BindingFlags]'Instance,NonPublic')
    $prop.SetValue($p, $true, $null)
    return $p
}

function Set-TrayIconState([string]$state) {
    if ($script:State.CurrentIconState -eq $state) { return }
    $i = New-StatusIcon $state
    $oldHandle = $script:State.LastIconHandle
    $script:NotifyIcon.Icon = $i.Icon
    $script:State.CurrentIconState = $state
    $script:State.LastIconHandle = $i.Handle
    if ($oldHandle -ne [IntPtr]::Zero) {
        try { [Native.User32]::DestroyIcon($oldHandle) | Out-Null } catch {}
    }
}

function Show-TrayBalloon([string]$Title, [string]$Text, [int]$Ms = 2500) {
    if (-not $script:NotifyIcon) { return }
    try { $script:NotifyIcon.ShowBalloonTip($Ms, $Title, $Text, [System.Windows.Forms.ToolTipIcon]::Info) } catch {}
}

# ----- Re-roam (async via hidden child process) ---------------------------
function Invoke-Reroam {
    param([string]$Reason = 'manual')
    $manual = ($Reason -eq 'manual')
    if ($script:State.Reroaming) {
        # A genuinely-running re-roam has a live child process. If there's no live
        # process the flag is stale (e.g. a crashed watcher) — let a manual request clear it.
        $stale = (-not $script:State.ReroamProc) -or $script:State.ReroamProc.HasExited
        if ($manual -and $stale) {
            Write-Log 'Re-roam: clearing stale in-progress state' 'WARN'
            if ($script:State.ReroamCheckTimer) { try { $script:State.ReroamCheckTimer.Stop(); $script:State.ReroamCheckTimer.Dispose() } catch {} ; $script:State.ReroamCheckTimer = $null }
            $script:State.Reroaming = $false; $script:State.ReroamProc = $null
        } else {
            Write-Log "Re-roam requested ($Reason) but already in progress" 'WARN'
            if ($manual) { Show-TrayBalloon 'Re-roam' 'A re-roam is already in progress.' }
            return
        }
    }
    $sinceLast = ((Get-Date) - $script:State.LastReroamAt).TotalSeconds
    if ($sinceLast -lt 30) {
        Write-Log "Re-roam suppressed (cooldown): $Reason" 'WARN'
        if ($manual) { Show-TrayBalloon 'Re-roam' ("Cooldown active — try again in {0}s." -f [int](30 - $sinceLast)) }
        return
    }
    $adapter = $script:State.AdapterName
    if (-not $adapter) {
        Write-Log "Re-roam aborted: no Wi-Fi adapter detected" 'ERR'
        if ($manual) { Show-TrayBalloon 'Re-roam' 'No Wi-Fi adapter detected.' }
        return
    }
    Write-Log "Re-roam start ($Reason) adapter=$adapter"
    if ($manual) { Show-TrayBalloon 'Re-roam' "Re-roaming $adapter…" }
    $script:State.Reroaming = $true
    $script:State.LastReroamAt = Get-Date
    Set-TrayIconState 'roaming'

    $logFile = (Get-LogFile)
    $child = @"
`$ErrorActionPreference = 'SilentlyContinue'
function L(`$m) { Add-Content -Path '$logFile' -Value ('{0} REROAM {1}' -f (Get-Date -Format 'HH:mm:ss.fff'), `$m) }
L 'disable adapter ($adapter)'
Disable-NetAdapter -Name '$adapter' -Confirm:`$false
Start-Sleep -Seconds 3
L 'enable adapter'
Enable-NetAdapter -Name '$adapter' -Confirm:`$false
for (`$i=0; `$i -lt 30; `$i++) {
    Start-Sleep -Seconds 1
    `$a = Get-NetAdapter -Name '$adapter'
    if (`$a.Status -eq 'Up') { Start-Sleep -Seconds 3; L 'adapter Up'; break }
}
L 'done'
"@
    $b64 = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($child))
    try {
        $p = Start-Process -FilePath powershell.exe `
             -ArgumentList '-NoProfile','-WindowStyle','Hidden','-ExecutionPolicy','Bypass','-EncodedCommand',$b64 `
             -WindowStyle Hidden -PassThru
        $script:State.ReroamProc = $p

        $check = New-Object System.Windows.Forms.Timer
        $check.Interval = 1000
        $check.Add_Tick({
            param($sender, $e)
            try {
                $proc = $script:State.ReroamProc
                if ($proc -and -not $proc.HasExited) { return }   # still running; keep waiting
                # Stop the timer that actually fired — never rely on a shared reference.
                $sender.Stop(); $sender.Dispose()
                if ($script:State.ReroamCheckTimer -eq $sender) { $script:State.ReroamCheckTimer = $null }
                $exit = if ($proc) { try { $proc.ExitCode } catch { 'n/a' } } else { 'n/a' }
                $script:State.Reroaming = $false
                $script:State.ReroamProc = $null
                if ($proc) { $script:State.Roams++ }   # only count a real adapter cycle
                Write-Log "Re-roam finished (exit=$exit)"
                Update-StatusForm
            } catch {
                Write-Log "Re-roam check error: $($_.Exception.Message)" 'ERR'
                try { $sender.Stop(); $sender.Dispose() } catch {}
                $script:State.Reroaming = $false
            }
        })
        $script:State.ReroamCheckTimer = $check
        $check.Start()
    } catch {
        Write-Log "Re-roam launch error: $($_.Exception.Message)" 'ERR'
        $script:State.Reroaming = $false
    }
}

# ----- Roam logic ---------------------------------------------------------
function Test-ShouldReroam {
    if ($script:State.Reroaming) { return $false }
    if (-not $script:State.SSID -or -not $script:State.BSSID) { return $false }
    if ($null -eq $script:State.RSSI) { return $false }
    if ($script:State.RSSI -ge $script:Config.MinRssiToConsider) { return $false }

    $ssid = if ($script:Config.SSIDOverride) { $script:Config.SSIDOverride } else { $script:State.SSID }
    $rows = Get-VisibleBSSIDs $ssid
    if (-not $rows -or $rows.Count -eq 0) { return $false }
    $best = $rows | Sort-Object Signal -Descending | Select-Object -First 1
    if (-not $best -or $best.BSSID -eq $script:State.BSSID) { return $false }
    $bestRssi = ConvertTo-Rssi $best.Signal
    $delta = $bestRssi - $script:State.RSSI
    if ($delta -ge $script:Config.RoamThresholdDb) {
        Write-Log ("Roam candidate: current={0} ({1} dBm) best={2} ({3}, ch {4}, {5}% ~{6} dBm) delta={7} dB" -f `
            $script:State.BSSID, $script:State.RSSI, $best.BSSID, $best.Band, $best.Channel, $best.Signal, $bestRssi, $delta)
        return $true
    }
    return $false
}

# ----- Netwtw event watcher ----------------------------------------------
function Check-NetwtwEvents {
    $since = $script:State.NetwtwLastSeen
    try {
        $evts = Get-WinEvent -FilterHashtable @{
            LogName='System'; StartTime=$since; ProviderName='Netwtw14','Netwtw16','Netwtw06','Netwtw08','Netwtw10','Netwtw12'; Level=1,2,3
        } -ErrorAction Stop
    } catch { $evts = $null }
    if ($evts) {
        foreach ($e in $evts) {
            $first = ($e.Message -split "`r?`n")[0]
            Write-Log ("NETWTW {0} Id={1} {2}" -f $e.LevelDisplayName, $e.Id, $first) 'WARN'
        }
        $script:State.NetwtwCount += $evts.Count
        try {
            $script:NotifyIcon.ShowBalloonTip(8000,
                "Wi-Fi driver fault",
                ("{0} new Netwtw event(s). Total since start: {1}" -f $evts.Count, $script:State.NetwtwCount),
                [System.Windows.Forms.ToolTipIcon]::Warning)
        } catch {}
    }
    $script:State.NetwtwLastSeen = Get-Date
}

# ----- Tooltip / icon update ---------------------------------------------
function Update-Tooltip {
    # NotifyIcon.Text is hard-capped at 63 chars by Shell_NotifyIcon. Keep it tight.
    $msStr = if ($null -eq $script:State.LastPingMs) { 'DROP' } else { ("{0}ms" -f $script:State.LastPingMs) }
    $bssidShort = if ($script:State.BSSID) { ($script:State.BSSID -split ':')[-3..-1] -join ':' } else { '-' }
    $sigStr = if ($null -ne $script:State.RSSI) { "{0}% {1}dBm" -f $script:State.SignalPct, $script:State.RSSI } else { 'no AP' }
    $rateStr = if ($script:State.RxRate) { "{0:N0}/{1:N0}M" -f $script:State.RxRate, $script:State.TxRate } else { '' }
    $flag = if ($script:State.Reroaming) { ' ROAM' } elseif ($script:State.NetwtwCount -gt 0) { " !$($script:State.NetwtwCount)" } else { '' }

    $tip = "$msStr  $sigStr$flag`n$bssidShort  ch$($script:State.Channel) $rateStr"
    if ($tip.Length -gt 63) { $tip = $tip.Substring(0,63) }
    $script:NotifyIcon.Text = $tip

    if ($script:State.Reroaming) { Set-TrayIconState 'roaming'; return }
    if ($null -eq $script:State.LastPingMs) { Set-TrayIconState 'bad'; return }
    if ($script:State.LastPingMs -gt 200 -or ($script:State.RSSI -and $script:State.RSSI -le -80)) { Set-TrayIconState 'warn'; return }
    Set-TrayIconState 'good'
}

# ----- UI: NotifyIcon + context menu --------------------------------------
$script:NotifyIcon = New-Object System.Windows.Forms.NotifyIcon
$script:NotifyIcon.Icon = (New-StatusIcon 'idle').Icon
$script:NotifyIcon.Text = 'NetMon initialising'
$script:NotifyIcon.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip
$miStatus  = $menu.Items.Add('Status: starting...')
$miStatus.Enabled = $false
$null      = $menu.Items.Add('-')
$miShow    = $menu.Items.Add('Show window')
$miReroam  = $menu.Items.Add('Force re-roam now')
$miOpenLog = $menu.Items.Add('Open log folder')
$miOpenCfg = $menu.Items.Add('Edit config.json')
$null      = $menu.Items.Add('-')
$miQuit    = $menu.Items.Add('Quit')
$script:NotifyIcon.ContextMenuStrip = $menu

# ----- Status window ------------------------------------------------------
$script:StatusForm = $null

function Show-StatusForm {
    if ($script:StatusForm -and -not $script:StatusForm.IsDisposed) {
        $script:StatusForm.WindowState = 'Normal'
        $script:StatusForm.Activate()
        return
    }
    $f = New-Object System.Windows.Forms.Form
    $f.Text = 'Network Monitor'
    $f.Icon = $script:AppIcon
    $f.Size = New-Object System.Drawing.Size(720, 480)
    $f.StartPosition = 'CenterScreen'
    $f.MinimumSize = New-Object System.Drawing.Size(480, 320)
    $f.BackColor = [System.Drawing.Color]::FromArgb(255, 24, 24, 28)
    $f.ForeColor = [System.Drawing.Color]::FromArgb(255, 220, 220, 220)

    $top = New-Object System.Windows.Forms.Label
    $top.Dock = 'Top'
    $top.Height = 140
    $top.Font = New-Object System.Drawing.Font('Consolas', 9)
    $top.Padding = New-Object System.Windows.Forms.Padding(10, 8, 10, 4)
    $top.TextAlign = 'TopLeft'
    $top.ForeColor = [System.Drawing.Color]::FromArgb(255, 220, 220, 220)

    $chart = New-ChartPanel
    $chart.Dock = 'Fill'
    $chart.BackColor = [System.Drawing.Color]::FromArgb(255, 18, 18, 22)
    $chart.Add_Paint({
        param($sender, $e)
        Invoke-PaintPingChart $sender $e.Graphics
    })
    $chart.Add_Resize({ $chart.Invalidate() })

    $f.Controls.Add($chart)   # add Fill-docked first
    $f.Controls.Add($top)
    $f.Tag = @{ Label = $top; Chart = $chart }
    $f.Add_FormClosed({ $script:StatusForm = $null })
    $script:StatusForm = $f
    $f.Show()
}

function Invoke-PaintPingChart {
    param($Panel, [System.Drawing.Graphics]$g)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
    $w = $Panel.ClientSize.Width
    $h = $Panel.ClientSize.Height

    $colBg     = [System.Drawing.Color]::FromArgb(255, 18, 18, 22)
    $colGrid   = [System.Drawing.Color]::FromArgb(70, 200, 200, 220)
    $colGridMaj= [System.Drawing.Color]::FromArgb(140, 200, 200, 220)
    $colAxis   = [System.Drawing.Color]::FromArgb(255, 180, 180, 200)
    $colText   = [System.Drawing.Color]::FromArgb(255, 200, 200, 215)
    $colLineG  = [System.Drawing.Color]::FromArgb(255,  90, 200, 120)
    $colLineY  = [System.Drawing.Color]::FromArgb(255, 220, 180,  50)
    $colDrop   = [System.Drawing.Color]::FromArgb(170, 230,  70,  70)
    $colArea   = [System.Drawing.Color]::FromArgb( 40,  90, 200, 120)

    $samples = $null
    try { $samples = $script:State.PingHistory.ToArray() } catch { $samples = @() }

    $font   = New-Object System.Drawing.Font 'Consolas', 8
    $fontHud= New-Object System.Drawing.Font 'Consolas', 9, ([System.Drawing.FontStyle]::Bold)
    $tbrush = New-Object System.Drawing.SolidBrush $colText

    if (-not $samples -or $samples.Count -lt 2) {
        $msg = 'Collecting ping samples...'
        $sz = $g.MeasureString($msg, $fontHud)
        $g.DrawString($msg, $fontHud, $tbrush, ($w - $sz.Width)/2, ($h - $sz.Height)/2)
        $font.Dispose(); $fontHud.Dispose(); $tbrush.Dispose()
        return
    }

    # stats
    $vals = @($samples | Where-Object { $null -ne $_.Ms } | ForEach-Object { [int]$_.Ms })
    $dropCount = ($samples | Where-Object { $null -eq $_.Ms }).Count
    $minMs = if ($vals.Count) { ($vals | Measure-Object -Minimum).Minimum } else { 0 }
    $maxMs = if ($vals.Count) { ($vals | Measure-Object -Maximum).Maximum } else { 0 }
    $avgMs = if ($vals.Count) { [int](($vals | Measure-Object -Average).Average) } else { 0 }

    # y-axis scale: round up to a "nice" value
    $niceSteps = 50, 100, 200, 300, 500, 750, 1000, 1500, 2000, 3000, 5000
    $yMax = ($niceSteps | Where-Object { $_ -ge ([Math]::Max(50, $maxMs)) } | Select-Object -First 1)
    if (-not $yMax) { $yMax = [int]([Math]::Ceiling($maxMs / 1000.0) * 1000) }

    $leftPad = 44; $rightPad = 12; $topPad = 22; $bottomPad = 24
    $plotW = $w - $leftPad - $rightPad
    $plotH = $h - $topPad - $bottomPad
    if ($plotW -le 10 -or $plotH -le 10) { $font.Dispose(); $fontHud.Dispose(); $tbrush.Dispose(); return }

    # background of plot area
    $plotBgBrush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 14, 14, 18))
    $g.FillRectangle($plotBgBrush, $leftPad, $topPad, $plotW, $plotH)
    $plotBgBrush.Dispose()

    # gridlines + y labels
    $gridPen = New-Object System.Drawing.Pen $colGrid
    $axisPen = New-Object System.Drawing.Pen $colAxis
    $steps = 5
    for ($i = 0; $i -le $steps; $i++) {
        $y = $topPad + ($plotH * (1 - $i / $steps))
        $g.DrawLine($gridPen, $leftPad, $y, $leftPad + $plotW, $y)
        $ms = [int]($yMax * $i / $steps)
        $lbl = "$ms"
        $sz = $g.MeasureString($lbl, $font)
        $g.DrawString($lbl, $font, $tbrush, $leftPad - 4 - $sz.Width, $y - $sz.Height/2)
    }
    # left axis
    $g.DrawLine($axisPen, $leftPad, $topPad, $leftPad, $topPad + $plotH)
    $g.DrawLine($axisPen, $leftPad, $topPad + $plotH, $leftPad + $plotW, $topPad + $plotH)

    # plot points
    $n = $samples.Count
    $dropPen = New-Object System.Drawing.Pen $colDrop
    $points = New-Object System.Collections.Generic.List[System.Drawing.PointF]
    for ($i = 0; $i -lt $n; $i++) {
        $x = $leftPad + ($plotW * $i / [Math]::Max(1, $n - 1))
        if ($null -eq $samples[$i].Ms) {
            $g.DrawLine($dropPen, $x, $topPad, $x, $topPad + $plotH)
        } else {
            $y = $topPad + $plotH * (1 - ([double]$samples[$i].Ms / $yMax))
            if ($y -lt $topPad) { $y = $topPad }
            $points.Add((New-Object System.Drawing.PointF $x, $y))
        }
    }

    # filled area under line
    if ($points.Count -gt 1) {
        $polyPoints = New-Object System.Collections.Generic.List[System.Drawing.PointF]
        $polyPoints.AddRange($points)
        $polyPoints.Add((New-Object System.Drawing.PointF $points[$points.Count-1].X, ($topPad + $plotH)))
        $polyPoints.Add((New-Object System.Drawing.PointF $points[0].X, ($topPad + $plotH)))
        $areaBrush = New-Object System.Drawing.SolidBrush $colArea
        $g.FillPolygon($areaBrush, [System.Drawing.PointF[]]$polyPoints.ToArray())
        $areaBrush.Dispose()
    }

    # line (color shifts amber if avg high)
    $lineColor = if ($avgMs -gt 150) { $colLineY } else { $colLineG }
    $linePen = New-Object System.Drawing.Pen $lineColor, 1.6
    if ($points.Count -gt 1) {
        $g.DrawLines($linePen, [System.Drawing.PointF[]]$points.ToArray())
    }

    # current value dot
    if ($points.Count -gt 0) {
        $last = $points[$points.Count - 1]
        $dotBrush = New-Object System.Drawing.SolidBrush $lineColor
        $g.FillEllipse($dotBrush, $last.X - 3, $last.Y - 3, 6, 6)
        $dotBrush.Dispose()
    }

    # x-axis labels
    if ($samples.Count -gt 0) {
        $firstT = $samples[0].Time.ToString('HH:mm:ss')
        $lastT  = $samples[$samples.Count - 1].Time.ToString('HH:mm:ss')
        $sz1 = $g.MeasureString($firstT, $font)
        $sz2 = $g.MeasureString($lastT, $font)
        $g.DrawString($firstT, $font, $tbrush, $leftPad, $topPad + $plotH + 4)
        $g.DrawString($lastT,  $font, $tbrush, $leftPad + $plotW - $sz2.Width, $topPad + $plotH + 4)
        if ($plotW -gt 240) {
            $midT = $samples[[int]($samples.Count / 2)].Time.ToString('HH:mm:ss')
            $szm = $g.MeasureString($midT, $font)
            $g.DrawString($midT, $font, $tbrush, $leftPad + $plotW/2 - $szm.Width/2, $topPad + $plotH + 4)
        }
    }

    # HUD: min / avg / max / drops in top-right
    $hud = "min {0}  avg {1}  max {2}  drops {3}/{4}" -f $minMs, $avgMs, $maxMs, $dropCount, $samples.Count
    $hudSz = $g.MeasureString($hud, $fontHud)
    $g.DrawString($hud, $fontHud, $tbrush, $leftPad + $plotW - $hudSz.Width, 4)

    $gridPen.Dispose(); $axisPen.Dispose(); $dropPen.Dispose(); $linePen.Dispose()
    $font.Dispose(); $fontHud.Dispose(); $tbrush.Dispose()
}

function Update-StatusForm {
    if (-not $script:StatusForm -or $script:StatusForm.IsDisposed) { return }
    $tag = $script:StatusForm.Tag
    $msStr = if ($null -eq $script:State.LastPingMs) { 'DROP' } else { "{0}ms" -f $script:State.LastPingMs }
    $reroamStr = if ($script:State.Reroaming) { '  [RE-ROAM IN PROGRESS]' } else { '' }
    $hdr = @"
Time     : {0}
SSID     : {1}
BSSID    : {2}
Band/Ch  : {3} / {4}
Signal   : {5}%  ({6} dBm)
Rate     : {7}/{8} Mbps
Ping     : {9}  (drops {10} / total {11})
Netwtw   : {12}    Roams: {13}{14}
"@ -f (Get-Date -Format 'HH:mm:ss'), $script:State.SSID, $script:State.BSSID, $script:State.Band, $script:State.Channel,
         $script:State.SignalPct, $script:State.RSSI, $script:State.RxRate, $script:State.TxRate,
         $msStr, $script:State.Drops, $script:State.Total, $script:State.NetwtwCount, $script:State.Roams, $reroamStr
    $tag.Label.Text = $hdr
    $tag.Chart.Invalidate()
}

# ----- Preview popup (left-click on tray icon) -----------------------------
$script:PreviewForm = $null
$script:PreviewHiddenAt = [DateTime]::MinValue

function New-PreviewForm {
    $f = New-Object System.Windows.Forms.Form
    $f.FormBorderStyle = 'None'
    $f.ShowInTaskbar = $false
    $f.TopMost = $true
    $f.StartPosition = 'Manual'
    $f.Size = New-Object System.Drawing.Size(380, 210)
    # form background doubles as a 1px border around the docked controls
    $f.BackColor = [System.Drawing.Color]::FromArgb(255, 70, 70, 80)
    $f.Padding = New-Object System.Windows.Forms.Padding(1)
    $f.KeyPreview = $true

    $hdr = New-Object System.Windows.Forms.Label
    $hdr.Dock = 'Top'
    $hdr.Height = 22
    $hdr.Font = New-Object System.Drawing.Font('Consolas', 9)
    $hdr.BackColor = [System.Drawing.Color]::FromArgb(255, 24, 24, 28)
    $hdr.ForeColor = [System.Drawing.Color]::FromArgb(255, 220, 220, 220)
    $hdr.Padding = New-Object System.Windows.Forms.Padding(6, 0, 6, 0)
    $hdr.TextAlign = 'MiddleLeft'

    $chart = New-ChartPanel
    $chart.Dock = 'Fill'
    $chart.BackColor = [System.Drawing.Color]::FromArgb(255, 18, 18, 22)
    $chart.Add_Paint({
        param($sender, $e)
        Invoke-PaintPingChart $sender $e.Graphics
    })
    $chart.Add_Resize({ param($sender, $e) $sender.Invalidate() })

    $f.Controls.Add($chart)   # add Fill-docked first
    $f.Controls.Add($hdr)
    $f.Tag = @{ Label = $hdr; Chart = $chart }

    $f.Add_Deactivate({ Hide-PreviewForm })
    $f.Add_KeyDown({ param($sender, $e) if ($e.KeyCode -eq [System.Windows.Forms.Keys]::Escape) { Hide-PreviewForm } })
    $f.Add_FormClosed({ $script:PreviewForm = $null })
    return $f
}

function Hide-PreviewForm {
    if ($script:PreviewForm -and -not $script:PreviewForm.IsDisposed -and $script:PreviewForm.Visible) {
        $script:PreviewForm.Hide()
        $script:PreviewHiddenAt = Get-Date
    }
}

function Show-PreviewForm {
    # Clicking the tray icon while the popup is open deactivates (hides) it just
    # before this handler runs; the grace window stops the same click from
    # instantly reopening it, so a second click acts as a toggle.
    if (((Get-Date) - $script:PreviewHiddenAt).TotalMilliseconds -lt 300) { return }
    if ($script:PreviewForm -and -not $script:PreviewForm.IsDisposed -and $script:PreviewForm.Visible) {
        Hide-PreviewForm
        return
    }
    if (-not $script:PreviewForm -or $script:PreviewForm.IsDisposed) { $script:PreviewForm = New-PreviewForm }
    $f = $script:PreviewForm
    # position near the cursor, clamped to the working area (sits above a bottom taskbar)
    $pt = [System.Windows.Forms.Cursor]::Position
    $wa = ([System.Windows.Forms.Screen]::FromPoint($pt)).WorkingArea
    $x = [Math]::Min([Math]::Max($pt.X - [int]($f.Width / 2), $wa.Left + 8), $wa.Right - $f.Width - 8)
    $y = if ($pt.Y -gt ($wa.Top + $wa.Height / 2)) { $wa.Bottom - $f.Height - 8 } else { $wa.Top + 8 }
    $f.Location = New-Object System.Drawing.Point($x, $y)
    $f.Show()
    $f.Activate()
    Update-PreviewForm
}

function Update-PreviewForm {
    $f = $script:PreviewForm
    if (-not $f -or $f.IsDisposed -or -not $f.Visible) { return }
    $msStr = if ($null -eq $script:State.LastPingMs) { 'DROP' } else { "{0}ms" -f $script:State.LastPingMs }
    $sigStr = if ($null -ne $script:State.RSSI) { "{0}% {1}dBm" -f $script:State.SignalPct, $script:State.RSSI } else { 'no AP' }
    $f.Tag.Label.Text = "{0}  {1}  {2}  ch{3}" -f $msStr, $script:State.SSID, $sigStr, $script:State.Channel
    $f.Tag.Chart.Invalidate()
}

# ----- Menu handlers ------------------------------------------------------
$miShow.Add_Click({ Show-StatusForm })
$miReroam.Add_Click({ Invoke-Reroam -Reason 'manual' })
$miOpenLog.Add_Click({ Start-Process explorer.exe $script:Config.LogDir })
$miOpenCfg.Add_Click({
    if (-not (Test-Path $cfgFile)) { ($script:Config | ConvertTo-Json) | Set-Content $cfgFile -Encoding UTF8 }
    Start-Process notepad.exe $cfgFile
})
$miQuit.Add_Click({
    Write-Log 'Quit requested'
    $script:NotifyIcon.Visible = $false
    $script:NotifyIcon.Dispose()
    if ($script:SingleInstanceMutex) {
        try { $script:SingleInstanceMutex.ReleaseMutex() } catch {}
        $script:SingleInstanceMutex.Dispose()
    }
    [System.Windows.Forms.Application]::Exit()
})
$script:NotifyIcon.Add_MouseClick({
    param($sender, $e)
    if ($e.Button -eq [System.Windows.Forms.MouseButtons]::Left) { Show-PreviewForm }
})
$script:NotifyIcon.Add_DoubleClick({ Hide-PreviewForm; Show-StatusForm })

# ----- Timers -------------------------------------------------------------
# Ping (fast)
$pingTimer = New-Object System.Windows.Forms.Timer
$pingTimer.Interval = $script:Config.PingIntervalMs
$pingTimer.Add_Tick({
    $ms = Test-Latency
    $script:State.LastPingMs = $ms
    $script:State.Total++
    if ($null -eq $ms) {
        $script:State.Drops++
        Write-Log "Ping DROP -> $($script:Config.Target)" 'WARN'
    } elseif ($ms -gt 200) {
        $script:State.HighLatency++
    }
    $now = Get-Date
    $script:State.PingHistory.Enqueue([pscustomobject]@{ Time = $now; Ms = $ms })
    Save-PingSample $now $ms
    # drop samples older than the configured window
    $cutoff = $now.AddMinutes(-$script:Config.PingHistoryMinutes)
    while ($script:State.PingHistory.Count -gt 0 -and $script:State.PingHistory.Peek().Time -lt $cutoff) {
        $null = $script:State.PingHistory.Dequeue()
    }
    # hard cap as a safety net
    while ($script:State.PingHistory.Count -gt $script:Config.PingHistoryMax) {
        $null = $script:State.PingHistory.Dequeue()
    }
    $miStatus.Text = "$(if ($null -eq $ms){'DROP'}else{"$($ms)ms"})  $($script:State.BSSID)  $($script:State.SignalPct)%"
    Update-Tooltip
    Update-StatusForm
    Update-PreviewForm
})

# Wi-Fi state refresh
$wifiTimer = New-Object System.Windows.Forms.Timer
$wifiTimer.Interval = $script:Config.WifiStateIntervalMs
$wifiTimer.Add_Tick({
    $st = Get-WifiStatus
    if ($st.State -ne 'connected') {
        if ($script:State.BSSID) { Write-Log "Wi-Fi link lost (was $($script:State.BSSID))" 'WARN'; $script:State.LinkLossCount++ }
        $script:State.SSID = $null; $script:State.BSSID = $null; $script:State.Band = $null
        $script:State.Channel = $null; $script:State.SignalPct = $null; $script:State.RSSI = $null
        $script:State.RxRate = $null; $script:State.TxRate = $null
        return
    }
    if ($script:State.BSSID -and $st.BSSID -ne $script:State.BSSID) {
        Write-Log "BSSID changed: $($script:State.BSSID) -> $($st.BSSID) ($($st.Band) ch $($st.Channel) $($st.Signal)% $($st.RSSI) dBm)"
    }
    $script:State.SSID = $st.SSID; $script:State.BSSID = $st.BSSID
    $script:State.Band = $st.Band; $script:State.Channel = $st.Channel
    $script:State.SignalPct = $st.Signal; $script:State.RSSI = $st.RSSI
    $script:State.RxRate = $st.RxRate; $script:State.TxRate = $st.TxRate
})

# Roam scan + decide
$scanTimer = New-Object System.Windows.Forms.Timer
$scanTimer.Interval = $script:Config.ScanIntervalMs
$scanTimer.Add_Tick({
    try {
        if ($script:Config.AutoRoam -and (Test-ShouldReroam)) { Invoke-Reroam -Reason 'auto-stronger-AP' }
    } catch {
        Write-Log "Scan error: $($_.Exception.Message)" 'ERR'
    }
})

# Netwtw watcher
$eventTimer = New-Object System.Windows.Forms.Timer
$eventTimer.Interval = $script:Config.EventIntervalMs
$eventTimer.Add_Tick({
    try { Check-NetwtwEvents } catch { Write-Log "Event check error: $($_.Exception.Message)" 'ERR' }
})

# History file prune (keeps ping-history.csv at the chart window size)
$pruneTimer = New-Object System.Windows.Forms.Timer
$pruneTimer.Interval = 60000
$pruneTimer.Add_Tick({ Prune-PingHistoryFile })

# ----- Bootstrap ----------------------------------------------------------
$adapter = Get-WifiAdapter
if ($adapter) {
    $script:State.AdapterName = $adapter.Name
    Write-Log "Started. adapter=$($adapter.Name) driver=$($adapter.DriverVersion) target=$($script:Config.Target)"
} else {
    Write-Log "Started. NO Wi-Fi adapter found" 'WARN'
}

# Prime Wi-Fi state immediately
& { $st = Get-WifiStatus
    if ($st.State -eq 'connected') {
        $script:State.SSID = $st.SSID; $script:State.BSSID = $st.BSSID; $script:State.Band = $st.Band
        $script:State.Channel = $st.Channel; $script:State.SignalPct = $st.Signal; $script:State.RSSI = $st.RSSI
        $script:State.RxRate = $st.RxRate; $script:State.TxRate = $st.TxRate
        Write-Log "Initial link: $($st.BSSID) $($st.Band) ch $($st.Channel) $($st.Signal)% $($st.RSSI) dBm"
    }
}

# Restore the chart window from the previous run, then trim the file
Restore-PingHistory
Prune-PingHistoryFile

$pingTimer.Start(); $wifiTimer.Start(); $scanTimer.Start(); $eventTimer.Start(); $pruneTimer.Start()

$script:NotifyIcon.ShowBalloonTip(2000, 'Network Monitor', 'Running.', [System.Windows.Forms.ToolTipIcon]::Info)
[System.Windows.Forms.Application]::Run()
