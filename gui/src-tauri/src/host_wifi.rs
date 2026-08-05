//! What access points the **customer's computer** can see.
//!
//! A KVM that isn't on a network can't tell you what's nearby, and that is
//! precisely when someone is trying to put it on Wi-Fi. Worse, only the Pro has
//! a scan endpoint at all — on a plain NanoKVM the picker has never had
//! anything to show, so the SSID had to be typed from memory, which is where
//! this goes wrong: the 2.4 and 5 GHz variants of one network, a trailing
//! space, a name that isn't quite what's printed on the router.
//!
//! The machine the KVM is plugged into is in the same room on the same radio,
//! and already knows the answer — including which network it is *itself* on,
//! which is almost always the one the KVM should join.
//!
//! # What this deliberately does not do
//!
//! It does not read the Wi-Fi password. On every platform that is privileged:
//! Windows keeps it behind `netsh … key=clear` (Administrator) and, in the
//! profile XML, behind DPAPI under SYSTEM; macOS keeps it in the keychain
//! behind an authorization prompt; NetworkManager keeps it root-only. Scanning
//! needs none of that, so scanning is what this crate does — the customer types
//! their own password, which they know, into a form that no longer asks them to
//! also remember the network's exact name.

use serde::Serialize;

/// One access point the host can see.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Network {
    pub ssid: String,
    /// Signal in dBm, negative, strongest nearest zero.
    ///
    /// **Derived, not measured**, on hosts that report a percentage (Windows
    /// and NetworkManager both do). The conversion is the usual inverse of the
    /// quality mapping — `dBm = quality/2 - 100` — which is right to within a
    /// few dB across the useful range. It exists so these entries sort and
    /// render beside a KVM's own scan, which reports real dBm; treat it as an
    /// ordering, not a measurement.
    pub signal: Option<i32>,
}

/// What the host could tell us.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HostWifi {
    /// Whether this platform can scan at all. False is a fact about the
    /// operating system, not a failure — see `note`.
    pub supported: bool,
    /// The SSID this computer is currently joined to, if any. The single most
    /// useful field here: it is usually the network the KVM should be on.
    pub current: Option<String>,
    /// Everything in range, strongest first, deduplicated by SSID.
    pub networks: Vec<Network>,
    /// Why the list is empty, in words worth showing a human. `None` when
    /// there is nothing to explain.
    pub note: Option<String>,
}

/// Percent quality (0–100) → approximate dBm. See [`Network::signal`].
fn quality_to_dbm(quality: u32) -> i32 {
    (quality.min(100) as i32) / 2 - 100
}

/// Collapse to one entry per SSID, keeping the strongest, strongest first.
///
/// A single network is usually several access points and both bands, so a raw
/// scan lists the same name three or four times. The KVM joins by name, so the
/// duplicates are noise in a list whose whole job is to be picked from.
fn dedupe_strongest(mut nets: Vec<Network>) -> Vec<Network> {
    nets.sort_by(|a, b| {
        b.signal
            .unwrap_or(i32::MIN)
            .cmp(&a.signal.unwrap_or(i32::MIN))
    });
    let mut seen = std::collections::HashSet::new();
    nets.retain(|n| !n.ssid.is_empty() && seen.insert(n.ssid.clone()));
    nets
}

// ---- Windows ---------------------------------------------------------------

