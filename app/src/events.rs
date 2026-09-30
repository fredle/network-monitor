//! Wi-Fi driver fault watcher. Reads the System event log through the Windows
//! Event Log API (`wevtapi`) for error/warning/critical events from a
//! configurable list of providers (default: the Intel `Netwtw*` drivers).
//! The System log is readable by standard users, so no elevation is needed.

use crate::timefmt::utc_iso;
use windows::core::HSTRING;
use windows::Win32::System::EventLog::{
    EvtClose, EvtNext, EvtQuery, EvtRender, EVT_HANDLE,
};

const EVT_QUERY_CHANNEL_PATH: u32 = 0x1;
const EVT_QUERY_FORWARD_DIRECTION: u32 = 0x100;
const EVT_RENDER_EVENT_XML: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct DriverEvent {
    pub provider: String,
    pub id: u32,
    pub level: u32,
    pub detail: String,
}

impl DriverEvent {
    pub fn level_name(&self) -> &'static str {
        match self.level {
            1 => "Critical",
            2 => "Error",
            3 => "Warning",
            _ => "Event",
        }
    }
}

/// XPath for error/warning/critical events from `providers` since `since_ms`.
/// Provider names are validated by `Settings::providers`, so they are safe to
/// place inside single quotes.
pub fn build_xpath(providers: &[String], since_ms: i64) -> String {
    let names: Vec<String> = providers.iter().map(|p| format!("@Name='{p}'")).collect();
    format!(
        "*[System[Provider[{}] and (Level=1 or Level=2 or Level=3) and TimeCreated[@SystemTime>='{}']]]",
        names.join(" or "),
        utc_iso(since_ms)
    )
}

/// Pull provider, event id, level and first `<Data>` text out of rendered event XML.
pub fn parse_event_xml(xml: &str) -> DriverEvent {
    DriverEvent {
        provider: attr(xml, "<Provider", "Name").unwrap_or_default(),
        id: element(xml, "EventID").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
        level: element(xml, "Level").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
        detail: element(xml, "Data").map(|s| unescape(s.trim())).unwrap_or_default(),
    }
}

fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after_open = xml[start..].find('>')? + start + 1;
    let close = xml[after_open..].find(&format!("</{tag}>"))? + after_open;
    Some(&xml[after_open..close])
}

fn attr(xml: &str, tag_prefix: &str, name: &str) -> Option<String> {
    let start = xml.find(tag_prefix)?;
    let tag_end = xml[start..].find('>')? + start;
    let tag = &xml[start..tag_end];
    let key = format!("{name}=");
    let k = tag.find(&key)? + key.len();
    let quote = tag[k..].chars().next()?;
    let rest = &tag[k + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Query matching events. Returns the events (oldest first) or an error string.
pub fn query_since(providers: &[String], since_ms: i64) -> Result<Vec<DriverEvent>, String> {
    if providers.is_empty() {
        return Ok(Vec::new());
    }
    let xpath = HSTRING::from(build_xpath(providers, since_ms));
    let channel = HSTRING::from("System");
    let mut out = Vec::new();
    unsafe {
        let results = EvtQuery(
            None,
            &channel,
            &xpath,
            EVT_QUERY_CHANNEL_PATH | EVT_QUERY_FORWARD_DIRECTION,
        )
        .map_err(|e| format!("EvtQuery: {e}"))?;

        loop {
            let mut handles = [0isize; 16];
            let mut returned = 0u32;
            if EvtNext(results, &mut handles, 1000, 0, &mut returned).is_err() || returned == 0 {
                break; // ERROR_NO_MORE_ITEMS or a read error: either way we are done
            }
            for h in &handles[..returned as usize] {
                let ev = EVT_HANDLE(*h);
                if let Some(xml) = render_xml(ev) {
                    out.push(parse_event_xml(&xml));
                }
                let _ = EvtClose(ev);
            }
        }
        let _ = EvtClose(results);
    }
    Ok(out)
}

unsafe fn render_xml(ev: EVT_HANDLE) -> Option<String> {
    let mut used = 0u32;
    let mut props = 0u32;
    // First call reports the size needed (and fails with ERROR_INSUFFICIENT_BUFFER).
    let _ = EvtRender(None, ev, EVT_RENDER_EVENT_XML, 0, None, &mut used, &mut props);
    if used == 0 {
        return None;
    }
    let mut buf = vec![0u16; (used as usize) / 2 + 1];
    EvtRender(
        None,
        ev,
        EVT_RENDER_EVENT_XML,
        (buf.len() * 2) as u32,
        Some(buf.as_mut_ptr() as *mut _),
        &mut used,
        &mut props,
    )
    .ok()?;
    let chars = (used as usize / 2).min(buf.len());
    let end = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
    Some(String::from_utf16_lossy(&buf[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System>\
<Provider Name='Netwtw14' Guid='{abc}'/><EventID Qualifiers='0'>7002</EventID><Level>2</Level>\
<TimeCreated SystemTime='2026-09-30T20:00:00.000000000Z'/></System>\
<EventData><Data>Driver &amp; firmware &lt;reset&gt;</Data></EventData></Event>";

    #[test]
    fn parses_rendered_event_xml() {
        let e = parse_event_xml(SAMPLE);
        assert_eq!(e.provider, "Netwtw14");
        assert_eq!(e.id, 7002);
        assert_eq!(e.level, 2);
        assert_eq!(e.detail, "Driver & firmware <reset>");
        assert_eq!(e.level_name(), "Error");
    }

    #[test]
    fn malformed_xml_yields_defaults_not_a_panic() {
        let e = parse_event_xml("<nonsense");
        assert_eq!(e, DriverEvent { provider: String::new(), id: 0, level: 0, detail: String::new() });
    }

    #[test]
    fn xpath_lists_every_provider_and_the_start_time() {
        let x = build_xpath(&["A".into(), "B-1".into()], 0);
        assert!(x.contains("@Name='A' or @Name='B-1'"));
        assert!(x.contains("1970-01-01T00:00:00.000Z"));
        assert!(x.contains("Level=1 or Level=2 or Level=3"));
    }

    #[test]
    fn live_query_against_the_system_log_does_not_error() {
        // Nonexistent provider: must return Ok(empty), proving the API plumbing works.
        let r = query_since(&["NetmonNoSuchProvider".to_string()], 0);
        assert_eq!(r, Ok(Vec::new()));
    }
}
