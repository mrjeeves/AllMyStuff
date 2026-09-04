//! # allmystuff-inventory
//!
//! Scan a machine for **everything plugged into it** — the compute
//! (CPU, GPU, RAM), the storage, the networks, and the human-facing
//! peripherals: displays, microphones (including 4+ element arrays),
//! speakers, cameras, keyboards, mice, and the rest of the USB bus.
//!
//! The result is one [`Inventory`] value: a stable, `serde`-clean
//! description of the box. AllMyStuff turns each entry into a node on
//! the mesh graph you can wire to other devices; this crate's only job
//! is to find the things and describe them well.
//!
//! ```no_run
//! let inv = allmystuff_inventory::scan();
//! println!("{} has {} devices", inv.host.hostname, inv.device_count());
//! println!("{}", allmystuff_inventory::report::render(&inv));
//! ```
//!
//! ## Design
//!
//! * **Never panics, always returns.** Every probe degrades to "nothing
//!   here" on a missing file or denied read, so a scan inside a minimal
//!   container returns the same well-formed shape it does on a loaded
//!   desktop — just with fewer devices.
//! * **Linux is the reference platform.** It reads `/proc` and `/sys`
//!   directly (no shelling out for the hot paths). macOS and Windows
//!   reuse the cross-platform `sysinfo` host basics and scaffold the
//!   device classes via `system_profiler` / CIM.
//! * **Pure parsers, fixture tests.** The fiddly decoding — EDID timing,
//!   ALSA stream channels, `/proc/bus/input/devices` classification — is
//!   isolated into pure functions tested against real-world samples, so
//!   correctness doesn't depend on the hardware being present.

// The HID endpoint→physical-device merge. Linux and Windows feed their
// collectors through it; macOS doesn't enumerate inputs yet (the cfg keeps
// the module from reading as dead code there, while `test` keeps its unit
// tests running on every platform).
#[cfg(any(target_os = "linux", target_os = "windows", test))]
mod dedupe;
mod sys;
mod types;

// Listening-service discovery (the "sites" source): pure port/banner
// classification + a `/proc/net/tcp` parser, plus an opt-in active probe.
// Public so the pure helpers are reachable (and never read as dead code on
// platforms whose `collect_listening` is a scaffold) — the same shape as
// `report`.
pub mod listening;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{windows_disk_for_drive_letter, WindowsDiskInfo};

pub mod report;

pub use types::*;

use sysinfo::System;

/// Scan this machine and return a full [`Inventory`].
///
/// Cheap enough to call on a button press — the heaviest cost is the
/// `sysinfo` CPU/memory refresh and a handful of `/sys` directory walks.
pub fn scan() -> Inventory {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();

    let mut gpus = platform_gpus();
    enrich_nvidia(&mut gpus);
    let (microphones, speakers) = platform_audio();

    let mut inv = Inventory {
        scanned_at: unix_now(),
        host: sys::host_info(),
        cpu: sys::cpu(&system),
        memory: sys::memory(&system),
        gpus,
        storage: sys::storage(),
        networks: sys::networks(),
        displays: platform_displays(),
        microphones,
        speakers,
        cameras: platform_cameras(),
        inputs: platform_inputs(),
        usb: platform_usb(),
        // Passive only — which ports are in LISTEN, classified by the
        // well-known-port table. The active banner probe
        // ([`listening::probe_services`]) is a separate, opt-in step a
        // caller runs off this hot path, since `scan` must stay cheap.
        listening: platform_listening(),
        temps: temps(),
    };
    ensure_category_defaults(&mut inv);
    inv
}

/// Temperature sensors, strictly as the OS exposes them (sysinfo: hwmon on
/// Linux, SMC on macOS, ACPI thermal zones on Windows). No vendor drivers, no
/// elevation tricks — on many consumer Windows boards the ACPI zone is absent
/// or admin-gated, and this returns empty there; UIs hide the section rather
/// than show a blank. Readings that are non-finite, exactly 0 °C, or below
/// −40 °C are dropped — the classic "zone exists but reads garbage"
/// signatures, not temperatures a live machine reports.
pub fn temps() -> Vec<TempSensor> {
    let comps = sysinfo::Components::new_with_refreshed_list();
    let mut temps: Vec<TempSensor> = comps
        .iter()
        .filter_map(|c| {
            let celsius = c.temperature()?;
            if !celsius.is_finite() || celsius == 0.0 || celsius < -40.0 {
                return None;
            }
            Some(TempSensor {
                label: c.label().trim().to_string(),
                celsius,
            })
        })
        .collect();
    temps.sort_by(|a, b| a.label.cmp(&b.label));
    temps
}

