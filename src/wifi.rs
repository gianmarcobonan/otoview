//! Switching the PC to the endoscope's own Wi-Fi (open network, SSID `softish-xxxx`)
//! and back, using each OS's standard tool: netsh (Windows), networksetup (macOS),
//! nmcli (Linux). All calls block for seconds: run them off the UI thread.

use crate::command;
use std::time::{Duration, Instant};

/// SSID prefixes of known endoscopes.
// ponytail: only the prefix documented by the original app; add others as devices show up.
const PREFIXES: &[&str] = &["softish"];

pub fn is_endoscope(ssid: &str) -> bool {
    let s = ssid.to_ascii_lowercase();
    PREFIXES.iter().any(|p| s.starts_with(p))
}

fn run(prog: &str, args: &[&str]) -> Result<String, String> {
    let out = command(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("{} {}", text.trim(), err.trim()).trim().to_string())
    }
}

/// Value after " : " on lines whose label is exactly `label` (netsh layout).
fn netsh_values<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| {
            let k = k.trim();
            k == label || k.strip_prefix(label).is_some_and(|n| n.trim().parse::<u32>().is_ok())
        })
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .collect()
}

/// SSID the PC is connected to now.
pub fn current_ssid() -> Option<String> {
    let s = if cfg!(windows) {
        let out = run("netsh", &["wlan", "show", "interfaces"]).ok()?;
        netsh_values(&out, "SSID").first()?.to_string()
    } else if cfg!(target_os = "macos") {
        let out = run("networksetup", &["-getairportnetwork", &mac_device()?]).ok()?;
        out.split_once(": ")?.1.trim().to_string()
    } else {
        let out = run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"]).ok()?;
        out.lines().find_map(|l| l.strip_prefix("yes:"))?.to_string()
    };
    (!s.is_empty()).then_some(s)
}

/// Visible endoscope networks (fresh scan, a few seconds).
pub fn scan() -> Result<Vec<String>, String> {
    let all: Vec<String> = if cfg!(windows) {
        #[cfg(windows)]
        win::trigger_scan();
        let out = run("netsh", &["wlan", "show", "networks"])?;
        netsh_values(&out, "SSID").into_iter().map(String::from).collect()
    } else if cfg!(target_os = "macos") {
        // Network names are the indented "Name:" lines under "Other Local Wi-Fi Networks".
        let out = run("system_profiler", &["SPAirPortDataType"])?;
        out.split("Other Local Wi-Fi Networks:")
            .nth(1)
            .unwrap_or("")
            .lines()
            .filter(|l| l.starts_with("            ") && !l.starts_with("             ") && l.trim_end().ends_with(':'))
            .map(|l| l.trim().trim_end_matches(':').to_string())
            .collect()
    } else {
        let out = run("nmcli", &["-t", "-f", "ssid", "dev", "wifi", "list", "--rescan", "yes"])?;
        out.lines().map(String::from).collect()
    };
    let mut found: Vec<String> = all.into_iter().filter(|s| is_endoscope(s)).collect();
    found.sort();
    found.dedup();
    Ok(found)
}

/// Joins an open network and waits until the OS reports it as current.
pub fn connect_open(ssid: &str) -> Result<(), String> {
    if cfg!(windows) {
        // Windows needs a profile; "manual" so it never auto-joins the endoscope later.
        let esc = ssid.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let xml = format!(
            r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>{esc}</name><SSIDConfig><SSID><name>{esc}</name></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>manual</connectionMode><MSM><security><authEncryption><authentication>open</authentication><encryption>none</encryption><useOneX>false</useOneX></authEncryption></security></MSM></WLANProfile>"#
        );
        let path = std::env::temp_dir().join("otoview-wifi-profile.xml");
        std::fs::write(&path, xml).map_err(|e| e.to_string())?;
        let file = format!("filename={}", path.display());
        run("netsh", &["wlan", "add", "profile", &file, "user=current"])?;
        let _ = std::fs::remove_file(&path);
        run("netsh", &["wlan", "connect", &format!("name={ssid}"), &format!("ssid={ssid}")])?;
    } else if cfg!(target_os = "macos") {
        run("networksetup", &["-setairportnetwork", &mac_device().ok_or("no Wi-Fi")?, ssid])?;
    } else {
        run("nmcli", &["dev", "wifi", "connect", ssid])?;
    }
    wait_for(ssid)
}

