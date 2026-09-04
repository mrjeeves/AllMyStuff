//! Windows device probing through WMI's in-process COM API.
//!
//! Linux (`linux.rs`) is the reference; this is the Windows implementation
//! of the same collector surface. Host basics (CPU/memory/storage/network)
//! come from `sysinfo`; everything here queries typed WMI rows directly.
//! Each probe is defensive — a failed query or a shape change degrades to
//! "nothing here" rather than a panic.

#![cfg(target_os = "windows")]

use serde::de::DeserializeOwned;
use serde::Deserialize;
use wmi::WMIConnection;

use crate::types::*;

/// The physical disk behind a drive letter, for KVM virtual-media reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsDiskInfo {
    pub number: u32,
    pub size: u64,
    pub usb: bool,
}

/// Run a read-only WMI query in-process. WMI initializes COM for this thread;
/// failures degrade to an empty result just as the former shell probes did.
fn query<T: DeserializeOwned>(namespace: &str, statement: &str) -> Vec<T> {
    WMIConnection::with_namespace_path(namespace)
        .and_then(|connection| connection.raw_query(statement))
        .unwrap_or_default()
}

fn cimv2<T: DeserializeOwned>(statement: &str) -> Vec<T> {
    query("ROOT\\CIMV2", statement)
}

/// Resolve a mounted drive through the native Windows Storage provider.
/// `BusType == 7` is `BusTypeUsb` from `STORAGE_BUS_TYPE`.
pub fn windows_disk_for_drive_letter(letter: char) -> Option<WindowsDiskInfo> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PartitionRow {
        disk_number: u32,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DiskRow {
        number: u32,
        size: u64,
        bus_type: u16,
    }

    let namespace = "ROOT\\Microsoft\\Windows\\Storage";
    let partition = query::<PartitionRow>(
        namespace,
        &format!(
            "SELECT DiskNumber FROM MSFT_Partition WHERE DriveLetter = '{}'",
            letter.to_ascii_uppercase()
        ),
    )
    .into_iter()
    .next()?;
    query::<DiskRow>(
        namespace,
        &format!(
            "SELECT Number, Size, BusType FROM MSFT_Disk WHERE Number = {}",
            partition.disk_number
        ),
    )
    .into_iter()
    .next()
    .map(|disk| WindowsDiskInfo {
        number: disk.number,
        size: disk.size,
        usb: disk.bus_type == 7,
    })
}

