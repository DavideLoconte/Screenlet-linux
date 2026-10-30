//! Screenlet's pen with pressure and tilt, for the session in front of the
//! PC. As root (D-Bus activated on the system bus), one virtual HID tablet
//! per caller on /dev/uhid, with the S Pen's own descriptor, fed only the
//! pen's reports, which the caller sends through a socket it gets from Open().
//! Polkit says who may: the active local session.
//!
//! No /dev/uhid descriptor is handed out: its holder could destroy the pen and
//! make a keyboard. Through the socket only 10-byte pen reports pass; anything
//! else is dropped. The pen goes when the socket closes.
//!
//! --session: on the session bus, as the user, no polkit (to try it out: it
//! can then do only what the user could already).

#[path = "../uhid.rs"]
mod uhid;

use gio::prelude::*;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NAME: &str = "io.github.davideloconte.Screenlet.Pen";
const PATH: &str = "/io/github/davideloconte/Screenlet/Pen";
const ACTION: &str = "io.github.davideloconte.screenlet.pen";
const XML: &str = r#"<node><interface name="io.github.davideloconte.Screenlet.Pen">
  <method name="Open"><arg name="reports" type="h" direction="out"/></method>
</interface></node>"#;
const MAX_PENS: usize = 4; // at once, all callers together
const IDLE: Duration = Duration::from_secs(60); // without a pen before the helper exits (activated again on demand)

fn authorized(bus: &gio::DBusConnection, sender: &str) -> bool {
    let dict = glib::VariantDict::new(None);
    dict.insert_value("name", &sender.to_variant());
    let subject = glib::Variant::tuple_from_iter(["system-bus-name".to_variant(), dict.end()]);
    let details = glib::Variant::array_from_iter_with_type(glib::VariantTy::new("{ss}").unwrap(), Vec::<glib::Variant>::new());
    let args = glib::Variant::tuple_from_iter([subject, ACTION.to_variant(), details, 0u32.to_variant(), "".to_variant()]);
    bus.call_sync(Some("org.freedesktop.PolicyKit1"), "/org/freedesktop/PolicyKit1/Authority",
                  "org.freedesktop.PolicyKit1.Authority", "CheckAuthorization", Some(&args), None,
                  gio::DBusCallFlags::NONE, 10000, gio::Cancellable::NONE)
        .ok().and_then(|r| r.child_value(0).child_value(0).get::<bool>()).unwrap_or(false)
}

/// A pen: its uhid device, fed from its socket on a thread of its own.
fn open(pens: Arc<AtomicUsize>) -> std::io::Result<UnixDatagram> {
    let mut device = std::fs::OpenOptions::new().read(true).write(true).open("/dev/uhid")?;
    device.write_all(&uhid::create_event())?;
    let (ours, theirs) = UnixDatagram::pair()?;
    pens.fetch_add(1, Ordering::SeqCst);
    // The kernel's start and open notifications, read so its queue never fills.
    let mut drain = device.try_clone()?;
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4380];
        while drain.read(&mut buffer).map(|n| n > 0).unwrap_or(false) {}
    });
    std::thread::spawn(move || {
        let mut report = [0u8; 64];
        let mut event = Vec::with_capacity(16);
        while let Ok(n) = ours.recv(&mut report) {
            if n == 0 {
                break;
            }
            if n == 10 && report[0] == uhid::REPORT_ID { // the pen's, and only that
                uhid::input_event(&report[..n], &mut event);
                let _ = device.write_all(&event);
            }
        }
        let _ = device.write_all(&uhid::destroy_event());
        pens.fetch_sub(1, Ordering::SeqCst);
    });
    Ok(theirs)
}

fn main() {
    let session = std::env::args().any(|a| a == "--session");
    let bus = gio::bus_get_sync(if session { gio::BusType::Session } else { gio::BusType::System }, gio::Cancellable::NONE)
        .expect("the bus");
    let pens = Arc::new(AtomicUsize::new(0));
    let used = Arc::new(std::sync::Mutex::new(Instant::now()));
    let interface = gio::DBusNodeInfo::for_xml(XML).expect("the interface").lookup_interface(NAME).expect("the interface");
    let (p, u, b) = (pens.clone(), used.clone(), bus.clone());
    let _object = bus.register_object(PATH, &interface).method_call(move |_, sender, _, _, method, _, invocation| {
        *u.lock().unwrap() = Instant::now();
        if method != "Open" {
            return invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
        }
        if !session && !sender.is_some_and(|s| authorized(&b, s)) {
            return invocation.return_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "not the session in front of this PC");
        }
        if p.load(Ordering::SeqCst) >= MAX_PENS {
            return invocation.return_dbus_error("org.freedesktop.DBus.Error.LimitsExceeded", "too many pens");
        }
        match open(p.clone()) {
            Ok(theirs) => {
                let fds = gio::UnixFDList::new();
                let fd: OwnedFd = theirs.into();
                if fds.append(fd).is_err() {
                    return invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", "no descriptor");
                }
                invocation.return_value_with_unix_fd_list(Some(&(glib::variant::Handle(0),).to_variant()), Some(&fds));
            }
            Err(e) => invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", &e.to_string()),
        }
    }).build().expect("the object");
    let _owner = gio::bus_own_name_on_connection(&bus, NAME, gio::BusNameOwnerFlags::NONE, |_, _| {}, |_, _| std::process::exit(1));
    let main = glib::MainLoop::new(None, false);
    let m = main.clone();
    glib::timeout_add_seconds_local(10, move || {
        if pens.load(Ordering::SeqCst) == 0 && used.lock().unwrap().elapsed() > IDLE {
            m.quit();
        }
        glib::ControlFlow::Continue
    });
    main.run();
}