/// Guarantee every device category that has a notion of "current default"
/// has exactly one marked. The Linux probe already flags the real default
/// (the ALSA default card, the built-in panel); this is the safety net for
/// the macOS / Windows scaffolds and for any degraded scan where the probe
/// found devices but had no signal to rank them — so the UI and routing can
/// always answer "which is the default here?" Idempotent: a category that
/// already has a default is left untouched.
fn ensure_category_defaults(inv: &mut Inventory) {
    fn ensure<T: DefaultFlag>(devices: &mut [T]) {
        if devices.iter().any(T::is_default) {
            return;
        }
        // Prefer an eligible device the type considers primary (a built-in
        // panel), else the first eligible one — both stable across rescans.
        // If nothing is eligible (e.g. a headless box with only disconnected
        // outputs), the category simply has no default, which is correct.
        let pick = devices
            .iter()
            .position(|d| d.is_eligible() && d.is_primary_hint())
            .or_else(|| devices.iter().position(T::is_eligible));
        if let Some(i) = pick {
            devices[i].set_default();
        }
    }
    ensure(&mut inv.microphones);
    ensure(&mut inv.speakers);
    ensure(&mut inv.displays);
    ensure(&mut inv.cameras);
}

/// Minimal interface over the device records that carry a `default` flag,
/// so [`ensure_category_defaults`] can treat them uniformly.
trait DefaultFlag {
    fn is_default(&self) -> bool;
    fn set_default(&mut self);
    /// Whether this device can be the category default at all. Default: yes;
    /// a display overrides it so a disconnected output is never chosen.
    fn is_eligible(&self) -> bool {
        true
    }
    /// Whether this device should win the fallback when nothing is marked
    /// (e.g. a built-in display). Default: no preference.
    fn is_primary_hint(&self) -> bool {
        false
    }
}

impl DefaultFlag for AudioDevice {
    fn is_default(&self) -> bool {
        self.default
    }
    fn set_default(&mut self) {
        self.default = true;
    }
}

impl DefaultFlag for Display {
    fn is_default(&self) -> bool {
        self.default
    }
    fn set_default(&mut self) {
        self.default = true;
    }
    fn is_eligible(&self) -> bool {
        self.connected
    }
    fn is_primary_hint(&self) -> bool {
        self.connected && self.internal
    }
}

