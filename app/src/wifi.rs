//! Wi-Fi access through the Windows Native Wifi API (`wlanapi.dll`).
//!
//! This replaces the old `netsh` text scraping (English-only) and the adapter
//! disable/enable "bounce" (needed administrator rights). Everything here works
//! from an ordinary user token, in any display language:
//!   * state and BSS list come back as structs, not text,
//!   * a roam is a `WlanConnect` that names the preferred BSSID.

use crate::model::WifiInfo;
use std::ffi::c_void;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::NetworkManagement::Ndis::NDIS_OBJECT_HEADER;
use windows::Win32::NetworkManagement::WiFi::*;

const ERROR_SUCCESS: u32 = 0;

#[derive(Clone, Debug)]
pub struct Interface {
    pub guid: GUID,
    pub description: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BssEntry {
    pub bssid: [u8; 6],
    pub ssid: String,
    pub rssi: i32,
    pub link_quality: u32,
    pub freq_khz: u32,
}

/// The current association plus what is needed to re-issue a connect to it.
#[derive(Clone, Debug)]
pub struct Link {
    pub info: WifiInfo,
    pub profile: String,
    pub ssid_raw: Vec<u8>,
    pub bssid: [u8; 6],
}

pub struct WifiClient {
    handle: HANDLE,
}

// The WLAN client handle is documented as usable from multiple threads.
unsafe impl Send for WifiClient {}
unsafe impl Sync for WifiClient {}

impl Drop for WifiClient {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.handle, None);
        }
    }
}

