# Network Monitor

A small tray app (PowerShell + WinForms) that:

- Continuously pings `8.8.8.8` (configurable) at 1 Hz and tracks drops + latency.
- Watches the System event log for **Intel `Netwtw14`** (and siblings 06/08/10/12/16) driver faults — pops a balloon tip on each new event and counts them in the tray tooltip.
- Periodically scans Wi-Fi BSSIDs for the SSID you're connected to. If a clearly stronger BSSID is available (default: current RSSI ≤ -72 dBm **and** a candidate is ≥ 12 dB stronger), it forces a re-roam by bouncing the adapter — same pattern that worked manually.
- Logs everything to `logs\netmon-YYYYMMDD.log`.

Tray icon colour:
- green = healthy
- amber = high latency or weak signal
- red = ping drop
- blue = re-roam in progress
- grey = starting / idle

## Files

| File | Purpose |
|---|---|
| `NetworkMonitor.ps1` | The app. |
| `Start-NetworkMonitor.vbs` | Silent launcher (no console window). |
| `Start-NetworkMonitor.cmd` | Foreground launcher (keeps a console — for debugging). |
| `Install-Startup.ps1` | Registers a Scheduled Task that runs the app at logon with highest privileges. |
| `Uninstall-Startup.ps1` | Removes that task. |
| `config.json` | Tunables. Edited live; re-read on next start. |
| `logs/` | Created at runtime. |
| `logs/ping-history.csv` | Rolling ping samples (last `PingHistoryMinutes`). Restored into the chart on restart; pruned every minute. |

## First run

The app needs **admin** to call `Disable-NetAdapter` / `Enable-NetAdapter` for re-roams. If launched non-elevated it will still ping and watch events, but the re-roam button (and the auto-roam logic) will fail and log an error.

Quick test (foreground, see output):

```powershell
# from an elevated PowerShell
cd C:\devel\code\network-monitor
.\Start-NetworkMonitor.cmd
```

A balloon tip says "Running." and a coloured dot appears in the tray.

- Right-click → **Show window** for live state + recent ping list.
- Right-click → **Force re-roam now** to bounce the adapter.
- Right-click → **Open log folder**.
- Right-click → **Edit config.json** (changes take effect on next start).
- Right-click → **Quit**.

## Auto-start at logon

```powershell
# elevated
cd C:\devel\code\network-monitor
.\Install-Startup.ps1
Start-ScheduledTask -TaskName NetworkMonitor   # start it now
```

The task runs as the current user with highest privileges, so it has the rights it needs without prompting UAC at every logon.

To remove:

```powershell
.\Uninstall-Startup.ps1
```

## Tuning `config.json`

| Field | Default | Notes |
|---|---|---|
| `Target` | `8.8.8.8` | Ping destination. |
| `PingIntervalMs` | `1000` | Ping cadence. |
| `WifiStateIntervalMs` | `5000` | How often to read `netsh wlan show interfaces`. |
| `ScanIntervalMs` | `30000` | How often to scan available BSSIDs and consider a roam. |
| `EventIntervalMs` | `60000` | How often to poll for new `Netwtw*` events. |
| `RoamThresholdDb` | `12` | Auto-roam only if best candidate is at least this many dB stronger than current. |
| `MinRssiToConsider` | `-72` | Only consider auto-roaming if current RSSI is at or below this. Keeps the app from disrupting a healthy link. |
| `SSIDOverride` | `""` | Empty = follow whatever SSID is connected. Set to a specific SSID name to pin. |
| `PingHistoryMax` | `3600` | Hard cap on samples kept in memory. |
| `PingHistoryMinutes` | `10` | Chart window. Samples older than this are dropped from memory and pruned from `ping-history.csv`. Persisted samples survive a restart. |

## Logs

`logs\netmon-YYYYMMDD.log` — one file per day, plaintext, includes:
- ping drops
- Netwtw events (level / id / first line of message)
- BSSID changes
- roam decisions and re-roam start/finish

## Caveats

- The auto-roam loop is conservative on purpose (cooldown + RSSI gate + dB delta gate) so it won't thrash. It will not roam if the current link is already healthy.
- A re-roam briefly drops the link (~8-12 s).
- Counts of "Netwtw events" start from the moment the app launches — historical events before launch are not counted.
- If you change `config.json`, restart the app to pick up the changes.
