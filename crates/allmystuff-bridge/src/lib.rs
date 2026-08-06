//! # allmystuff-bridge
//!
//! The seam between hardware and graph: turn a scanned [`Inventory`] into
//! the routable [`Capability`] nodes the graph wires together. Shared by
//! the headless CLI and the Tauri backend so "what this machine exposes"
//! is computed in exactly one place.
//!
//! Two kinds of capability come out of a machine:
//!
//!  * **Physical** — one per real device the scan found. A mic is an audio
//!    *source*, a speaker an audio *sink*, a camera a video source, a
//!    monitor a display sink, a keyboard an input source, a volume a duplex
//!    storage endpoint.
//!  * **Synthetic** — a small fixed set that represents "the machine
//!    itself" so whole-computer flows work: its **screen** (a display
//!    source you can cast or use as a remote desktop), **control** (an
//!    input sink that lets a remote keyboard/mouse drive it), its
//!    **keyboard & mouse** (an input *source* — what a console window
//!    forwards is "whatever this machine's user does", not one scanned
//!    device, so driving a remote never depends on the input scan finding
//!    hardware — macOS finds none), **system audio** (a duplex
//!    endpoint for its mixer), **video in** (a video *sink* — the app
//!    shows inbound camera streams in a window, so any machine can receive
//!    one), and its **clipboard** (a duplex endpoint a console pushes a
//!    paste into). These are what whole-computer flows (remote control, a
//!    room's screen share, a camera feed) land on at the far end.
//!
//! This is the only place hardware vocabulary meets graph vocabulary, so
//! it's small and thoroughly tested.

use allmystuff_graph::{Capability, Flow, MediaKind, NodeId};
use allmystuff_inventory::{InputKind, Inventory};
use allmystuff_protocol::InventorySummary;

pub mod sites;

/// One additional capturable screen beyond the primary — supplied by the
/// caller that actually owns a capture stack (the GUI, from its monitor
/// enumeration), since the cross-platform inventory can't promise its
/// display list maps onto what the capturer can grab. `id` must round-trip
/// through the capability id (`<node>:screen:<id>`) back to the same
/// monitor on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSource {
    pub id: u32,
    pub label: String,
}

/// Build every capability `node` should expose, given its scan.
pub fn capabilities_from_inventory(inv: &Inventory, node: &NodeId) -> Vec<Capability> {
    capabilities_with_screens(inv, node, &[])
}

