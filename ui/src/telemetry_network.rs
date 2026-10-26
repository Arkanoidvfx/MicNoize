use std::{sync::Mutex, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

static CACHE: Mutex<Option<(Instant, &'static str, u64)>> = Mutex::new(None);

fn possible_vpn(kind: u32, up: bool, description: &str) -> bool {
    let text = description.to_lowercase();
    up && (matches!(kind, 23 | 131) || ["wireguard", "wintun", "tap-windows", "openvpn", " vpn", "vpn "].iter().any(|word| text.contains(word)))
}

pub fn hint() -> serde_json::Value {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let Ok(mut cache) = CACHE.lock() else { return serde_json::Value::Null; };
    if cache.as_ref().is_none_or(|(at, _, _)| at.elapsed() >= Duration::from_secs(600)) {
        *cache = Some((Instant::now(), detect(), now));
    }
    let (_, status, at) = cache.as_ref().unwrap();
    serde_json::json!({"vpn":status,"observed_at":format!("{}Z", super::chrono_free_utc(*at))})
}

#[cfg(not(windows))]
fn detect() -> &'static str { "unknown" }

#[cfg(windows)]
fn detect() -> &'static str {
    // IP_ADAPTER_ADDRESSES_LH prefix; GetAdaptersAddresses allocates the full rows in our bounded buffer.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Adapter {
        alignment: u64, next: *const Adapter, name: *const u8,
        unicast: *const u8, anycast: *const u8, multicast: *const u8, dns: *const u8,
        suffix: *const u16, description: *const u16, friendly: *const u16,
        physical: [u8; 8], physical_len: u32, flags: u32, mtu: u32, kind: u32, status: u32,
    }
    #[link(name = "iphlpapi")]
    unsafe extern "system" { fn GetAdaptersAddresses(family: u32, flags: u32, reserved: *mut std::ffi::c_void, adapters: *mut Adapter, size: *mut u32) -> u32; }
    let mut size = 15 * 1024u32;
    for _ in 0..3 {
        if !(128..=1024*1024).contains(&size) { return "unknown"; }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let result = unsafe { GetAdaptersAddresses(0, 0x2f, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size) };
        if result == 111 { continue; }
        if result != 0 { return "unknown"; }
        let start = buffer.as_ptr() as usize;
        let end = start + buffer.len()*8;
        let mut ptr = start as *const Adapter;
        for _ in 0..1024 {
            if ptr.is_null() { break; }
            let address = ptr as usize;
            if address < start || address > end.saturating_sub(std::mem::size_of::<Adapter>()) { return "unknown"; }
            let row = unsafe { std::ptr::read_unaligned(ptr) };
            let mut description = Vec::new();
            let text = row.description as usize;
            if text >= start && text < end {
                for offset in (text..end.saturating_sub(1)).step_by(2).take(256) {
                    let value = unsafe { std::ptr::read_unaligned(offset as *const u16) };
                    if value == 0 { break; } description.push(value);
                }
            }
            if possible_vpn(row.kind, row.status == 1, &String::from_utf16_lossy(&description)) { return "possible"; }
            ptr = row.next;
        }
        return "unknown";
    }
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn tunnel_is_only_a_hint_and_inactive_adapters_do_not_count() {
        assert!(possible_vpn(131, true, "Tunnel"));
        assert!(possible_vpn(6, true, "WireGuard Tunnel"));
        assert!(possible_vpn(6, true, "TAP-Windows Adapter"));
        assert!(!possible_vpn(131, false, "WireGuard"));
        assert!(!possible_vpn(6, true, "Ethernet"));
    }
    #[test] fn observation_is_compact_cached_and_has_no_adapter_identity() {
        let first = hint(); assert_eq!(first, hint());
        assert!(matches!(first["vpn"].as_str(), Some("possible" | "unknown")));
        assert_eq!(first.as_object().unwrap().len(), 2);
    }
}
