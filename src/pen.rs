//! The tablet's pen as a real Linux pen tablet (pressure, tilt, hover): a
//! virtual HID device with the S Pen's descriptor on /dev/uhid, made by
//! Screenlet's pen helper (root, D-Bus activated, polkit: the session in
//! front of the PC) or here when this user may open /dev/uhid. GNOME then
//! maps it onto the tablet's monitor.

use crate::link::Result;
use gtk::gio::{self, prelude::*};
use gtk::glib::VariantTy;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;

pub const HELPER: &str = "io.github.davideloconte.Screenlet.Pen";
pub const HELPER_PATH: &str = "/io/github/davideloconte/Screenlet/Pen";
pub use crate::uhid::*;

pub enum Pen {
    /// The helper's socket: it checks each report and writes it to its uhid device.
    Helper(UnixDatagram),
    Uhid(std::sync::Mutex<std::fs::File>),
}

impl Pen {
    /// A pen tablet mapped onto the monitor with edid: through the helper, or
    /// /dev/uhid directly; None when neither (the pen then drives the pointer).
    pub fn open(edid: Option<&[String; 3]>) -> Option<Pen> {
        let edid = edid?;
        if let Err(e) = map_to_monitor(edid) {
            eprintln!("pen: not mapped to the tablet's monitor ({e}): the pointer instead");
            return None;
        }
        match helper() {
            Ok(pen) => return Some(pen),
            Err(e) => eprintln!("pen: no helper ({e})"),
        }
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open("/dev/uhid").ok()?;
        file.write_all(&create_event()).ok()?;
        eprintln!("pen: a pen tablet on /dev/uhid");
        Some(Pen::Uhid(std::sync::Mutex::new(file)))
    }

    /// One sample from INPUT (the report without its ID).
    pub fn report(&self, sample: &[u8]) {
        let mut report = [0u8; 10];
        report[0] = REPORT_ID;
        report[1..].copy_from_slice(&sample[..9]);
        match self {
            // Non-blocking: the helper behind, this one report goes, not the session.
            Pen::Helper(socket) => { let _ = socket.send(&report); }
            Pen::Uhid(file) => {
                let mut event = Vec::with_capacity(16);
                input_event(&report, &mut event);
                let _ = file.lock().unwrap().write_all(&event);
            }
        }
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        if let Pen::Uhid(file) = self {
            let _ = file.get_mut().unwrap().write_all(&destroy_event());
        }
    }
}

fn helper() -> Result<Pen> {
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)?;
    let (reply, fds) = bus.call_with_unix_fd_list_sync(Some(HELPER), HELPER_PATH, HELPER, "Open", None,
        Some(VariantTy::new("(h)")?), gio::DBusCallFlags::NONE, 10000, gio::UnixFDList::NONE, gio::Cancellable::NONE)?;
    let index = reply.child_value(0).get::<gtk::glib::variant::Handle>().ok_or("no handle")?.0;
    let fd = fds.ok_or("no fd")?.get(index)?;
    let socket = UnixDatagram::from(unsafe { OwnedFd::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(fd)) });
    socket.set_nonblocking(true)?;
    eprintln!("pen: a pen tablet made by the helper (pressure and tilt)");
    Ok(Pen::Helper(socket))
}

/// GNOME maps our tablet (VENDOR:PRODUCT) onto the monitor with this EDID.
fn map_to_monitor(edid: &[String; 3]) -> Result<()> {
    const SCHEMA: &str = "org.gnome.desktop.peripherals.tablet";
    let source = gio::SettingsSchemaSource::default().ok_or("no settings schemas")?;
    source.lookup(SCHEMA, true).ok_or("no tablet settings schema")?;
    let settings = gio::Settings::with_path(SCHEMA, &format!("/org/gnome/desktop/peripherals/tablets/{VENDOR:04x}:{PRODUCT:04x}/"));
    settings.set_strv("output", edid.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
    settings.set_boolean("keep-aspect", false)?;
    gio::Settings::sync();
    Ok(())
}