/// Parse `netsh wlan show networks mode=bssid`.
///
/// **Parsed by shape, not by label.** `netsh` output is localized — on a German
/// or French Windows every label is translated — so keying off "Signal" or
/// "Authentication" would work in English and silently return nothing for a
/// customer anywhere else. Two things survive translation: `SSID <n>` (an
/// acronym nobody translates) starts each block, and a signal reads as a number
/// followed by a percent sign. Those are the two fields that matter, so those
/// are the two this reads.
///
/// Security is deliberately not parsed: its label AND its value are both
/// localized, and the picker doesn't need it — the form asks for a password
/// either way.
pub fn parse_netsh_networks(out: &str) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    for line in out.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        let value = value.trim();

        // `SSID 3 : Some Network` — a new block. `BSSID 1 : aa:bb:…` must not
        // match, hence the check on the whole first token rather than a
        // contains(). An unnamed (hidden) network gives an empty value; it is
        // kept out because it can't be picked by name.
        let mut parts = label.split_whitespace();
        if parts.next() == Some("SSID") && parts.next().is_some_and(|n| n.parse::<u32>().is_ok()) {
            if !value.is_empty() {
                nets.push(Network {
                    ssid: value.to_string(),
                    signal: None,
                });
            }
            continue;
        }

        // A percentage anywhere in this block is the signal. French Windows
        // writes `87 %`, so the space is stripped before parsing.
        if let Some(num) = value.strip_suffix('%') {
            if let Ok(pct) = num.trim().parse::<u32>() {
                if let Some(last) = nets.last_mut() {
                    // Several BSSIDs per SSID: keep the strongest.
                    let dbm = quality_to_dbm(pct);
                    if last.signal.is_none_or(|s| dbm > s) {
                        last.signal = Some(dbm);
                    }
                }
            }
        }
    }
    dedupe_strongest(nets)
}

/// Pull the joined SSID out of `netsh wlan show interfaces`.
///
/// Same discipline: `SSID` is the label that survives localization, and the
/// exact-token match is what keeps `BSSID` from being read as the answer. A
/// disconnected adapter prints the label with an empty value, which reads as
/// "not on Wi-Fi" rather than an empty-string SSID.
pub fn parse_netsh_current(out: &str) -> Option<String> {
    for line in out.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        if label.trim() == "SSID" {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ---- Linux -----------------------------------------------------------------

/// Parse `nmcli -t -f IN-USE,SSID,SIGNAL dev wifi list`.
///
/// `-t` is the machine-readable mode: colon-separated, no padding, and — unlike
/// the human table — not localized. Colons inside an SSID are escaped by nmcli
/// as `\:`, so fields are split on unescaped colons only.
pub fn parse_nmcli_wifi(out: &str) -> (Option<String>, Vec<Network>) {
    let mut current = None;
    let mut nets = Vec::new();
    for line in out.lines() {
        let fields = split_nmcli(line);
        if fields.len() < 3 {
            continue;
        }
        let in_use = fields[0].trim() == "*";
        let ssid = fields[1].trim().to_string();
        if ssid.is_empty() {
            continue; // hidden network — nothing to pick
        }
        let signal = fields[2].trim().parse::<u32>().ok().map(quality_to_dbm);
        if in_use {
            current = Some(ssid.clone());
        }
        nets.push(Network { ssid, signal });
    }
    (current, dedupe_strongest(nets))
}

/// Split one `nmcli -t` record on unescaped `:`.
fn split_nmcli(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut escaped = false;
    for c in line.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            ':' if !escaped => out.push(String::new()),
            _ => {
                escaped = false;
                out.last_mut().expect("always non-empty").push(c);
            }
        }
    }
    out
}

// ---- macOS -----------------------------------------------------------------

