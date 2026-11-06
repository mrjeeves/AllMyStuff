//! GNOME's paired native screen-cast and remote-input session.
//!
//! Both halves belong to the same D-Bus client and are stopped together. This
//! supports an existing, unlocked GNOME desktop; the compositor may inhibit
//! remote access at the lock screen. See Mutter's RemoteDesktop, ScreenCast and
//! DisplayConfig introspection XML (GNOME 42 and later).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use zbus::blocking::{connection::Builder, Connection, Proxy};
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};

use crate::wayland_input::DesktopControl;

const REMOTE: &str = "org.gnome.Mutter.RemoteDesktop";
const REMOTE_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const REMOTE_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const CAST: &str = "org.gnome.Mutter.ScreenCast";
const CAST_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const CAST_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const CAST_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const DISPLAY: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

type Properties = HashMap<String, OwnedValue>;
type MonitorSpec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Properties);
type PhysicalMonitor = (MonitorSpec, Vec<Mode>, Properties);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, Properties);
type DisplayState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, Properties);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub id: u32,
    pub connector: String,
    pub label: String,
    pub primary: bool,
    /// Captured pixel dimensions, including the logical monitor's rotation.
    pub width: u32,
    pub height: u32,
}

fn connection() -> Result<Connection, String> {
    Builder::session()
        .map_err(|e| format!("GNOME session bus: {e}"))?
        .method_timeout(METHOD_TIMEOUT)
        .build()
        .map_err(|e| format!("GNOME session bus: {e}"))
}

