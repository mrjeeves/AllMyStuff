//! Cross-platform host basics via `sysinfo` — CPU, memory, storage,
//! networks, and the OS/uptime fields. The richer per-device classes
//! (displays, audio, cameras, input, USB) are platform-specific and live
//! in `linux.rs` / `macos.rs` / `windows.rs`.

use std::collections::BTreeMap;

use sysinfo::{Disks, Networks, System};

use crate::types::*;

pub fn host_info() -> HostInfo {
    let arch = std::env::consts::ARCH.to_string();

    #[cfg(target_os = "linux")]
    let (board, product, soc) = (
        crate::linux::board_label(),
        crate::linux::product_label(),
        crate::linux::soc_label(),
    );
    #[cfg(target_os = "macos")]
    let (board, product, soc) = (
        crate::macos::board_label(),
        crate::macos::product_label(),
        crate::macos::soc_label(),
    );
    #[cfg(target_os = "windows")]
    let (board, product, soc) = (
        crate::windows::board_label(),
        crate::windows::product_label(),
        crate::windows::soc_label(),
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let (board, product, soc) = (None, None, None);

    HostInfo {
        hostname: device_hostname(),
        os: std::env::consts::OS.to_string(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        arch,
        board,
        product,
        soc,
        uptime_secs: System::uptime(),
    }
}

/// The device's hostname for its node card and presence advert.
///
/// `sysinfo` reports the OS hostname, which is the right name on a desktop
/// ("chris-mbp"). But phones have no meaningful hostname to give: iOS — and
/// most modern Android — always answer the loopback placeholder "localhost".
/// Left as-is, a fresh phone advertises itself to the whole mesh as
/// "Localhost". Treat that (and an empty name) as "no hostname" and fall
/// back to a friendly device-class name the user can still override from
/// settings.
fn device_hostname() -> String {
    match System::host_name() {
        Some(name) if is_real_hostname(&name) => name,
        _ => default_device_name(),
    }
}

/// Whether `name` is a real machine hostname rather than the loopback
/// placeholder. Case and any trailing FQDN dot are ignored, and the loopback
/// name is rejected in every form the platforms hand back — bare "localhost",
/// "localhost.local", "localhost.localdomain".
fn is_real_hostname(name: &str) -> bool {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return false;
    }
    let first_label = name.split('.').next().unwrap_or(name);
    !first_label.eq_ignore_ascii_case("localhost")
}

/// Friendly stand-in when the OS reports no real hostname — named by device
/// class where we know it (a phone), else the neutral "this device" the
/// inventory has always used for the unnamed case.
fn default_device_name() -> String {
    match std::env::consts::OS {
        "ios" => "iPhone",
        "android" => "Android phone",
        _ => "this device",
    }
    .to_string()
}

pub fn cpu(sys: &System) -> Cpu {
    let cpus = sys.cpus();
    let first = cpus.first();

    // sysinfo sometimes blanks the brand inside minimal VMs; fall back to
    // the raw cpuinfo line on Linux before giving up.
    let brand = first
        .map(|c| c.brand().trim().to_string())
        .filter(|b| !b.is_empty());
    #[cfg(target_os = "linux")]
    let brand = brand.or_else(crate::linux::cpu_brand_fallback);
    let brand = brand.unwrap_or_else(|| "Unknown CPU".into());

    let vendor = first
        .map(|c| c.vendor_id().trim().to_string())
        .filter(|v| !v.is_empty());

    let max_mhz = first.map(|c| c.frequency()).filter(|&f| f > 0);

    Cpu {
        brand,
        vendor,
        physical_cores: System::physical_core_count(),
        logical_cores: cpus.len().max(1),
        max_mhz,
    }
}

pub fn memory(sys: &System) -> Memory {
    Memory {
        total_bytes: sys.total_memory(),
        available_bytes: sys.available_memory(),
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
    }
}

pub fn storage() -> Vec<StorageVolume> {
    // On macOS sysinfo asks CoreFoundation for capacity before it filters
    // remote volumes. That can block on our own WebDAV mount (or an offline
    // network drive). Enumerate first, then query capacity only for the local
    // disks that survived sysinfo's filtering.
    let mut disks = Disks::new_with_refreshed_list_specifics(
        sysinfo::DiskRefreshKind::everything().without_storage(),
    );
    // Dedupe by mount point — bind mounts and the like surface the same
    // volume several times.
    let mut by_mount: BTreeMap<String, StorageVolume> = BTreeMap::new();
    for d in disks.list_mut() {
        let mount = d.mount_point().to_string_lossy().to_string();
        // Skip the usual pseudo / read-only system mounts that aren't
        // "storage you'd share."
        if is_pseudo_mount(&mount) {
            continue;
        }
        d.refresh_specifics(sysinfo::DiskRefreshKind::nothing().with_storage());
        let name = d.name().to_string_lossy().to_string();
        let kind = if d.is_removable() {
            DiskKind::Removable
        } else {
            match d.kind() {
                sysinfo::DiskKind::SSD => DiskKind::Ssd,
                sysinfo::DiskKind::HDD => DiskKind::Hdd,
                _ => DiskKind::Unknown,
            }
        };
        let vol = StorageVolume {
            id: format!("disk:{mount}"),
            name: if name.is_empty() { mount.clone() } else { name },
            mount_point: Some(mount.clone()),
            filesystem: Some(d.file_system().to_string_lossy().to_string())
                .filter(|s| !s.is_empty()),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
            removable: d.is_removable(),
            kind,
        };
        by_mount.insert(mount, vol);
    }
    by_mount.into_values().collect()
}

fn is_pseudo_mount(mount: &str) -> bool {
    mount.starts_with("/proc")
        || mount.starts_with("/sys")
        || mount.starts_with("/dev")
        || mount.starts_with("/run")
        || mount.starts_with("/snap")
        || mount == "/boot/efi"
}

pub fn networks() -> Vec<NetworkInterface> {
    let nets = Networks::new_with_refreshed_list();
    let mut out = Vec::new();
    for (name, data) in &nets {
        let mac = {
            let m = data.mac_address().to_string();
            (m != "00:00:00:00:00:00").then_some(m)
        };

        // IP addresses (sysinfo ≥ 0.31 surfaces these per interface).
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for ipn in data.ip_networks() {
            match ipn.addr {
                std::net::IpAddr::V4(a) => ipv4.push(a.to_string()),
                std::net::IpAddr::V6(a) => ipv6.push(a.to_string()),
            }
        }

        #[cfg(target_os = "linux")]
        let (kind, up, speed_mbps) = crate::linux::net_detail(name);
        #[cfg(not(target_os = "linux"))]
        let (kind, up, speed_mbps) = (
            if name == "lo" || name.starts_with("lo") {
                NetKind::Loopback
            } else {
                NetKind::Unknown
            },
            true,
            None,
        );

        out.push(NetworkInterface {
            id: format!("net:{name}"),
            name: name.clone(),
            mac,
            kind,
            up,
            speed_mbps,
            ipv4,
            ipv6,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_placeholder_is_not_a_real_hostname() {
        // The exact value iOS (and modern Android) hands back, plus the
        // FQDN/casing variants seen in the wild.
        assert!(!is_real_hostname("localhost"));
        assert!(!is_real_hostname("LocalHost"));
        assert!(!is_real_hostname("LOCALHOST"));
        assert!(!is_real_hostname("localhost."));
        assert!(!is_real_hostname("localhost.local"));
        assert!(!is_real_hostname("localhost.localdomain"));
        // Empty / whitespace-only is "no hostname" too.
        assert!(!is_real_hostname(""));
        assert!(!is_real_hostname("   "));
    }

    #[test]
    fn genuine_hostnames_are_kept() {
        assert!(is_real_hostname("chris-mbp"));
        assert!(is_real_hostname("Studio.local"));
        assert!(is_real_hostname("desk-pc.lan"));
        // A host that merely *contains* "localhost" is still a real name.
        assert!(is_real_hostname("localhost-2"));
        assert!(is_real_hostname("my-localhost"));
    }

    #[test]
    fn default_device_name_is_never_the_placeholder() {
        let name = default_device_name();
        assert!(!name.is_empty());
        assert!(
            is_real_hostname(&name),
            "the fallback must itself be usable"
        );
    }

    #[test]
    fn scanned_hostname_is_never_the_loopback_placeholder() {
        // Whatever the host running the test reports, the assembled hostname
        // must never be the loopback placeholder — the bug this guards.
        assert!(is_real_hostname(&host_info().hostname));
    }
}