impl DefaultFlag for Camera {
    fn is_default(&self) -> bool {
        self.default
    }
    fn set_default(&mut self) {
        self.default = true;
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- platform dispatch -----------------------------------------------
//
// Each device class routes to the active platform module, or to an empty
// vec on a platform we haven't taught the class yet. Kept as small
// dispatch fns rather than a `use platform::*` alias so each class can be
// implemented on different platforms independently.

macro_rules! platform_dispatch {
    ($name:ident, $ret:ty, $method:ident) => {
        fn $name() -> $ret {
            #[cfg(target_os = "linux")]
            {
                return linux::$method();
            }
            #[cfg(target_os = "macos")]
            {
                return macos::$method();
            }
            #[cfg(target_os = "windows")]
            {
                return windows::$method();
            }
            #[allow(unreachable_code)]
            Default::default()
        }
    };
}

platform_dispatch!(platform_gpus, Vec<Gpu>, collect_gpus);
platform_dispatch!(platform_displays, Vec<Display>, collect_displays);
platform_dispatch!(
    platform_audio,
    (Vec<AudioDevice>, Vec<AudioDevice>),
    collect_audio
);
platform_dispatch!(platform_cameras, Vec<Camera>, collect_cameras);
platform_dispatch!(platform_inputs, Vec<InputDevice>, collect_inputs);
platform_dispatch!(platform_usb, Vec<UsbDevice>, collect_usb);
platform_dispatch!(platform_listening, Vec<ListeningService>, collect_listening);

/// Fill in NVIDIA VRAM + model from `nvidia-smi` when it's on PATH.
/// amdgpu reports VRAM through sysfs already (handled in `linux.rs`);
/// NVIDIA needs the tool, so this is the one place we shell out, and only
/// if there's an NVIDIA card to ask about. Mirrors MyOwnLLM's probe.
fn enrich_nvidia(gpus: &mut [Gpu]) {
    if !gpus.iter().any(|g| g.vendor == GpuVendor::Nvidia) {
        return;
    }
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    // From the windowless GUI a console child would flash up its own
    // console window on Windows; run it without one.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = text.lines().filter_map(|l| {
        let (name, mib) = l.split_once(',')?;
        let mib: u64 = mib.trim().parse().ok()?;
        Some((name.trim().to_string(), mib * 1024 * 1024))
    });
    for g in gpus.iter_mut().filter(|g| g.vendor == GpuVendor::Nvidia) {
        if let Some((name, bytes)) = rows.next() {
            g.name = name;
            g.vram_bytes = Some(bytes);
            g.kind = GpuKind::Discrete;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_is_well_formed() {
        // The scan must always return a usable shape, even in a sandbox
        // with almost nothing mounted — that's the whole contract.
        let inv = scan();
        assert!(!inv.host.os.is_empty());
        assert!(inv.cpu.logical_cores >= 1);
        // CPU + memory are the two guaranteed devices.
        assert!(inv.device_count() >= 2);
        // Round-trips through JSON (the wire shape the GUI consumes).
        let json = serde_json::to_string(&inv).expect("serialize");
        let back: Inventory = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.cpu.logical_cores, inv.cpu.logical_cores);
    }

    #[test]
    fn array_microphone_is_flagged() {
        let mic = AudioDevice {
            id: "mic:1:0".into(),
            name: "ReSpeaker 4 Mic Array".into(),
            direction: AudioDirection::Input,
            channels: Some(4),
            card: Some("1".into()),
            default: false,
        };
        assert!(mic.is_array());

        let mono = AudioDevice {
            channels: Some(1),
            ..mic.clone()
        };
        assert!(!mono.is_array());

        // Output devices are never "arrays" regardless of channel count.
        let speaker = AudioDevice {
            direction: AudioDirection::Output,
            channels: Some(8),
            ..mic
        };
        assert!(!speaker.is_array());
    }

    #[test]
    fn category_defaults_are_guaranteed_and_idempotent() {
        let mut inv = scan();
        // Give the scan some devices regardless of the host it ran on, with
        // none marked default, and confirm the fallback marks exactly one.
        inv.cameras = vec![
            Camera {
                id: "cam:0".into(),
                name: "A".into(),
                path: None,
                default: false,
            },
            Camera {
                id: "cam:1".into(),
                name: "B".into(),
                path: None,
                default: false,
            },
        ];
        inv.displays = vec![
            Display {
                id: "d:ext".into(),
                name: "Ext".into(),
                connector: "HDMI-A-1".into(),
                connected: true,
                width_px: None,
                height_px: None,
                internal: false,
                default: false,
            },
            Display {
                id: "d:panel".into(),
                name: "Panel".into(),
                connector: "eDP-1".into(),
                connected: true,
                width_px: None,
                height_px: None,
                internal: true,
                default: false,
            },
        ];
        ensure_category_defaults(&mut inv);
        assert_eq!(inv.cameras.iter().filter(|c| c.default).count(), 1);
        assert!(
            inv.cameras[0].default,
            "first camera is the fallback default"
        );
        // The built-in panel wins the display fallback over the external.
        assert!(inv.displays.iter().find(|d| d.internal).unwrap().default);
        assert_eq!(inv.displays.iter().filter(|d| d.default).count(), 1);

        // Running again changes nothing (idempotent).
        ensure_category_defaults(&mut inv);
        assert_eq!(inv.cameras.iter().filter(|c| c.default).count(), 1);
        assert_eq!(inv.displays.iter().filter(|d| d.default).count(), 1);

        // A headless box (only disconnected outputs) gets *no* default
        // display — never a disconnected one.
        inv.displays = vec![Display {
            id: "d:off".into(),
            name: "Off".into(),
            connector: "HDMI-A-1".into(),
            connected: false,
            width_px: None,
            height_px: None,
            internal: false,
            default: false,
        }];
        ensure_category_defaults(&mut inv);
        assert!(inv.displays.iter().all(|d| !d.default));
    }
}
