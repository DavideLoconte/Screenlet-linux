//! The screen and the input through Mutter's private D-Bus APIs: unlike the
//! portal they need no consent dialog, so the display appears as soon as the
//! tablet connects. A virtual monitor (extend) takes the size negotiated on
//! the PipeWire stream and goes when the session stops or this process exits;
//! or one of the PC's monitors is recorded as it is (mirror).

use crate::link::Result;
use gtk::gio::{self, prelude::*};
use gtk::glib::{self, Variant, VariantDict, VariantTy};
use std::time::{Duration, Instant};

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const SC: &str = "org.gnome.Mutter.ScreenCast";
const DC: &str = "org.gnome.Mutter.DisplayConfig";
pub const BTN_LEFT: i32 = 0x110;
pub const BTN_RIGHT: i32 = 0x111;

fn bus() -> Result<gio::DBusConnection> {
    Ok(gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?)
}

fn call(bus: &gio::DBusConnection, name: &str, path: &str, iface: &str, method: &str, args: Option<&Variant>,
        reply: Option<&VariantTy>) -> Result<Variant> {
    Ok(bus.call_sync(Some(name), path, iface, method, args, reply, gio::DBusCallFlags::NONE, 5000, gio::Cancellable::NONE)?)
}

fn options(entries: &[(&str, Variant)]) -> Variant {
    let dict = VariantDict::new(None);
    for (key, value) in entries {
        dict.insert_value(key, value);
    }
    Variant::tuple_from_iter([dict.end()])
}

/// A screen-cast session tied to a remote-desktop one (the input goes to it).
pub struct Session {
    bus: gio::DBusConnection,
    rd: String,
    stream: String,
    pub node: u32,
}