/// [`capabilities_from_inventory`], plus one display source per extra
/// capturable screen — so a multi-monitor machine offers each monitor as
/// its own console tab. The primary stays the plain `screen` capability.
pub fn capabilities_with_screens(
    inv: &Inventory,
    node: &NodeId,
    screens: &[ScreenSource],
) -> Vec<Capability> {
    let mut caps = Vec::new();
    let n = node.as_str();
    let mk = |id: String, label: String, media, flow, origin: &str| {
        Capability::new(node.clone(), id, label, media, flow, origin.to_string())
    };

    // ---- synthetic "the machine itself" -----------------------------
    caps.push(mk(
        format!("{n}:screen"),
        "Screen".into(),
        MediaKind::Display,
        Flow::Source,
        "screen",
    ));
    for s in screens {
        caps.push(mk(
            format!("{n}:screen:{}", s.id),
            s.label.clone(),
            MediaKind::Display,
            Flow::Source,
            "screen",
        ));
    }
    caps.push(mk(
        format!("{n}:control"),
        "Keyboard & mouse control".into(),
        MediaKind::Input,
        Flow::Sink,
        "control",
    ));
    caps.push(mk(
        format!("{n}:keyboard-mouse"),
        "Keyboard & mouse".into(),
        MediaKind::Input,
        Flow::Source,
        "controller",
    ));
    caps.push(mk(
        format!("{n}:system-audio"),
        "System audio".into(),
        MediaKind::Audio,
        Flow::Duplex,
        "system",
    ));
    // The machine's clipboard — duplex, since a console can both push into
    // it (paste here) and, in time, pull from it (the planned cross-machine
    // copy/paste). Like control, it's a property of the machine itself, not
    // of any scanned device.
    caps.push(mk(
        format!("{n}:clipboard"),
        "Clipboard".into(),
        MediaKind::Clipboard,
        Flow::Duplex,
        "clipboard",
    ));
    // The video sink every camera stream lands on: the app renders inbound
    // camera video in a window (console stage, room tile), so "can show
    // video" is a property of the machine itself — like control — not of
    // any scanned device. Without it a camera source has nowhere to go:
    // monitors are *display* sinks, deliberately a different media.
    caps.push(mk(
        format!("{n}:video-in"),
        "Video in".into(),
        MediaKind::Video,
        Flow::Sink,
        "viewer",
    ));
    // The landing point for a drive another machine explicitly maps here.
    // Physical volumes remain Storage sources/duplex endpoints; this synthetic
    // sink prevents the graph from pretending a remote USB drive is being
    // copied *onto* one of this machine's real disks.
    caps.push(mk(
        format!("{n}:storage-in"),
        "Mapped drives".into(),
        MediaKind::Storage,
        Flow::Sink,
        "storage-in",
    ));

    // ---- physical devices -------------------------------------------
    //
    // The scan flags each category's current default (the mic the machine
    // captures from, the display it drives first…); that flag rides onto
    // the capability so the UI can badge it and routing can prefer it.
    for m in &inv.microphones {
        let label = if m.is_array() {
            format!("{} (array)", m.name)
        } else {
            m.name.clone()
        };
        caps.push(
            mk(
                qualify(n, &m.id),
                label,
                MediaKind::Audio,
                Flow::Source,
                "microphone",
            )
            .as_default(m.default),
        );
    }
    for s in &inv.speakers {
        caps.push(
            mk(
                qualify(n, &s.id),
                s.name.clone(),
                MediaKind::Audio,
                Flow::Sink,
                "speaker",
            )
            .as_default(s.default),
        );
    }
    for c in &inv.cameras {
        caps.push(
            mk(
                qualify(n, &c.id),
                c.name.clone(),
                MediaKind::Video,
                Flow::Source,
                "camera",
            )
            .as_default(c.default),
        );
    }
    for d in &inv.displays {
        // Only connected monitors are wireable sinks.
        if !d.connected {
            continue;
        }
        caps.push(
            mk(
                qualify(n, &d.id),
                d.name.clone(),
                MediaKind::Display,
                Flow::Sink,
                "display",
            )
            .as_default(d.default),
        );
    }
    for i in &inv.inputs {
        // Keyboards/mice/etc. are input *sources* (they produce events).
        let origin = match i.kind {
            InputKind::Keyboard => "keyboard",
            InputKind::Mouse => "mouse",
            InputKind::Touchpad => "touchpad",
            InputKind::Touchscreen => "touchscreen",
            InputKind::Gamepad => "gamepad",
            InputKind::Tablet => "tablet",
            InputKind::Other => "input",
        };
        caps.push(mk(
            qualify(n, &i.id),
            i.name.clone(),
            MediaKind::Input,
            Flow::Source,
            origin,
        ));
    }
    for v in &inv.storage {
        // Skip the tiny pseudo-volumes; share real ones both ways.
        if v.total_bytes < 1 << 30 {
            continue;
        }
        caps.push(mk(
            qualify(n, &v.id),
            v.name.clone(),
            MediaKind::Storage,
            Flow::Duplex,
            "storage",
        ));
    }

    caps
}

/// A thumbnail of the machine for the presence advert / node card.
pub fn node_summary(inv: &Inventory) -> InventorySummary {
    InventorySummary {
        os: match &inv.host.os_version {
            Some(v) => format!("{} {}", inv.host.os, v),
            None => inv.host.os.clone(),
        },
        cpu: inv.cpu.brand.clone(),
        ram_bytes: inv.memory.total_bytes,
        device_count: inv.device_count() as u32,
        // The product/model name identifies the machine to a CEC technician;
        // empty when DMI has nothing usable (the wire default).
        product: inv.host.product.clone().unwrap_or_default(),
    }
}

