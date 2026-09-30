# Network Monitor (Rust)

A small Windows tray app that pings a host once a second, tracks Wi-Fi link quality, watches for Wi-Fi driver faults, and (optionally) roams to a better access point. It replaces the PowerShell script in the repo root.

- **Tray icon** coloured by health: green good, amber slow or weak signal, red dropped ping, blue roaming, grey starting. Left-click for a latency popup, double-click for the full window.
- **Status window** with live Wi-Fi details and a latency chart.
- **Settings tab** containing every tunable. Nothing is configured by editing files.
- **Self-updating** through [Velopack](https://velopack.io) from GitHub Releases.

## Footprint

Two processes, so the part that is always running stays tiny:

| Process | When | Memory (private) |
|---|---|---|
| Tray daemon (`network-monitor.exe`) | always | ~3 MB, <1 ms CPU/s |
| Window (`network-monitor.exe --ui ...`) | only while a window is open | ~60 MB (OpenGL + fonts) |

The daemon does all monitoring and never loads the GPU stack. Windows are short-lived child processes that talk to it over a per-user named pipe (JSON lines) and exit when closed.

## What changed from the PowerShell version

| Was | Now |
|---|---|
| Parsed `netsh` text (English-only) | Native Wifi API (`wlanapi`): structs, any language, real dBm and channel |
| Roamed by disabling/enabling the adapter (admin, 15-20 s outage) | `WlanConnect` naming the target BSSID: no outage, **no admin** |
| Ran elevated via a scheduled task | Runs as the normal user (`asInvoker`) |
| Logs, history and config next to the script | `%LOCALAPPDATA%\NetworkMonitor\Data` |
| Intel-only driver watcher, hardcoded | Configurable provider list, can be switched off |
| `ping` via .NET | `IcmpSendEcho` (no admin, no child process) |
| Auto-roam on by default, could flap | Off by default, with guard-rails (below) |
| No log cleanup | Log retention setting |

### Roaming guard-rails

The old auto-roam once chased a distant, weak 5 GHz satellite as the "stronger" AP and flapped between it and the main router. Auto-roam now requires all of:

1. Current signal below `Only when current signal is below` (default -72 dBm).
2. A target on the same network at least `Target must be stronger by` (12 dB) better.
3. That target itself at least `-67 dBm`: it will not roam to a barely-usable AP.
4. The same target winning 2 scans in a row.
5. The cooldown (120 s) and hourly cap (4) have not been hit.

Manual "Force re-roam now" bypasses 1-4 but still respects a short cooldown.

## Build and test

Needs the MSVC Rust toolchain and the Windows SDK (`rc.exe`, to embed the icon and manifest).

```
cargo build --release
cargo test                          # 44 tests; include live ICMP and Event Log calls
cargo test -- --ignored             # also re-associates to your current AP (safe no-op roam)
cargo run --example gen_assets      # regenerate assets/app.ico and assets/store logos
```

## Release

```
dotnet tool install -g vpk
.\scripts\release.ps1               # tests, build, vpk pack -> app\Releases
```

CI does this on a `v*` tag (`.github/workflows/release.yml`): it checks the tag matches `Cargo.toml`, tests, builds, packs with deltas, signs if Azure Trusted Signing variables are configured on the `release` environment, and publishes to GitHub Releases.

**Updates need a public repo** (or another reachable feed): the updater downloads release assets anonymously. Set the feed in *Settings > Updates > Update source*; it accepts a GitHub URL, any HTTP folder of Velopack files, or a local folder.

## Layout

| File | |
|---|---|
| `main.rs` | Velopack hooks, argument dispatch, daemon start-up, single-instance |
| `engine.rs` | ping loop, Wi-Fi/scan/event scheduler, roam execution, shared state |
| `wifi.rs` | Native Wifi API wrapper |
| `roam.rs` | roam decision and governor (pure, tested) |
| `ping.rs` | ICMP echo |
| `events.rs` | Event Log query for driver faults |
| `tray.rs` | tray icon, menu, Win32 message loop, launches windows |
| `ipc.rs`, `model.rs` | pipe transport and the messages that cross it |
| `ui/` | egui window: `mod.rs` (tabs, popup), `chart.rs`, `settings_panel.rs` |
| `update.rs` | Velopack update check/download/apply |
| `platform.rs` | toasts, start-with-Windows, package detection |
| `settings.rs` | every setting, with validation and defaults |
| `icon.rs` | procedural tray and app icons |

## Data

`%LOCALAPPDATA%\NetworkMonitor\Data`: `settings.json`, `ping-history.csv` (rolling window), `logs\netmon-YYYYMMDD.log`.