impl Session {
    /// Starts recording: a new virtual monitor, or the monitor on connector.
    /// Blocks until PipeWire has the stream.
    pub fn start(mirror: Option<&str>) -> Result<Session> {
        let bus = bus()?;
        // Its own main context: the stream's signal comes to this thread, not the window's.
        let context = glib::MainContext::new();
        let node = std::rc::Rc::new(std::cell::Cell::new(None));
        let session = context.with_thread_default(|| -> Result<Session> {
            let rd = call(&bus, RD, "/org/gnome/Mutter/RemoteDesktop", RD, "CreateSession", None, Some(VariantTy::new("(o)")?))?
                .child_value(0).str().ok_or("no remote desktop session")?.to_string();
            let id = call(&bus, RD, &rd, "org.freedesktop.DBus.Properties", "Get",
                          Some(&(format!("{RD}.Session"), "SessionId").to_variant()), Some(VariantTy::new("(v)")?))?
                .child_value(0).as_variant().and_then(|v| v.str().map(String::from)).ok_or("no session id")?;
            let sc = call(&bus, SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession",
                          Some(&options(&[("remote-desktop-session-id", id.to_variant())])), Some(VariantTy::new("(o)")?))?
                .child_value(0).str().ok_or("no screen cast session")?.to_string();
            // cursor-mode 1, embedded: the tablet has no cursor of its own.
            let stream = match mirror {
                Some(connector) => call(&bus, SC, &sc, &format!("{SC}.Session"), "RecordMonitor",
                                        Some(&Variant::tuple_from_iter([connector.to_variant(),
                                            options(&[("cursor-mode", 1u32.to_variant())]).child_value(0)])),
                                        Some(VariantTy::new("(o)")?))?,
                None => call(&bus, SC, &sc, &format!("{SC}.Session"), "RecordVirtual",
                             Some(&options(&[("cursor-mode", 1u32.to_variant()), ("is-platform", true.to_variant())])),
                             Some(VariantTy::new("(o)")?))?,
            }.child_value(0).str().ok_or("no stream")?.to_string();
            let found = node.clone();
            #[allow(deprecated)]
            let subscription = bus.signal_subscribe(Some(SC), Some(&format!("{SC}.Stream")), Some("PipeWireStreamAdded"),
                Some(&stream), None, gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, params| found.set(params.child_value(0).get::<u32>()));
            call(&bus, RD, &rd, &format!("{RD}.Session"), "Start", None, None)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while node.get().is_none() && Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(2));
            }
            #[allow(deprecated)]
            bus.signal_unsubscribe(subscription);
            let node = node.get().ok_or("Mutter gave no PipeWire stream")?;
            Ok(Session { bus: bus.clone(), rd, stream, node })
        })??;
        Ok(session)
    }

    /// An input event, sent without waiting for Mutter's answer: the receiving
    /// thread goes on to the next at once.
    fn notify(&self, method: &str, args: Variant) {
        let message = gio::DBusMessage::new_method_call(Some(RD), &self.rd, Some(&format!("{RD}.Session")), method);
        message.set_body(&args);
        message.set_flags(gio::DBusMessageFlags::NO_REPLY_EXPECTED);
        let _ = self.bus.send_message(&message, gio::DBusSendMessageFlags::NONE);
    }

    pub fn touch_down(&self, slot: u32, x: f64, y: f64) {
        self.notify("NotifyTouchDown", (self.stream.as_str(), slot, x, y).to_variant());
    }

    pub fn touch_motion(&self, slot: u32, x: f64, y: f64) {
        self.notify("NotifyTouchMotion", (self.stream.as_str(), slot, x, y).to_variant());
    }

    pub fn touch_up(&self, slot: u32) {
        self.notify("NotifyTouchUp", (slot,).to_variant());
    }

    pub fn pointer_move(&self, x: f64, y: f64) {
        self.notify("NotifyPointerMotionAbsolute", (self.stream.as_str(), x, y).to_variant());
    }

    pub fn pointer_button(&self, button: i32, pressed: bool) {
        self.notify("NotifyPointerButton", (button, pressed).to_variant());
    }

    pub fn stop(&self) {
        let _ = call(&self.bus, RD, &self.rd, &format!("{RD}.Session"), "Stop", None, None);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The PC's monitors: (connector, vendor, product, serial, display name, width, height, is virtual).
pub struct Monitor {
    pub connector: String,
    pub edid: [String; 3],
    pub name: String,
    pub size: (i32, i32),
    pub current_mode: String,
    pub scales: Vec<f64>,
}

pub fn monitors() -> Result<(u32, Vec<Monitor>, Variant)> {
    let state = call(&bus()?, DC, "/org/gnome/Mutter/DisplayConfig", DC, "GetCurrentState", None, None)?;
    let serial = state.child_value(0).get::<u32>().unwrap_or(0);
    let mut out = Vec::new();
    for monitor in state.child_value(1).iter() {
        let spec = monitor.child_value(0);
        let text = |i| spec.child_value(i).str().unwrap_or_default().to_string();
        let props = VariantDict::new(Some(&monitor.child_value(2)));
        let name = props.lookup::<String>("display-name").ok().flatten().unwrap_or_else(|| text(0));
        let mut size = (0, 0);
        let mut current_mode = String::new();
        let mut scales = Vec::new();
        for mode in monitor.child_value(1).iter() {
            let mode_props = VariantDict::new(Some(&mode.child_value(6)));
            if mode_props.lookup::<bool>("is-current").ok().flatten() == Some(true) {
                size = (mode.child_value(1).get().unwrap_or(0), mode.child_value(2).get().unwrap_or(0));
                current_mode = mode.child_value(0).str().unwrap_or_default().to_string();
                scales = mode.child_value(5).get::<Vec<f64>>().unwrap_or_default();
            }
        }
        out.push(Monitor { connector: text(0), edid: [text(1), text(2), text(3)], name, size, current_mode, scales });
    }
    Ok((serial, out, state.child_value(2)))
}

/// The newest virtual monitor (Mutter's are "Meta-N"): the tablet's.
pub fn virtual_monitor() -> Option<Monitor> {
    monitors().ok()?.1.into_iter().filter(|m| m.connector.starts_with("Meta-"))
        .max_by_key(|m| u64::from_str_radix(&m.edid[2], 16).unwrap_or(0))
}

/// The tablet's virtual monitor at scale (2: HiDPI), the others as they are,
/// as GNOME Settings would set it, temporarily: whether it could.
pub fn set_virtual_scale(scale: f64) -> Result<bool> {
    let (serial, monitors, logical) = monitors()?;
    let Some(target) = monitors.iter().filter(|m| m.connector.starts_with("Meta-"))
        .max_by_key(|m| u64::from_str_radix(&m.edid[2], 16).unwrap_or(0)) else { return Ok(false) };
    if !target.scales.iter().any(|s| (s - scale).abs() < 0.01) {
        return Ok(false);
    }
    let mut config = Vec::new();
    for l in logical.iter() {
        let connectors: Vec<String> = l.child_value(5).iter().map(|s| s.child_value(0).str().unwrap_or_default().to_string()).collect();
        let mut specs = Vec::new();
        for c in &connectors {
            let Some(m) = monitors.iter().find(|m| &m.connector == c) else { return Ok(false) };
            specs.push((c.clone(), m.current_mode.clone(), VariantDict::new(None).end()).to_variant());
        }
        let lscale = if connectors.contains(&target.connector) { scale } else { l.child_value(2).get::<f64>().unwrap_or(1.0) };
        config.push(Variant::tuple_from_iter([
            l.child_value(0), l.child_value(1), lscale.to_variant(), l.child_value(3), l.child_value(4),
            Variant::array_from_iter_with_type(VariantTy::new("(ssa{sv})")?, specs),
        ]));
    }
    let args = Variant::tuple_from_iter([
        serial.to_variant(), 1u32.to_variant(),
        Variant::array_from_iter_with_type(VariantTy::new("(iiduba(ssa{sv}))")?, config),
        VariantDict::new(None).end(),
    ]);
    call(&bus()?, DC, "/org/gnome/Mutter/DisplayConfig", DC, "ApplyMonitorsConfig", Some(&args), None)?;
    Ok(true)
}