/// Parse `networksetup -getairportnetwork <dev>`.
///
/// The label is localized but the format isn't: everything after the first
/// colon is the name. A machine that isn't associated answers with a sentence
/// containing no colon, which falls through to `None`.
pub fn parse_networksetup_current(out: &str) -> Option<String> {
    let (_, value) = out.split_once(':')?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

// ---- entry point -----------------------------------------------------------

/// Ask this computer what it can see.
///
/// Never fails: a platform that can't answer says so in `note` and the caller
/// falls back to the form it already has. Nothing here is privileged, so it
/// can't provoke a UAC or keychain prompt on a customer's machine.
pub fn scan() -> HostWifi {
    platform::scan()
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    /// Keep `netsh` from flashing a console window over the customer's screen.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn netsh(args: &[&str]) -> Option<String> {
        let out = Command::new("netsh")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        // netsh writes its console output in the OEM code page, so an SSID with
        // a non-ASCII character can arrive as invalid UTF-8. Lossy rather than
        // dropping the whole scan: one mangled name beats no list.
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn scan() -> HostWifi {
        let current = netsh(&["wlan", "show", "interfaces"])
            .as_deref()
            .and_then(parse_netsh_current);
        let Some(raw) = netsh(&["wlan", "show", "networks", "mode=bssid"]) else {
            return HostWifi {
                supported: false,
                current,
                networks: Vec::new(),
                note: Some("This computer didn't answer a Wi-Fi scan.".into()),
            };
        };
        let networks = parse_netsh_networks(&raw);
        let note = networks.is_empty().then(|| {
            "This computer sees no Wi-Fi networks — its wireless may be off or it may have no \
             wireless adapter."
                .to_string()
        });
        HostWifi {
            supported: true,
            current,
            networks,
            note,
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn scan() -> HostWifi {
        let out = Command::new("nmcli")
            .args(["-t", "-f", "IN-USE,SSID,SIGNAL", "dev", "wifi", "list"])
            .output();
        let Ok(out) = out else {
            return HostWifi {
                supported: false,
                note: Some(
                    "Scanning for Wi-Fi needs NetworkManager (nmcli), which isn't installed."
                        .into(),
                ),
                ..Default::default()
            };
        };
        let (current, networks) = parse_nmcli_wifi(&String::from_utf8_lossy(&out.stdout));
        let note = networks.is_empty().then(|| {
            "This computer sees no Wi-Fi networks — its wireless may be off or it may have no \
             wireless adapter."
                .to_string()
        });
        HostWifi {
            supported: true,
            current,
            networks,
            note,
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn scan() -> HostWifi {
        // Scanning is genuinely unavailable to an ordinary app here. The
        // `airport` tool lost its scan in macOS 14.4, and CoreWLAN's
        // `scanForNetworks` has required Location Services authorization since
        // 10.15 — a permission this app has no other reason to hold, and one
        // the customer would be right to find odd. Say so plainly instead of
        // returning an empty list that reads as "no networks nearby".
        //
        // The current network is still worth asking for: it's the field that
        // matters most, and on the versions where `networksetup` still answers
        // it costs nothing. Newer macOS gates this behind Location too, and
        // then it simply comes back empty.
        let current = Command::new("networksetup")
            .args(["-getairportnetwork", "en0"])
            .output()
            .ok()
            .and_then(|o| parse_networksetup_current(&String::from_utf8_lossy(&o.stdout)));
        HostWifi {
            supported: false,
            current,
            networks: Vec::new(),
            note: Some(
                "macOS doesn't let an app list nearby Wi-Fi networks without Location access, \
                 so type the network name below."
                    .into(),
            ),
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub fn scan() -> HostWifi {
        HostWifi {
            supported: false,
            note: Some("This computer can't scan for Wi-Fi networks.".into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netsh_reads_ssids_and_signal() {
        let out = "\
Interface name : Wi-Fi
There are 3 networks currently visible.

SSID 1 : Home-5G
    Network type            : Infrastructure
    Authentication          : WPA2-Personal
    Encryption              : CCMP
    BSSID 1                 : aa:bb:cc:dd:ee:01
         Signal             : 92%
         Radio type         : 802.11ac

SSID 2 : Neighbour
    Network type            : Infrastructure
    BSSID 1                 : aa:bb:cc:dd:ee:02
         Signal             : 40%

SSID 3 :
    Network type            : Infrastructure
    BSSID 1                 : aa:bb:cc:dd:ee:03
         Signal             : 70%
";
        let nets = parse_netsh_networks(out);
        // The hidden network (empty SSID) can't be picked by name, so it's out.
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0].ssid, "Home-5G");
        assert_eq!(nets[0].signal, Some(quality_to_dbm(92)));
        assert_eq!(nets[1].ssid, "Neighbour");
        assert!(nets[0].signal > nets[1].signal, "strongest first");
    }

    #[test]
    fn netsh_keeps_the_strongest_bssid_of_one_ssid() {
        // One network, two access points. The KVM joins by name, so the list
        // must show it once — at the signal of the nearer radio.
        let out = "\
SSID 1 : Office
    BSSID 1                 : aa:bb:cc:dd:ee:01
         Signal             : 30%
    BSSID 2                 : aa:bb:cc:dd:ee:02
         Signal             : 88%
";
        let nets = parse_netsh_networks(out);
        assert_eq!(nets.len(), 1);
        assert_eq!(nets[0].signal, Some(quality_to_dbm(88)));
    }

    #[test]
    fn netsh_survives_localization() {
        // A German and a French Windows. Every label differs; `SSID <n>` and a
        // percentage do not. Keying off "Signal"/"Authentication" would return
        // nothing here — which is the whole reason this parses by shape.
        let de = "\
Schnittstellenname : WLAN
Es sind 1 Netzwerke derzeit sichtbar.

SSID 1 : Fritzbox
    Netzwerktyp             : Infrastruktur
    Authentifizierung       : WPA2-Personal
    Verschlüsselung         : CCMP
    BSSID 1                 : aa:bb:cc:dd:ee:01
         Signal             : 75%
";
        let nets = parse_netsh_networks(de);
        assert_eq!(nets.len(), 1);
        assert_eq!(nets[0].ssid, "Fritzbox");
        assert_eq!(nets[0].signal, Some(quality_to_dbm(75)));

        // French puts a space before the percent sign.
        let fr = "\
SSID 1 : Livebox
    Type de réseau          : Infrastructure
    Authentification        : WPA2 - Personnel
    BSSID 1                 : aa:bb:cc:dd:ee:01
         Signal             : 64 %
";
        let nets = parse_netsh_networks(fr);
        assert_eq!(nets.len(), 1);
        assert_eq!(nets[0].ssid, "Livebox");
        assert_eq!(nets[0].signal, Some(quality_to_dbm(64)));
    }

    #[test]
    fn netsh_current_is_ssid_not_bssid() {
        // `BSSID` contains `SSID`, and it's the line right after. A contains()
        // match would hand back a MAC address as the network name.
        let out = "\
    Name                   : Wi-Fi
    State                  : connected
    SSID                   : Home-5G
    BSSID                  : aa:bb:cc:dd:ee:01
";
        assert_eq!(parse_netsh_current(out), Some("Home-5G".into()));
    }

    #[test]
    fn netsh_current_is_none_when_disconnected() {
        let out = "    Name : Wi-Fi\n    State : disconnected\n    SSID :\n";
        assert_eq!(parse_netsh_current(out), None);
    }

    #[test]
    fn nmcli_reads_in_use_and_signal() {
        let out = "*:Home-5G:92\n :Neighbour:40\n :Home-5G:55\n :\\:odd\\:name:60\n";
        let (current, nets) = parse_nmcli_wifi(out);
        assert_eq!(current, Some("Home-5G".into()));
        // Home-5G appears twice (two bands) and collapses to the stronger.
        assert_eq!(nets.len(), 3);
        assert_eq!(nets[0].ssid, "Home-5G");
        assert_eq!(nets[0].signal, Some(quality_to_dbm(92)));
        // An SSID containing colons is escaped by nmcli and must survive.
        assert!(nets.iter().any(|n| n.ssid == ":odd:name"));
    }

    #[test]
    fn networksetup_current() {
        assert_eq!(
            parse_networksetup_current("Current Wi-Fi Network: Home-5G\n"),
            Some("Home-5G".into())
        );
        // Not associated: a sentence with no colon.
        assert_eq!(
            parse_networksetup_current("You are not associated with an AirPort network.\n"),
            None
        );
    }

    #[test]
    fn quality_maps_across_the_useful_range() {
        assert_eq!(quality_to_dbm(100), -50);
        assert_eq!(quality_to_dbm(0), -100);
        assert!(quality_to_dbm(90) > quality_to_dbm(40));
        // Out-of-range input can't produce a nonsensically strong reading.
        assert_eq!(quality_to_dbm(255), -50);
    }
}