/// Probe service ownership without creating a session or displaying a dialog.
/// `true` means the APIs exist, not that GNOME currently permits remote access.
pub fn available() -> Result<bool, String> {
    let conn = connection()?;
    let bus = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|e| e.to_string())?;
    for name in [REMOTE, CAST, DISPLAY] {
        let owned: bool = bus
            .call_method("NameHasOwner", &(name))
            .map_err(|e| format!("GNOME service lookup: {e}"))?
            .body()
            .deserialize()
            .map_err(|e| format!("GNOME service lookup reply: {e}"))?;
        if !owned {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn monitors() -> Result<Vec<Monitor>, String> {
    read_monitors(&connection()?)
}

fn read_monitors(conn: &Connection) -> Result<Vec<Monitor>, String> {
    let proxy = Proxy::new(conn, DISPLAY, DISPLAY_PATH, DISPLAY).map_err(|e| e.to_string())?;
    let state: DisplayState = proxy
        .call_method("GetCurrentState", &())
        .map_err(|e| format!("GNOME monitor discovery: {e}"))?
        .body()
        .deserialize()
        .map_err(|e| format!("GNOME monitor state: {e}"))?;
    monitors_from_state(&state.1, &state.2)
}

fn bool_property(properties: &Properties, name: &str) -> bool {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn monitors_from_state(
    physical: &[PhysicalMonitor],
    logical: &[LogicalMonitor],
) -> Result<Vec<Monitor>, String> {
    if !logical.is_empty() && logical.iter().filter(|m| m.4).count() != 1 {
        return Err("GNOME did not identify exactly one primary logical monitor".into());
    }
    let mut monitors = Vec::new();
    let mut connectors = HashSet::new();
    for logical in logical {
        // A mirrored logical monitor can contain several connectors. Choose a
        // deterministic primary representative, but keep explicit connector IDs.
        let mut specs: Vec<_> = logical.5.iter().collect();
        specs.sort_by(|a, b| a.0.cmp(&b.0));
        if specs.is_empty() {
            return Err("GNOME returned a logical monitor without a connector".into());
        }
        for (index, spec) in specs.into_iter().enumerate() {
            if spec.0.is_empty() || !connectors.insert(spec.0.clone()) {
                return Err("GNOME returned an empty or duplicate active connector".into());
            }
            let matches: Vec<_> = physical.iter().filter(|m| &m.0 == spec).collect();
            let [monitor] = matches.as_slice() else {
                return Err(format!(
                    "GNOME connector {} has ambiguous monitor identity",
                    spec.0
                ));
            };
            let current: Vec<_> = monitor
                .1
                .iter()
                .filter(|mode| bool_property(&mode.6, "is-current"))
                .collect();
            let [mode] = current.as_slice() else {
                return Err(format!(
                    "GNOME connector {} has no unique current mode",
                    spec.0
                ));
            };
            let (width, height) = oriented_size(mode.1, mode.2, logical.3)?;
            let label = monitor
                .2
                .get("display-name")
                .and_then(|value| <&str>::try_from(value).ok())
                .filter(|name| !name.trim().is_empty())
                .map(|name| format!("{name} ({})", spec.0))
                .unwrap_or_else(|| spec.0.clone());
            monitors.push(Monitor {
                id: connector_id(&spec.0),
                connector: spec.0.clone(),
                label,
                primary: logical.4 && index == 0,
                width,
                height,
            });
        }
    }
    validate_ids(&monitors)?;
    monitors.sort_by(|a, b| a.connector.cmp(&b.connector));
    Ok(monitors)
}

fn oriented_size(width: i32, height: i32, transform: u32) -> Result<(u32, u32), String> {
    if width <= 0 || height <= 0 || transform > 7 {
        return Err("GNOME returned invalid monitor geometry".into());
    }
    let size = (width as u32, height as u32);
    Ok(if transform % 2 == 1 {
        (size.1, size.0)
    } else {
        size
    })
}

fn connector_id(connector: &str) -> u32 {
    // Fixed FNV-1a, rather than DefaultHasher's unspecified algorithm. The high
    // bit keeps native connector IDs distinct from ordinary small XRandR IDs.
    connector.bytes().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    }) | 0x8000_0000
}

fn validate_ids(monitors: &[Monitor]) -> Result<(), String> {
    let mut ids = HashMap::new();
    for monitor in monitors {
        if let Some(previous) = ids.insert(monitor.id, &monitor.connector) {
            return Err(format!(
                "GNOME connector IDs collide for {previous} and {}; refusing ambiguous output",
                monitor.connector
            ));
        }
    }
    Ok(())
}

fn select_monitor(monitors: &[Monitor], id: Option<u32>) -> Result<&Monitor, String> {
    let matches: Vec<_> = monitors
        .iter()
        .filter(|monitor| id.map_or(monitor.primary, |id| monitor.id == id))
        .collect();
    match matches.as_slice() {
        [monitor] => Ok(monitor),
        _ => Err(match id {
            Some(id) => format!("GNOME monitor {id:#x} is unavailable or ambiguous"),
            None => "GNOME primary monitor is unavailable or ambiguous".into(),
        }),
    }
}

/// Owns capture independently of the input registry's shared control handle.
/// Dropping capture stops the session even if a controller still holds its Arc.
pub struct NativeCapture {
    owner: SessionOwner,
    control: Arc<Control>,
    node_id: u32,
}

impl NativeCapture {
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub(crate) fn control(&self) -> Arc<dyn DesktopControl> {
        self.control.clone()
    }

    /// Follow the negotiated PipeWire pixels when the compositor resizes a
    /// stream without changing its logical monitor rectangle.
    pub fn set_size(&self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.control
                .size
                .store(pack_size(width, height), Ordering::Release);
        }
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        self.owner.close();
    }
}

struct SessionOwner {
    conn: Option<Connection>,
    remote: OwnedObjectPath,
    active: Arc<AtomicBool>,
    listeners: Vec<JoinHandle<()>>,
}