impl WifiClient {
    /// Fails when the WLAN AutoConfig service is not running (e.g. a desktop with
    /// no Wi-Fi hardware). Callers treat that as "no Wi-Fi" and retry later.
    pub fn open() -> Result<Self, u32> {
        let mut negotiated = 0u32;
        let mut handle = HANDLE::default();
        let rc = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut handle) };
        if rc != ERROR_SUCCESS {
            return Err(rc);
        }
        Ok(Self { handle })
    }

    pub fn interfaces(&self) -> Vec<Interface> {
        let mut out = Vec::new();
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.handle, None, &mut list) != ERROR_SUCCESS || list.is_null() {
                return out;
            }
            let n = (*list).dwNumberOfItems as usize;
            let first = (*list).InterfaceInfo.as_ptr();
            for i in 0..n {
                let info = &*first.add(i);
                out.push(Interface {
                    guid: info.InterfaceGuid,
                    description: wide_to_string(&info.strInterfaceDescription),
                    connected: info.isState == wlan_interface_state_connected,
                });
            }
            WlanFreeMemory(list as *const c_void);
        }
        out
    }

    /// Prefer an interface that is connected, otherwise the first one.
    pub fn primary_interface(&self) -> Option<Interface> {
        let all = self.interfaces();
        all.iter().find(|i| i.connected).cloned().or_else(|| all.into_iter().next())
    }

    /// Trigger an asynchronous scan; results show up in `bss_list` shortly after.
    pub fn scan(&self, guid: &GUID) {
        unsafe {
            WlanScan(self.handle, guid, None, None, None);
        }
    }

    pub fn bss_list(&self, guid: &GUID) -> Vec<BssEntry> {
        let mut out = Vec::new();
        unsafe {
            let mut list: *mut WLAN_BSS_LIST = std::ptr::null_mut();
            let rc = WlanGetNetworkBssList(
                self.handle,
                guid,
                None,
                dot11_BSS_type_any,
                false,
                None,
                &mut list,
            );
            if rc != ERROR_SUCCESS || list.is_null() {
                return out;
            }
            let n = (*list).dwNumberOfItems as usize;
            let first = (*list).wlanBssEntries.as_ptr();
            for i in 0..n {
                let e = &*first.add(i);
                let len = (e.dot11Ssid.uSSIDLength as usize).min(32);
                out.push(BssEntry {
                    bssid: e.dot11Bssid,
                    ssid: String::from_utf8_lossy(&e.dot11Ssid.ucSSID[..len]).into_owned(),
                    rssi: e.lRssi,
                    link_quality: e.uLinkQuality,
                    freq_khz: e.ulChCenterFrequency,
                });
            }
            WlanFreeMemory(list as *const c_void);
        }
        out
    }

    /// Current association, or `None` when not connected. `bss` is the latest BSS
    /// list so the real dBm value and channel can be taken from the matching entry;
    /// without a match the percentage is converted instead.
    pub fn link(&self, guid: &GUID, bss: &[BssEntry]) -> Option<Link> {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut c_void = std::ptr::null_mut();
            let rc = WlanQueryInterface(
                self.handle,
                guid,
                wlan_intf_opcode_current_connection,
                None,
                &mut size,
                &mut data,
                None,
            );
            if rc != ERROR_SUCCESS || data.is_null() {
                return None;
            }
            let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let assoc = &attrs.wlanAssociationAttributes;
            let len = (assoc.dot11Ssid.uSSIDLength as usize).min(32);
            let ssid_raw = assoc.dot11Ssid.ucSSID[..len].to_vec();
            let bssid = assoc.dot11Bssid;
            let pct = assoc.wlanSignalQuality;
            let (rx, tx) = (assoc.ulRxRate, assoc.ulTxRate);
            let profile = wide_to_string(&attrs.strProfileName);
            let connected = attrs.isState == wlan_interface_state_connected;
            WlanFreeMemory(data);
            if !connected {
                return None;
            }

            let entry = bss.iter().find(|b| b.bssid == bssid);
            let (band, channel) = entry.map(|e| band_and_channel(e.freq_khz)).unwrap_or(("", 0));
            let rssi = entry.map(|e| e.rssi).unwrap_or_else(|| pct_to_rssi(pct));
            Some(Link {
                info: WifiInfo {
                    ssid: String::from_utf8_lossy(&ssid_raw).into_owned(),
                    bssid: format_bssid(&bssid),
                    band: band.to_string(),
                    channel,
                    signal_pct: pct,
                    rssi,
                    rx_mbps: rx / 1000,
                    tx_mbps: tx / 1000,
                },
                profile,
                ssid_raw,
                bssid,
            })
        }
    }

    /// Ask Windows to (re)connect to `profile`, preferring `bssid`. The link is
    /// not torn down first, so when the target is reachable this is a normal
    /// 802.11 re-association rather than an outage.
    pub fn connect_bssid(&self, guid: &GUID, profile: &str, ssid_raw: &[u8], bssid: [u8; 6]) -> Result<(), u32> {
        let wide: Vec<u16> = profile.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ssid = DOT11_SSID { uSSIDLength: ssid_raw.len().min(32) as u32, ucSSID: [0; 32] };
        ssid.ucSSID[..ssid.uSSIDLength as usize].copy_from_slice(&ssid_raw[..ssid.uSSIDLength as usize]);
        let mut list = DOT11_BSSID_LIST {
            Header: NDIS_OBJECT_HEADER {
                Type: 0x80, // NDIS_OBJECT_TYPE_DEFAULT
                Revision: 1, // DOT11_BSSID_LIST_REVISION_1
                Size: std::mem::size_of::<DOT11_BSSID_LIST>() as u16,
            },
            uNumOfEntries: 1,
            uTotalNumOfEntries: 1,
            BSSIDs: bssid,
        };
        let params = WLAN_CONNECTION_PARAMETERS {
            wlanConnectionMode: wlan_connection_mode_profile,
            strProfile: PCWSTR(wide.as_ptr()),
            pDot11Ssid: &mut ssid,
            pDesiredBssidList: &mut list,
            dot11BssType: dot11_BSS_type_infrastructure,
            dwFlags: 0,
        };
        let rc = unsafe { WlanConnect(self.handle, guid, &params, None) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(rc) }
    }

    /// Drop and re-establish the connection, letting Windows pick the AP. Used
    /// for a manual re-roam when no better access point is currently visible.
    pub fn reconnect(&self, guid: &GUID, profile: &str) -> Result<(), u32> {
        let wide: Vec<u16> = profile.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            WlanDisconnect(self.handle, guid, None);
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let params = WLAN_CONNECTION_PARAMETERS {
            wlanConnectionMode: wlan_connection_mode_profile,
            strProfile: PCWSTR(wide.as_ptr()),
            pDot11Ssid: std::ptr::null_mut(),
            pDesiredBssidList: std::ptr::null_mut(),
            dot11BssType: dot11_BSS_type_infrastructure,
            dwFlags: 0,
        };
        let rc = unsafe { WlanConnect(self.handle, guid, &params, None) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(rc) }
    }
}