fn clean_string(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A placeholder DMI string firmware leaves in the *system* identity fields
/// on a custom-built PC — ASUS ships "System Product Name" / "System
/// manufacturer", other boards "To be filled by O.E.M." or "Default string".
/// Mirrors `linux::dmi_placeholder`; matched precisely so a real model that
/// merely starts with "System" (IBM "System x3650 M4") is left alone.
fn dmi_placeholder(v: &str) -> bool {
    let l = v.to_lowercase();
    v.is_empty()
        || l.contains("to be filled")
        || l.contains("system manufacturer")
        || l.contains("system product")
        || l.contains("default string")
        || l == "none"
}

/// A trimmed, non-empty WMI field that also isn't a DMI placeholder.
fn clean(value: Option<String>) -> Option<String> {
    clean_string(value).filter(|x| !dmi_placeholder(x))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BaseBoardRow {
    manufacturer: Option<String>,
    product: Option<String>,
}

/// The motherboard's own manufacturer + product from `Win32_BaseBoard`. On a
/// custom build `Win32_ComputerSystem` carries only the "System Product Name"
/// placeholder, and the real identity is here — `Product` is the sibling of
/// `Manufacturer` ("PRIME X570-P" next to "ASUSTeK COMPUTER INC.").
fn baseboard() -> (Option<String>, Option<String>) {
    cimv2::<BaseBoardRow>("SELECT Manufacturer, Product FROM Win32_BaseBoard")
        .into_iter()
        .next()
        .map(|row| (clean(row.manufacturer), clean(row.product)))
        .unwrap_or_default()
}

/// The motherboard's own product string, exactly as `Win32_BaseBoard.Product`
/// reports it — verbatim. Deliberately NO placeholder filtering, NO
/// manufacturer prefixing, NO fallback to the system record: the Board row
/// shows whatever the system has for the field.
pub fn board_label() -> Option<String> {
    cimv2::<BaseBoardRow>("SELECT Manufacturer, Product FROM Win32_BaseBoard")
        .into_iter()
        .next()
        .and_then(|row| clean_string(row.product))
}

/// Just the product / model name — the friendly `Win32_ComputerSystem.Model`
/// an OEM burns in ("OptiPlex 7090", not "Dell Inc. OptiPlex 7090"). On a
/// custom build that field is only the "System Product Name" placeholder, so
/// fall back to the motherboard's own product — the sibling of its
/// manufacturer in `Win32_BaseBoard` ("PRIME X570-P").
pub fn product_label() -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ComputerSystemRow {
        model: Option<String>,
    }
    let sys_model = cimv2::<ComputerSystemRow>("SELECT Model FROM Win32_ComputerSystem")
        .into_iter()
        .next()
        .and_then(|row| clean(row.model));
    sys_model.or_else(|| baseboard().1)
}

pub fn soc_label() -> Option<String> {
    None
}

pub fn collect_gpus() -> Vec<Gpu> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct VideoControllerRow {
        name: Option<String>,
        adapter_ram: Option<u64>,
        driver_version: Option<String>,
    }
    cimv2::<VideoControllerRow>("SELECT Name, AdapterRAM, DriverVersion FROM Win32_VideoController")
        .into_iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let name = clean_string(row.name)?;
            let lname = name.to_lowercase();
            let vendor = if lname.contains("nvidia") {
                GpuVendor::Nvidia
            } else if lname.contains("amd") || lname.contains("radeon") {
                GpuVendor::Amd
            } else if lname.contains("intel") {
                GpuVendor::Intel
            } else {
                GpuVendor::Other
            };
            // AdapterRAM is a uint32 and wraps for >4 GB cards; treat 0 /
            // missing as unknown rather than wrong.
            let vram_bytes = row.adapter_ram.filter(|&b| b > 0);
            Some(Gpu {
                id: format!("gpu:{i}"),
                name,
                vendor,
                vram_bytes,
                kind: if vendor == GpuVendor::Intel {
                    GpuKind::Integrated
                } else if vram_bytes.is_some() {
                    GpuKind::Discrete
                } else {
                    GpuKind::Unknown
                },
                driver: clean_string(row.driver_version),
            })
        })
        .collect()
}