impl SessionOwner {
    fn close(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(conn) = self.conn.take() {
            if let Ok(proxy) = Proxy::new(&conn, REMOTE, self.remote.clone(), REMOTE_SESSION) {
                let _ = proxy.call_method("Stop", &());
            }
            // This is a dedicated session connection. Closing it also wakes
            // signal listeners after errors/timeouts, so no waiter is leaked.
            let _ = conn.close();
        }
        for listener in self.listeners.drain(..) {
            let _ = listener.join();
        }
    }
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn open(monitor_id: Option<u32>) -> Result<NativeCapture, String> {
    open_on(connection()?, monitor_id)
}

fn open_on(conn: Connection, monitor_id: Option<u32>) -> Result<NativeCapture, String> {
    let monitors = read_monitors(&conn)?;
    let monitor = select_monitor(&monitors, monitor_id)?;
    let manager = Proxy::new(&conn, REMOTE, REMOTE_PATH, REMOTE).map_err(|e| e.to_string())?;
    let remote: OwnedObjectPath = manager
        .call_method("CreateSession", &())
        .map_err(|e| format!("GNOME remote desktop unavailable (the session may be locked): {e}"))?
        .body()
        .deserialize()
        .map_err(|e| format!("GNOME remote desktop session: {e}"))?;
    let active = Arc::new(AtomicBool::new(false));
    let mut owner = SessionOwner {
        conn: Some(conn.clone()),
        remote: remote.clone(),
        active: active.clone(),
        listeners: Vec::new(),
    };
    let input =
        Proxy::new(&conn, REMOTE, remote.clone(), REMOTE_SESSION).map_err(|e| e.to_string())?;
    let session_id: String = input
        .get_property("SessionId")
        .map_err(|e| format!("GNOME remote desktop session ID: {e}"))?;
    let cast = Proxy::new(&conn, CAST, CAST_PATH, CAST).map_err(|e| e.to_string())?;
    let options = HashMap::from([(
        "remote-desktop-session-id",
        Value::from(session_id.as_str()),
    )]);
    let cast_path: OwnedObjectPath = cast
        .call_method("CreateSession", &(options))
        .map_err(|e| format!("GNOME paired screen cast: {e}"))?
        .body()
        .deserialize()
        .map_err(|e| format!("GNOME screen cast session: {e}"))?;
    let session = Proxy::new(&conn, CAST, cast_path, CAST_SESSION).map_err(|e| e.to_string())?;
    let options = HashMap::from([("cursor-mode", Value::from(1u32))]);
    let stream: OwnedObjectPath = session
        .call_method("RecordMonitor", &(monitor.connector.as_str(), options))
        .map_err(|e| format!("GNOME monitor {}: {e}", monitor.connector))?
        .body()
        .deserialize()
        .map_err(|e| format!("GNOME monitor stream: {e}"))?;

    let (ready_tx, ready_rx) = mpsc::channel();
    let (node_tx, node_rx) = mpsc::channel();
    let listener_conn = conn.clone();
    let listener_stream = stream.clone();
    owner.listeners.push(
        std::thread::Builder::new()
            .name("mutter-stream-ready".into())
            .spawn(move || {
                let subscribe = || {
                    Proxy::new(&listener_conn, CAST, listener_stream, CAST_STREAM)?
                        .receive_signal("PipeWireStreamAdded")
                };
                let mut signals = match subscribe() {
                    Ok(signals) => {
                        let _ = ready_tx.send(Ok(()));
                        signals
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let result = signals
                    .next()
                    .ok_or_else(|| "GNOME stream closed before exposing its PipeWire node".into())
                    .and_then(|message| {
                        message
                            .body()
                            .deserialize::<u32>()
                            .map_err(|e| e.to_string())
                    });
                let _ = node_tx.send(result);
            })
            .map_err(|e| format!("GNOME stream listener: {e}"))?,
    );
    ready_rx
        .recv_timeout(METHOD_TIMEOUT)
        .map_err(|_| "GNOME stream subscription timed out".to_string())??;

    let (closed_ready_tx, closed_ready_rx) = mpsc::channel();
    let closed_conn = conn.clone();
    let closed_path = remote.clone();
    let closed_active = active.clone();
    owner.listeners.push(
        std::thread::Builder::new()
            .name("mutter-session-closed".into())
            .spawn(move || {
                let subscribe = || {
                    Proxy::new(&closed_conn, REMOTE, closed_path, REMOTE_SESSION)?
                        .receive_signal("Closed")
                };
                match subscribe() {
                    Ok(mut signals) => {
                        let _ = closed_ready_tx.send(Ok(()));
                        let _ = signals.next();
                        closed_active.store(false, Ordering::Release);
                    }
                    Err(error) => {
                        let _ = closed_ready_tx.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|e| format!("GNOME session listener: {e}"))?,
    );
    closed_ready_rx
        .recv_timeout(METHOD_TIMEOUT)
        .map_err(|_| "GNOME session subscription timed out".to_string())??;
    active.store(true, Ordering::Release);
    input
        .call_method("Start", &())
        .map_err(|e| format!("GNOME remote desktop start: {e}"))?;
    let node_id = node_rx
        .recv_timeout(STREAM_TIMEOUT)
        .map_err(|_| "GNOME did not expose a PipeWire node within 10 seconds".to_string())??;
    if !active.load(Ordering::Acquire) {
        return Err("GNOME closed the remote desktop session during startup".into());
    }
    let control = Arc::new(Control {
        conn,
        remote,
        stream,
        active,
        size: AtomicU64::new(pack_size(monitor.width, monitor.height)),
    });
    Ok(NativeCapture {
        owner,
        control,
        node_id,
    })
}

struct Control {
    conn: Connection,
    remote: OwnedObjectPath,
    stream: OwnedObjectPath,
    active: Arc<AtomicBool>,
    size: AtomicU64,
}

impl Control {
    fn call<B: serde::Serialize + DynamicType>(
        &self,
        method: &str,
        body: &B,
    ) -> Result<(), String> {
        if !self.is_active() {
            return Err("GNOME remote desktop session has closed".into());
        }
        let proxy = Proxy::new(&self.conn, REMOTE, self.remote.clone(), REMOTE_SESSION)
            .map_err(|e| e.to_string())?;
        // A rejected input event is not proof that the whole session died.
        // Keep release calls possible after transient errors; otherwise an
        // input-route rejection can strand its held keys while video stays up.
        // Only the compositor's Closed signal and capture owner retire it.
        proxy
            .call_method(method, body)
            .map(|_| ())
            .map_err(|e| format!("GNOME input {method}: {e}"))
    }
}

fn normalized_point(x: f64, y: f64, width: u32, height: u32) -> Result<(f64, f64), String> {
    if !x.is_finite() || !y.is_finite() || width == 0 || height == 0 {
        return Err("invalid GNOME pointer coordinates".into());
    }
    Ok((
        x.clamp(0.0, 1.0) * f64::from(width - 1),
        y.clamp(0.0, 1.0) * f64::from(height - 1),
    ))
}

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

impl DesktopControl for Control {
    fn absolute(&self, x: f64, y: f64) -> Result<(), String> {
        let size = self.size.load(Ordering::Acquire);
        let (x, y) = normalized_point(x, y, (size >> 32) as u32, size as u32)?;
        // Mutter transforms captured pixels to compositor coordinates, including
        // the monitor's scale and origin. Parameters["size"] is logical size.
        self.call("NotifyPointerMotionAbsolute", &(self.stream.as_str(), x, y))
    }

    fn relative(&self, dx: f64, dy: f64) -> Result<(), String> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err("invalid GNOME relative pointer coordinates".into());
        }
        self.call("NotifyPointerMotionRelative", &(dx, dy))
    }

    fn button(&self, button: i32, down: bool) -> Result<(), String> {
        self.call("NotifyPointerButton", &(button, down))
    }

    fn key(&self, keysym: u32, down: bool) -> Result<(), String> {
        self.call("NotifyKeyboardKeysym", &(keysym, down))
    }

    fn wheel(&self, dx: f64, dy: f64) -> Result<(), String> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err("invalid GNOME scroll coordinates".into());
        }
        self.call("NotifyPointerAxis", &(dx, dy, 3u32))
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(connector: &str, primary: bool) -> Monitor {
        Monitor {
            id: connector_id(connector),
            connector: connector.into(),
            label: connector.into(),
            primary,
            width: 3840,
            height: 2160,
        }
    }

    #[test]
    fn explicit_output_never_falls_back_after_unplug() {
        let monitors = vec![monitor("eDP-1", true), monitor("DP-1", false)];
        assert_eq!(select_monitor(&monitors, None).unwrap().connector, "eDP-1");
        assert_eq!(
            select_monitor(&monitors, Some(connector_id("DP-1")))
                .unwrap()
                .connector,
            "DP-1"
        );
        assert!(select_monitor(&monitors[..1], Some(connector_id("DP-1"))).is_err());
    }

    #[test]
    fn connector_id_is_fixed_and_enumeration_order_independent() {
        let a = monitor("DP-1", false);
        let b = monitor("eDP-1", true);
        assert_eq!(a.id, 0x9dba_d777);
        assert_ne!(a.id, b.id);
        assert_eq!(
            select_monitor(&[b.clone(), a.clone()], Some(a.id)).unwrap(),
            &a
        );
        assert_eq!(select_monitor(&[a.clone(), b], Some(a.id)).unwrap(), &a);
    }

    #[test]
    fn connector_hash_collision_is_rejected() {
        let a = monitor("DP-1", false);
        let mut b = monitor("eDP-1", true);
        b.id = a.id;
        assert!(validate_ids(&[a, b]).unwrap_err().contains("collide"));
    }

    #[test]
    fn pointer_uses_pixels_and_stays_inside_the_captured_output() {
        assert_eq!(
            normalized_point(0.5, 0.5, 3840, 2160).unwrap(),
            (1919.5, 1079.5)
        );
        assert_eq!(
            normalized_point(-1.0, 2.0, 3840, 2160).unwrap(),
            (0.0, 2159.0)
        );
        assert!(normalized_point(f64::NAN, 0.0, 3840, 2160).is_err());
        assert!(normalized_point(0.0, f64::INFINITY, 3840, 2160).is_err());
        assert!(normalized_point(0.0, 0.0, 0, 2160).is_err());
    }

    #[test]
    fn rotation_changes_capture_geometry_and_rejects_invalid_modes() {
        assert_eq!(oriented_size(3840, 2160, 1).unwrap(), (2160, 3840));
        assert_eq!(oriented_size(3840, 2160, 6).unwrap(), (3840, 2160));
        assert!(oriented_size(3840, 2160, 8).is_err());
        assert!(oriented_size(-1, 2160, 0).is_err());
    }

    fn display_state() -> DisplayState {
        let spec = (
            "DP-1".into(),
            "Vendor".into(),
            "Panel".into(),
            "Serial".into(),
        );
        let mode = (
            "3840x2160".into(),
            3840,
            2160,
            60.0,
            2.0,
            vec![1.0, 2.0],
            HashMap::from([("is-current".into(), OwnedValue::from(true))]),
        );
        (
            1,
            vec![(spec.clone(), vec![mode], HashMap::new())],
            vec![(0, 0, 2.0, 0, true, vec![spec], HashMap::new())],
            HashMap::new(),
        )
    }

    #[test]
    fn display_config_mapping_uses_current_physical_mode_not_logical_scale() {
        let (_, physical, mut logical, _) = display_state();
        let monitors = monitors_from_state(&physical, &logical).unwrap();
        assert_eq!((monitors[0].width, monitors[0].height), (3840, 2160));
        logical[0].3 = 1;
        let rotated = monitors_from_state(&physical, &logical).unwrap();
        assert_eq!((rotated[0].width, rotated[0].height), (2160, 3840));
        logical[0].5[0].0 = "missing-output".into();
        assert!(monitors_from_state(&physical, &logical).is_err());
    }

    // These mocks run the real zbus wire protocol against a private bus. They
    // catch tuple signatures, subscription ordering, shared-client ownership,
    // and resource lifetime mistakes that a helper-only test cannot exercise.
    const TEST_REMOTE: &str = "/org/gnome/Mutter/RemoteDesktop/Session/test";
    const TEST_CAST: &str = "/org/gnome/Mutter/ScreenCast/Session/test";
    const TEST_STREAM: &str = "/org/gnome/Mutter/ScreenCast/Stream/test";

    #[derive(Default)]
    struct MockState {
        events: std::sync::Mutex<Vec<String>>,
        peer: std::sync::Mutex<Option<String>>,
        fail_start: AtomicBool,
        fail_key_up: AtomicBool,
    }

    impl MockState {
        fn record(&self, event: impl Into<String>) {
            self.events.lock().unwrap().push(event.into());
        }

        fn check_peer(&self, header: &zbus::message::Header<'_>) -> zbus::fdo::Result<()> {
            let sender = header.sender().unwrap().to_string();
            let mut peer = self.peer.lock().unwrap();
            if peer.as_ref().is_some_and(|peer| peer != &sender) {
                return Err(zbus::fdo::Error::AccessDenied(
                    "different session client".into(),
                ));
            }
            *peer = Some(sender);
            Ok(())
        }
    }

    struct MockRemoteManager(Arc<MockState>);

    #[zbus::interface(name = "org.gnome.Mutter.RemoteDesktop")]
    impl MockRemoteManager {
        fn create_session(
            &self,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.check_peer(&header)?;
            self.0.record("CreateRemote");
            Ok(TEST_REMOTE.try_into().unwrap())
        }
    }

    struct MockCastManager(Arc<MockState>);

    #[zbus::interface(name = "org.gnome.Mutter.ScreenCast")]
    impl MockCastManager {
        fn create_session(
            &self,
            properties: Properties,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.check_peer(&header)?;
            assert_eq!(
                <&str>::try_from(properties.get("remote-desktop-session-id").unwrap()).unwrap(),
                "native-session-id"
            );
            self.0.record("CreateCast");
            Ok(TEST_CAST.try_into().unwrap())
        }
    }

    struct MockCastSession(Arc<MockState>);

    #[zbus::interface(name = "org.gnome.Mutter.ScreenCast.Session")]
    impl MockCastSession {
        fn record_monitor(
            &self,
            connector: &str,
            properties: Properties,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.check_peer(&header)?;
            assert_eq!(
                u32::try_from(properties.get("cursor-mode").unwrap()).unwrap(),
                1
            );
            assert_eq!(connector, "DP-1");
            self.0.record(format!("Record:{connector}"));
            Ok(TEST_STREAM.try_into().unwrap())
        }
    }

    struct MockStream;

    #[zbus::interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
    impl MockStream {
        #[zbus(property)]
        fn parameters(&self) -> Properties {
            // Deliberately logical dimensions. Input must use captured pixels.
            HashMap::from([(
                "size".into(),
                OwnedValue::try_from(Value::from((1920i32, 1080i32))).unwrap(),
            )])
        }
    }

    struct MockDisplay;

    #[zbus::interface(name = "org.gnome.Mutter.DisplayConfig")]
    impl MockDisplay {
        fn get_current_state(&self) -> DisplayState {
            display_state()
        }
    }

    struct MockRemoteSession(Arc<MockState>);

    #[zbus::interface(name = "org.gnome.Mutter.RemoteDesktop.Session")]
    impl MockRemoteSession {
        #[zbus(property)]
        fn session_id(&self) -> &str {
            "native-session-id"
        }

        async fn start(
            &self,
            #[zbus(connection)] conn: &zbus::Connection,
        ) -> zbus::fdo::Result<()> {
            self.0.record("Start");
            if self.0.fail_start.load(Ordering::Acquire) {
                return Err(zbus::fdo::Error::AccessDenied("session inhibited".into()));
            }
            conn.emit_signal(
                None::<&str>,
                TEST_STREAM,
                CAST_STREAM,
                "PipeWireStreamAdded",
                &77u32,
            )
            .await?;
            Ok(())
        }

        async fn stop(&self, #[zbus(connection)] conn: &zbus::Connection) -> zbus::fdo::Result<()> {
            self.0.record("Stop");
            conn.emit_signal(None::<&str>, TEST_REMOTE, REMOTE_SESSION, "Closed", &())
                .await?;
            Ok(())
        }

        fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) {
            assert_eq!(stream, TEST_STREAM);
            self.0.record(format!("Absolute:{x:.1},{y:.1}"));
        }

        fn notify_pointer_motion_relative(&self, dx: f64, dy: f64) {
            self.0.record(format!("Relative:{dx:.1},{dy:.1}"));
        }

        fn notify_pointer_button(&self, button: i32, down: bool) {
            self.0.record(format!("Button:{button},{down}"));
        }

        fn notify_keyboard_keysym(&self, keysym: u32, down: bool) -> zbus::fdo::Result<()> {
            if !down && self.0.fail_key_up.swap(false, Ordering::AcqRel) {
                self.0.record(format!("KeyFailed:{keysym},{down}"));
                return Err(zbus::fdo::Error::Failed(
                    "transient key release failure".into(),
                ));
            }
            self.0.record(format!("Key:{keysym},{down}"));
            Ok(())
        }

        fn notify_pointer_axis(&self, dx: f64, dy: f64, flags: u32) {
            self.0.record(format!("Wheel:{dx:.1},{dy:.1},{flags}"));
        }
    }

    struct PrivateBus {
        daemon: std::process::Child,
        address: String,
    }

    impl PrivateBus {
        fn start() -> Option<Self> {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let daemon = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut daemon = match daemon {
                Ok(daemon) => daemon,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("dbus-daemon unavailable; skipping private-bus integration test");
                    return None;
                }
                Err(error) => panic!("start private D-Bus daemon: {error}"),
            };
            let mut address = String::new();
            std::io::BufReader::new(daemon.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            let bus = Self {
                daemon,
                address: address.trim().into(),
            };
            assert!(
                !bus.address.is_empty(),
                "private D-Bus daemon exited before printing its address"
            );
            Some(bus)
        }

        fn connect(&self) -> Connection {
            Builder::address(self.address.as_str())
                .unwrap()
                .method_timeout(METHOD_TIMEOUT)
                .build()
                .unwrap()
        }

        fn serve(&self, state: &Arc<MockState>) -> Connection {
            Builder::address(self.address.as_str())
                .unwrap()
                .name(REMOTE)
                .unwrap()
                .name(CAST)
                .unwrap()
                .name(DISPLAY)
                .unwrap()
                .serve_at(REMOTE_PATH, MockRemoteManager(state.clone()))
                .unwrap()
                .serve_at(CAST_PATH, MockCastManager(state.clone()))
                .unwrap()
                .serve_at(DISPLAY_PATH, MockDisplay)
                .unwrap()
                .serve_at(TEST_REMOTE, MockRemoteSession(state.clone()))
                .unwrap()
                .serve_at(TEST_CAST, MockCastSession(state.clone()))
                .unwrap()
                .serve_at(TEST_STREAM, MockStream)
                .unwrap()
                .build()
                .unwrap()
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    #[test]
    fn native_dbus_handshake_input_and_capture_lifetime() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let state = Arc::new(MockState::default());
        let _server = bus.serve(&state);
        let capture = open_on(bus.connect(), Some(connector_id("DP-1"))).unwrap();
        assert_eq!(capture.node_id(), 77);
        let control = capture.control();
        assert!(control.is_active());
        control.absolute(0.5, 0.5).unwrap();
        capture.set_size(2560, 1440);
        control.absolute(1.0, 1.0).unwrap();
        control.relative(3.0, -2.0).unwrap();
        control.button(272, true).unwrap();
        control.key(65, true).unwrap();
        control.wheel(0.0, 10.0).unwrap();
        drop(capture);
        assert!(
            !control.is_active(),
            "input Arc must not retain capture permissions"
        );
        assert!(control.key(65, false).is_err());
        assert_eq!(
            *state.events.lock().unwrap(),
            [
                "CreateRemote",
                "CreateCast",
                "Record:DP-1",
                "Start",
                "Absolute:1919.5,1079.5",
                "Absolute:2559.0,1439.0",
                "Relative:3.0,-2.0",
                "Button:272,true",
                "Key:65,true",
                "Wheel:0.0,10.0,3",
                "Stop"
            ]
        );
    }

    #[test]
    fn failed_native_start_stops_and_releases_the_paired_session() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let state = Arc::new(MockState::default());
        state.fail_start.store(true, Ordering::Release);
        let _server = bus.serve(&state);
        let error = match open_on(bus.connect(), None) {
            Ok(_) => panic!("inhibited session unexpectedly started"),
            Err(error) => error,
        };
        assert!(error.contains("session inhibited"));
        assert_eq!(
            *state.events.lock().unwrap(),
            ["CreateRemote", "CreateCast", "Record:DP-1", "Start", "Stop"]
        );
    }

    #[test]
    fn transient_input_failure_preserves_authority_to_release_held_keys() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let state = Arc::new(MockState::default());
        let _server = bus.serve(&state);
        let capture = open_on(bus.connect(), None).unwrap();
        let control = capture.control();
        control.key(65, true).unwrap();
        state.fail_key_up.store(true, Ordering::Release);
        assert!(control.key(65, false).is_err());
        assert!(
            control.is_active(),
            "a failed event must not block teardown releases"
        );
        control.key(65, false).unwrap();
        drop(capture);
        assert_eq!(
            *state.events.lock().unwrap(),
            [
                "CreateRemote",
                "CreateCast",
                "Record:DP-1",
                "Start",
                "Key:65,true",
                "KeyFailed:65,false",
                "Key:65,false",
                "Stop"
            ]
        );
    }
}