pub fn describe_error(code: u32) -> String {
    format!("Win32 error {} ({})", code, WIN32_ERROR(code).to_hresult().message())
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---- pure helpers (unit-tested) --------------------------------------------

pub fn format_bssid(b: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5])
}

#[cfg(test)]
pub fn parse_bssid(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for slot in out.iter_mut() {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() { None } else { Some(out) }
}

/// Windows link quality: 0% is -100 dBm, 100% is -50 dBm.
pub fn pct_to_rssi(pct: u32) -> i32 {
    -100 + (pct.min(100) as i32) / 2
}

/// Centre frequency (kHz) to a band label and channel number.
pub fn band_and_channel(freq_khz: u32) -> (&'static str, u32) {
    let mhz = freq_khz / 1000;
    match mhz {
        2412..=2472 => ("2.4 GHz", (mhz - 2407) / 5),
        2484 => ("2.4 GHz", 14),
        5150..=5895 => ("5 GHz", (mhz - 5000) / 5),
        5955..=7115 => ("6 GHz", (mhz - 5950) / 5),
        _ => ("", 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bssid_round_trips() {
        let b = [0x02, 0x00, 0x5e, 0x10, 0x00, 0x01];
        assert_eq!(format_bssid(&b), "02:00:5e:10:00:01");
        assert_eq!(parse_bssid("02:00:5e:10:00:01"), Some(b));
        assert_eq!(parse_bssid("02:00:5e:10:00"), None);
        assert_eq!(parse_bssid("02:00:5e:10:00:01:00"), None);
        assert_eq!(parse_bssid("zz:00:5e:10:00:01"), None);
    }

    #[test]
    fn percentage_maps_to_dbm() {
        assert_eq!(pct_to_rssi(0), -100);
        assert_eq!(pct_to_rssi(100), -50);
        assert_eq!(pct_to_rssi(90), -55);
        assert_eq!(pct_to_rssi(250), -50);
    }

    #[test]
    fn frequencies_map_to_channels() {
        assert_eq!(band_and_channel(2_452_000), ("2.4 GHz", 9));
        assert_eq!(band_and_channel(2_484_000), ("2.4 GHz", 14));
        assert_eq!(band_and_channel(5_180_000), ("5 GHz", 36));
        assert_eq!(band_and_channel(5_745_000), ("5 GHz", 149));
        assert_eq!(band_and_channel(5_975_000), ("6 GHz", 5));
        assert_eq!(band_and_channel(0), ("", 0));
    }

    /// Live check of the roam primitive without changing APs: ask Windows to
    /// (re)connect to the BSSID we are already on. Run with `--ignored`.
    #[test]
    #[ignore = "touches the live Wi-Fi link"]
    fn live_reassociation_to_current_ap_keeps_the_link() {
        let client = WifiClient::open().expect("wlan service");
        let Some(iface) = client.primary_interface() else { return };
        let bss = client.bss_list(&iface.guid);
        let Some(link) = client.link(&iface.guid, &bss) else { return };
        client
            .connect_bssid(&iface.guid, &link.profile, &link.ssid_raw, link.bssid)
            .expect("WlanConnect with a desired BSSID must be accepted for a standard user");
        std::thread::sleep(std::time::Duration::from_secs(5));
        let bss = client.bss_list(&iface.guid);
        let after = client.link(&iface.guid, &bss).expect("still connected");
        assert_eq!(after.bssid, link.bssid);
        assert!(!after.info.band.is_empty(), "band should resolve from the BSS list");
        assert!(after.info.rssi < 0 && after.info.rssi > -100);
    }
}