pub fn collect_displays() -> Vec<Display> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ResolutionRow {
        current_horizontal_resolution: Option<u32>,
        current_vertical_resolution: Option<u32>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct MonitorIdRow {
        user_friendly_name: Option<Vec<u16>>,
        instance_name: Option<String>,
    }

    // Per-monitor native resolution needs the EDID timing block; keep the
    // former best-effort primary resolution until that richer parser lands.
    let resolution = cimv2::<ResolutionRow>(
        "SELECT CurrentHorizontalResolution, CurrentVerticalResolution \
         FROM Win32_VideoController WHERE CurrentHorizontalResolution IS NOT NULL",
    )
    .into_iter()
    .next();
    let (width_px, height_px) = resolution
        .map(|row| {
            (
                row.current_horizontal_resolution,
                row.current_vertical_resolution,
            )
        })
        .unwrap_or_default();

    query::<MonitorIdRow>(
        "ROOT\\WMI",
        "SELECT UserFriendlyName, InstanceName FROM WmiMonitorID",
    )
    .into_iter()
    .enumerate()
    .map(|(i, row)| {
        let name = row
            .user_friendly_name
            .map(|name| {
                let end = name
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(name.len());
                String::from_utf16_lossy(&name[..end]).trim().to_string()
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Display {i}"));
        let connector = clean_string(row.instance_name).unwrap_or_default();
        let internal =
            connector.to_uppercase().contains("LCD") || name.to_lowercase().contains("internal");
        Display {
            id: format!("display:{i}"),
            name,
            connector,
            connected: true,
            width_px,
            height_px,
            internal,
            default: false,
        }
    })
    .collect()
}

pub fn collect_audio() -> (Vec<AudioDevice>, Vec<AudioDevice>) {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct AudioEndpointRow {
        name: Option<String>,
        device_id: Option<String>,
    }
    let (mut mics, mut speakers) = (Vec::new(), Vec::new());
    let endpoints = cimv2::<AudioEndpointRow>(
        "SELECT Name, DeviceID FROM Win32_PnPEntity WHERE PNPClass = 'AudioEndpoint'",
    );
    for (i, row) in endpoints.into_iter().enumerate() {
        let Some(name) = clean_string(row.name) else {
            continue;
        };
        let l = name.to_lowercase();
        let is_input = l.contains("microphone")
            || l.contains("mic ")
            || l.contains("line in")
            || l.contains("capture")
            || l.contains("input");
        let dev = AudioDevice {
            id: format!("{}:{i}", if is_input { "mic" } else { "spk" }),
            name,
            direction: if is_input {
                AudioDirection::Input
            } else {
                AudioDirection::Output
            },
            channels: None,
            card: clean_string(row.device_id),
            default: false,
        };
        if is_input {
            mics.push(dev);
        } else {
            speakers.push(dev);
        }
    }
    (mics, speakers)
}

pub fn collect_cameras() -> Vec<Camera> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct CameraRow {
        name: Option<String>,
        pnp_class: Option<String>,
    }
    // Webcams register under PNPClass 'Camera' on most modern drivers but
    // 'Image' on plenty of others (UVC devices especially) — query both.
    // 'Image' also covers scanners, so those rows only count when the name
    // says camera; 'Camera'-class rows are taken at their word.
    cimv2::<CameraRow>(
        "SELECT Name, PNPClass FROM Win32_PnPEntity \
         WHERE PNPClass = 'Camera' OR PNPClass = 'Image'",
    )
    .into_iter()
    .filter(|row| {
        let class = row.pnp_class.as_deref().unwrap_or_default();
        if class.eq_ignore_ascii_case("camera") {
            return true;
        }
        let name = row.name.as_deref().unwrap_or_default().to_lowercase();
        name.contains("cam") || name.contains("video")
    })
    .enumerate()
    .filter_map(|(i, row)| {
        Some(Camera {
            id: format!("cam:{i}"),
            name: clean_string(row.name)?,
            path: None,
            default: false,
        })
    })
    .collect()
}

pub fn collect_inputs() -> Vec<InputDevice> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct InputRow {
        name: Option<String>,
        description: Option<String>,
        pnp_device_id: Option<String>,
    }
    // One physical device registers a WMI row per HID interface ("HID
    // Keyboard Device" three times over); merge them by the PnP VID:PID.
    // WMI rows carry no stable per-port path, so two identical units of
    // one model merge too — an accepted trade for a readable list (these
    // entries are display-only input sources).
    let mut raw = Vec::new();
    for (i, row) in cimv2::<InputRow>("SELECT Name, Description, PNPDeviceID FROM Win32_Keyboard")
        .into_iter()
        .enumerate()
    {
        let name = clean_string(row.name)
            .or_else(|| clean_string(row.description))
            .unwrap_or_else(|| "Keyboard".into());
        raw.push(crate::dedupe::RawInput {
            group: row.pnp_device_id.as_deref().and_then(pnp_vid_pid),
            fallback_id: format!("input:kbd:{i}"),
            name,
            kind: InputKind::Keyboard,
        });
    }
    for (i, row) in
        cimv2::<InputRow>("SELECT Name, Description, PNPDeviceID FROM Win32_PointingDevice")
            .into_iter()
            .enumerate()
    {
        let name = clean_string(row.name)
            .or_else(|| clean_string(row.description))
            .unwrap_or_else(|| "Pointer".into());
        let l = name.to_lowercase();
        let kind = if l.contains("touchpad") || l.contains("trackpad") {
            InputKind::Touchpad
        } else {
            InputKind::Mouse
        };
        raw.push(crate::dedupe::RawInput {
            group: row.pnp_device_id.as_deref().and_then(pnp_vid_pid),
            fallback_id: format!("input:pt:{i}"),
            name,
            kind,
        });
    }
    crate::dedupe::merge_inputs(raw)
}