/// Namespace a device id under its node so the same inventory id on two
/// machines stays distinct on the graph.
fn qualify(node: &str, device_id: &str) -> String {
    format!("{node}:{device_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_inventory::*;

    fn empty_inventory() -> Inventory {
        Inventory {
            scanned_at: 0,
            host: HostInfo {
                hostname: "box".into(),
                os: "linux".into(),
                os_version: None,
                kernel_version: None,
                arch: "x86_64".into(),
                board: None,
                product: None,
                soc: None,
                uptime_secs: 0,
            },
            cpu: Cpu {
                brand: "Test CPU".into(),
                vendor: None,
                physical_cores: Some(4),
                logical_cores: 8,
                max_mhz: None,
            },
            memory: Memory {
                total_bytes: 16 << 30,
                available_bytes: 8 << 30,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            gpus: vec![],
            storage: vec![],
            networks: vec![],
            displays: vec![],
            microphones: vec![],
            speakers: vec![],
            cameras: vec![],
            inputs: vec![],
            usb: vec![],
            listening: vec![],
            temps: vec![],
        }
    }

    #[test]
    fn every_machine_exposes_screen_control_controller_and_system_audio() {
        let inv = empty_inventory();
        let caps = capabilities_from_inventory(&inv, &NodeId::this());
        let by_origin = |o: &str| caps.iter().find(|c| c.origin == o).cloned();

        let screen = by_origin("screen").expect("screen");
        assert_eq!(screen.media, MediaKind::Display);
        assert_eq!(screen.flow, Flow::Source);
        assert_eq!(screen.id.as_str(), "this:screen");

        let control = by_origin("control").expect("control");
        assert_eq!(
            (control.media, control.flow),
            (MediaKind::Input, Flow::Sink)
        );

        // The outbound twin: every machine can *drive* a remote, even when
        // the input scan found no devices (macOS today).
        let controller = by_origin("controller").expect("keyboard & mouse source");
        assert_eq!(
            (controller.media, controller.flow),
            (MediaKind::Input, Flow::Source)
        );
        assert_eq!(controller.id.as_str(), "this:keyboard-mouse");

        let sys = by_origin("system").expect("system audio");
        assert_eq!((sys.media, sys.flow), (MediaKind::Audio, Flow::Duplex));

        // The camera landing spot: every machine can *show* inbound video,
        // so every machine is a video sink — cameras need somewhere to go.
        let viewer = by_origin("viewer").expect("video in");
        assert_eq!((viewer.media, viewer.flow), (MediaKind::Video, Flow::Sink));
        assert_eq!(viewer.id.as_str(), "this:video-in");

        let mapped = by_origin("storage-in").expect("mapped drives");
        assert_eq!(
            (mapped.media, mapped.flow),
            (MediaKind::Storage, Flow::Sink)
        );
        assert_eq!(mapped.id.as_str(), "this:storage-in");

        // The clipboard bridge: duplex, so a console can paste into it (and,
        // later, pull from it for cross-machine copy/paste).
        let clip = by_origin("clipboard").expect("clipboard");
        assert_eq!(
            (clip.media, clip.flow),
            (MediaKind::Clipboard, Flow::Duplex)
        );
        assert_eq!(clip.id.as_str(), "this:clipboard");
    }

    #[test]
    fn extra_screens_become_display_sources_after_the_primary() {
        let inv = empty_inventory();
        let screens = [
            ScreenSource {
                id: 7,
                label: "Screen — DELL U2723QE".into(),
            },
            ScreenSource {
                id: 12,
                label: "Screen 3".into(),
            },
        ];
        let caps = capabilities_with_screens(&inv, &NodeId::this(), &screens);
        let sources: Vec<_> = caps
            .iter()
            .filter(|c| c.origin == "screen")
            .map(|c| c.id.as_str().to_string())
            .collect();
        // Primary first (its id sorts first too — match_endpoint's
        // tie-break keeps auto-picks landing on the primary).
        assert_eq!(
            sources,
            vec!["this:screen", "this:screen:7", "this:screen:12"]
        );
        let named = caps
            .iter()
            .find(|c| c.id.as_str() == "this:screen:7")
            .unwrap();
        assert_eq!(named.label, "Screen — DELL U2723QE");
        assert_eq!(
            (named.media, named.flow),
            (MediaKind::Display, Flow::Source)
        );
    }

    #[test]
    fn devices_map_to_the_right_media_and_flow() {
        let mut inv = empty_inventory();
        inv.microphones.push(AudioDevice {
            id: "mic:1:0".into(),
            name: "ReSpeaker".into(),
            direction: AudioDirection::Input,
            channels: Some(4),
            card: Some("1".into()),
            default: true,
        });
        inv.speakers.push(AudioDevice {
            id: "spk:0:0".into(),
            name: "Speakers".into(),
            direction: AudioDirection::Output,
            channels: None,
            card: Some("0".into()),
            default: false,
        });
        inv.cameras.push(Camera {
            id: "cam:video0".into(),
            name: "Webcam".into(),
            path: None,
            default: true,
        });
        inv.displays.push(Display {
            id: "display:HDMI-A-1".into(),
            name: "Monitor".into(),
            connector: "HDMI-A-1".into(),
            connected: true,
            width_px: Some(1920),
            height_px: Some(1080),
            internal: false,
            default: true,
        });
        inv.inputs.push(InputDevice {
            id: "input:input1".into(),
            name: "Keyboard".into(),
            kind: InputKind::Keyboard,
            endpoints: 1,
        });

        let caps = capabilities_from_inventory(&inv, &NodeId::this());
        let find = |origin: &str| caps.iter().find(|c| c.origin == origin).unwrap();

        // The scan's per-category default flag rides onto the capability.
        assert!(
            find("microphone").default,
            "mic was the default capture device"
        );
        assert!(find("camera").default);
        assert!(find("display").default);
        assert!(!find("speaker").default, "this speaker wasn't the default");
        // Synthetic machine endpoints aren't a category default.
        assert!(!find("system").default);

        assert_eq!(
            (find("microphone").media, find("microphone").flow),
            (MediaKind::Audio, Flow::Source)
        );
        // The array tag rides through to the friendly label.
        assert!(find("microphone").label.contains("array"));
        assert_eq!(
            (find("speaker").media, find("speaker").flow),
            (MediaKind::Audio, Flow::Sink)
        );
        assert_eq!(
            (find("camera").media, find("camera").flow),
            (MediaKind::Video, Flow::Source)
        );
        assert_eq!(
            (find("display").media, find("display").flow),
            (MediaKind::Display, Flow::Sink)
        );
        assert_eq!(
            (find("keyboard").media, find("keyboard").flow),
            (MediaKind::Input, Flow::Source)
        );

        // Ids are namespaced under the node.
        assert_eq!(find("microphone").id.as_str(), "this:mic:1:0");
    }

    #[test]
    fn disconnected_displays_are_not_wireable() {
        let mut inv = empty_inventory();
        inv.displays.push(Display {
            id: "display:DP-3".into(),
            name: "Nothing".into(),
            connector: "DP-3".into(),
            connected: false,
            width_px: None,
            height_px: None,
            internal: false,
            default: false,
        });
        let caps = capabilities_from_inventory(&inv, &NodeId::this());
        assert!(caps.iter().all(|c| c.origin != "display"));
    }

    #[test]
    fn summary_reflects_the_scan() {
        let inv = empty_inventory();
        let s = node_summary(&inv);
        assert_eq!(s.cpu, "Test CPU");
        assert_eq!(s.ram_bytes, 16 << 30);
        assert_eq!(s.os, "linux");
        assert!(s.device_count >= 2);
    }
}