/// Back to a network the PC already knows (saved password).
pub fn connect_known(ssid: &str) -> Result<(), String> {
    if cfg!(windows) {
        run("netsh", &["wlan", "connect", &format!("name={ssid}")])?;
    } else if cfg!(target_os = "macos") {
        run("networksetup", &["-setairportnetwork", &mac_device().ok_or("no Wi-Fi")?, ssid])?;
    } else {
        run("nmcli", &["con", "up", "id", ssid]).or_else(|_| run("nmcli", &["dev", "wifi", "connect", ssid]))?;
    }
    wait_for(ssid)
}

fn wait_for(ssid: &str) -> Result<(), String> {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if current_ssid().as_deref() == Some(ssid) {
            // Give DHCP a moment to assign an address.
            std::thread::sleep(Duration::from_secs(2));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("timeout: {ssid}"))
}

/// The OS Wi-Fi settings, as a fallback.
pub fn open_settings() {
    let _ = if cfg!(windows) {
        command("explorer").arg("ms-settings:network-wifi").spawn()
    } else if cfg!(target_os = "macos") {
        command("open").arg("x-apple.systempreferences:com.apple.wifi-settings-extension").spawn()
    } else {
        command("nm-connection-editor").spawn()
    };
}

/// macOS Wi-Fi device name (usually en0).
fn mac_device() -> Option<String> {
    let out = run("networksetup", &["-listallhardwareports"]).ok()?;
    let mut lines = out.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "Hardware Port: Wi-Fi" {
            return lines.next()?.strip_prefix("Device: ").map(|d| d.trim().to_string());
        }
    }
    None
}

#[cfg(windows)]
mod win {
    //! `netsh` only lists Windows' cached scan; ask the driver for a fresh one.
    use std::ffi::c_void;

    #[link(name = "wlanapi")]
    unsafe extern "system" {
        fn WlanOpenHandle(ver: u32, res: *const c_void, neg: *mut u32, h: *mut isize) -> u32;
        fn WlanEnumInterfaces(h: isize, res: *const c_void, list: *mut *mut u8) -> u32;
        fn WlanScan(h: isize, guid: *const u8, ssid: *const c_void, ie: *const c_void, res: *const c_void) -> u32;
        fn WlanFreeMemory(p: *mut u8);
        fn WlanCloseHandle(h: isize, res: *const c_void) -> u32;
    }

    /// Starts a scan on every Wi-Fi interface and waits for it to complete.
    pub fn trigger_scan() {
        // WLAN_INTERFACE_INFO_LIST: two u32, then entries of GUID(16) + WCHAR[256] + u32.
        const ENTRY: usize = 16 + 512 + 4;
        unsafe {
            let (mut neg, mut h) = (0u32, 0isize);
            if WlanOpenHandle(2, std::ptr::null(), &mut neg, &mut h) != 0 {
                return;
            }
            let mut list: *mut u8 = std::ptr::null_mut();
            if WlanEnumInterfaces(h, std::ptr::null(), &mut list) == 0 {
                let n = *(list as *const u32);
                for i in 0..n as usize {
                    let guid = list.add(8 + i * ENTRY);
                    WlanScan(h, guid, std::ptr::null(), std::ptr::null(), std::ptr::null());
                }
                WlanFreeMemory(list);
            }
            WlanCloseHandle(h, std::ptr::null());
        }
        // Drivers must finish within 4 s.
        std::thread::sleep(std::time::Duration::from_secs(4));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netsh_parsing() {
        let it = "    Nome                   : Wi-Fi\n    SSID                   : Casa Rossi\n    BSSID AP               : 2c:c8:1b:c6:43:80\n";
        assert_eq!(netsh_values(it, "SSID"), ["Casa Rossi"]);
        let list = "SSID 1 : Casa Rossi\n    Autenticazione : WPA2\nSSID 2 : softish-1A2B\nSSID 3 : \n";
        assert_eq!(netsh_values(list, "SSID"), ["Casa Rossi", "softish-1A2B"]);
    }

    #[test]
    fn endoscope_names() {
        assert!(is_endoscope("softish-1A2B"));
        assert!(is_endoscope("SOFTISH-77"));
        assert!(!is_endoscope("Casa Rossi"));
    }
}