/// `HID\VID_046D&PID_C52B&MI_01\8&2f662e1&0&0000` → `046d:c52b` — the
/// physical unit's identity, shared by all its interfaces.
fn pnp_vid_pid(id: &str) -> Option<String> {
    let (vid, pid) = parse_usb_id(id)?;
    Some(format!("{vid}:{pid}"))
}

pub fn collect_usb() -> Vec<UsbDevice> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct UsbRow {
        name: Option<String>,
        manufacturer: Option<String>,
        device_id: Option<String>,
    }
    let mut out = Vec::new();
    for row in cimv2::<UsbRow>(
        "SELECT Name, Manufacturer, DeviceID FROM Win32_PnPEntity \
         WHERE DeviceID LIKE 'USB%'",
    ) {
        let Some(device_id) = clean_string(row.device_id) else {
            continue;
        };
        let Some((vid, pid)) = parse_usb_id(&device_id) else {
            continue;
        };
        // Skip Microsoft/host root entries that aren't really peripherals.
        let name = clean_string(row.name).unwrap_or_else(|| format!("USB {vid}:{pid}"));
        out.push(UsbDevice {
            id: format!("usb:{vid}:{pid}"),
            name,
            vendor_id: vid,
            product_id: pid,
            manufacturer: clean_string(row.manufacturer),
            class: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// `USB\VID_046D&PID_C52B\...` → (`046d`, `c52b`).
fn parse_usb_id(device_id: &str) -> Option<(String, String)> {
    let up = device_id.to_uppercase();
    let vid = up.split("VID_").nth(1)?.get(..4)?.to_lowercase();
    let pid = up.split("PID_").nth(1)?.get(..4)?.to_lowercase();
    (vid.chars().all(|c| c.is_ascii_hexdigit()) && pid.chars().all(|c| c.is_ascii_hexdigit()))
        .then_some((vid, pid))
}

/// Enumerate listening TCP ports from the standard Windows networking WMI
/// provider, tagged with a best-effort owning process name.
pub fn collect_listening() -> Vec<ListeningService> {
    use std::collections::HashMap;

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ConnectionRow {
        local_address: Option<String>,
        local_port: u16,
        owning_process: Option<u32>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ProcessRow {
        process_id: u32,
        name: Option<String>,
    }

    let processes: HashMap<u32, String> =
        cimv2::<ProcessRow>("SELECT ProcessId, Name FROM Win32_Process")
            .into_iter()
            .filter_map(|row| clean_string(row.name).map(|name| (row.process_id, name)))
            .collect();
    let rows = query::<ConnectionRow>(
        "ROOT\\StandardCimv2",
        "SELECT LocalAddress, LocalPort, OwningProcess FROM MSFT_NetTCPConnection WHERE State = 2",
    )
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "LocalAddress": row.local_address.unwrap_or_default(),
            "LocalPort": row.local_port,
            "Process": row
                .owning_process
                .and_then(|pid| processes.get(&pid))
                .cloned()
                .unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();
    crate::listening::services_from_nettcp_rows(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usb_device_id() {
        assert_eq!(
            parse_usb_id("USB\\VID_046D&PID_C52B\\5&1234"),
            Some(("046d".into(), "c52b".into()))
        );
        assert_eq!(parse_usb_id("HID\\nope"), None);
    }

    #[test]
    fn pnp_id_yields_the_units_group_key() {
        // The MI_xx interface suffix differs per endpoint; the key doesn't.
        assert_eq!(
            pnp_vid_pid("HID\\VID_046D&PID_C52B&MI_00\\8&2f662e1&0&0000").as_deref(),
            Some("046d:c52b")
        );
        assert_eq!(
            pnp_vid_pid("HID\\VID_046D&PID_C52B&MI_01\\9&aaaa&0&0000").as_deref(),
            Some("046d:c52b")
        );
        assert_eq!(pnp_vid_pid("ACPI\\PNP0303\\4&1ab2c3d&0"), None);
    }
}
