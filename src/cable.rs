//! The tablet over the USB cable (PROTOCOL.md §4, Android Open Accessory):
//! this PC is the USB host, the tablet its accessory, one bulk stream each
//! way. The link's TLS connections ride in it as numbered streams (no
//! loopback socket in between) and the display's sealed datagrams as frames
//! of their own. The cable is trusted with nothing: TLS and AES-GCM run end
//! to end.

use crate::link::Result;
use rusb::{Context, DeviceHandle, Direction, TransferType, UsbContext};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HELLO: u8 = 0;
pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
pub const CLOSE: u8 = 3;
pub const DATAGRAM: u8 = 4;
const HEADER: usize = 6;
const READ: usize = 16 * 1024;     // the app's transfer size
const BATCH: usize = 48 * 1024;    // the most the app takes in one transfer
const MAX_FRAME: usize = 1 << 20;
const IDENTITY: [&str; 6] = ["Screenlet", "Screenlet", "Screenlet on the PC", "1",
                             "https://github.com/DavideLoconte/Screenlet-Companion", ""];

const GOOGLE: u16 = 0x18D1;
/// Android tablets' makers: Samsung, Google, Lenovo, Xiaomi, OnePlus, Motorola,
/// Huawei, OPPO/realme, Honor, Amazon, Asus, Sony, TCL, Nothing. Then the chips' makers,
/// whose ids smaller brands keep (a Bigme is MediaTek's): MediaTek, Qualcomm, Unisoc, Rockchip.
pub const ANDROID_VENDORS: [u16; 18] = [0x04E8, 0x18D1, 0x17EF, 0x2717, 0x2A70, 0x22B8, 0x12D1, 0x22D9,
                                        0x339B, 0x1949, 0x0B05, 0x0FCE, 0x1BBB, 0x2B0E,
                                        0x0E8D, 0x05C6, 0x1782, 0x2207];

fn is_accessory(vendor: u16, product: u16) -> bool {
    vendor == GOOGLE && (0x2D00..=0x2D05).contains(&product)
}

/// A frame: kind | connection | length u32 | data.
pub fn frame(kind: u8, connection: u8, data: &[u8], out: &mut Vec<u8>) {
    out.push(kind);
    out.push(connection);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// The whole frames at the start of buffer, removed from it.
pub fn frames(buffer: &mut Vec<u8>) -> Result<Vec<(u8, u8, Vec<u8>)>> {
    let mut found = Vec::new();
    let mut at = 0;
    while buffer.len() - at >= HEADER {
        let length = u32::from_be_bytes(buffer[at + 2..at + 6].try_into()?) as usize;
        if length > MAX_FRAME {
            return Err(format!("the tablet sent a frame of {length} bytes").into());
        }
        if buffer.len() - at < HEADER + length {
            break;
        }
        found.push((buffer[at], buffer[at + 1], buffer[at + HEADER..at + HEADER + length].to_vec()));
        at += HEADER + length;
    }
    buffer.drain(..at);
    Ok(found)
}

/// Whether device may be an Android tablet: one of those makers, and none of
/// a laptop's own parts (the same makers' keyboards, cameras, hubs, Bluetooth).
fn android<T: UsbContext>(device: &rusb::Device<T>) -> bool {
    let Ok(dd) = device.device_descriptor() else { return false };
    if !ANDROID_VENDORS.contains(&dd.vendor_id()) || is_accessory(dd.vendor_id(), dd.product_id()) {
        return false;
    }
    const NOT_A_TABLET: [u8; 5] = [0x01, 0x03, 0x09, 0x0E, 0xE0]; // audio, HID, hub, video, wireless
    let Ok(config) = device.config_descriptor(0) else { return false };
    !config.interfaces().flat_map(|i| i.descriptors().collect::<Vec<_>>())
        .any(|d| NOT_A_TABLET.contains(&d.class_code()))
}

/// A tablet on the cable, whatever it shows itself as for now.
#[derive(Clone, PartialEq, Debug)]
pub struct Plug {
    pub serial: String, // the same as an accessory
    pub name: String,
}

/// A file of the device's in sysfs: readable without opening it, by anyone.
fn attribute<T: UsbContext>(device: &rusb::Device<T>, name: &str) -> Option<String> {
    let ports: Vec<String> = device.port_numbers().ok()?.iter().map(u8::to_string).collect();
    let text = std::fs::read_to_string(format!("/sys/bus/usb/devices/{}-{}/{name}", device.bus_number(), ports.join("."))).ok()?;
    Some(text.trim().to_string())
}

/// Which tablet it is: its serial, or with none the port it is on.
fn serial<T: UsbContext>(device: &rusb::Device<T>) -> String {
    attribute(device, "serial").unwrap_or_else(|| format!("{}-{}", device.bus_number(), device.port_number()))
}

/// The Android tablets plugged in, and those already in accessory mode.
pub fn plugged() -> Vec<Plug> {
    let Ok(list) = rusb::devices() else { return Vec::new() };
    let tablets = list.iter().filter(|d| android(d) || d.device_descriptor()
        .map(|dd| is_accessory(dd.vendor_id(), dd.product_id())).unwrap_or(false));
    let mut plugs: Vec<Plug> = tablets.map(|d| Plug { serial: serial(&d), name: attribute(&d, "product").unwrap_or_default() }).collect();
    plugs.sort_by(|a, b| (&a.name, &a.serial).cmp(&(&b.name, &b.serial)));
    plugs
}

/// What the window calls a tablet on the cable: with two of a kind, a bit of their serials too.
pub fn shown(plug: &Plug, plugs: &[Plug]) -> String {
    let name = if plug.name.is_empty() { "Tablet on the cable" } else { &plug.name };
    if plugs.iter().filter(|p| p.name == plug.name).count() < 2 { return name.to_string(); }
    format!("{name} ({})", &plug.serial[plug.serial.len().saturating_sub(4)..])
}

pub struct Cable {
    pub serial: String,
    handle: DeviceHandle<Context>,
    out: u8,
    packet: usize,
    writing: Mutex<()>,
    streams: Mutex<HashMap<u8, Sender<Vec<u8>>>>,
    next: Mutex<u8>,
    running: AtomicBool,
    /// The display's datagrams from the tablet, as they arrive (the reading thread).
    pub on_datagram: Mutex<Option<Box<dyn FnMut(&[u8]) + Send>>>,
}

impl Cable {
    /// Switches this tablet (none: the first on the cable) to accessory mode, opens it and
    /// waits for the app's HELLO (the first time, Android asks its user). Blocks.
    pub fn open(chosen: Option<&str>) -> Result<Arc<Cable>> {
        let wanted = match chosen {
            Some(wanted) => wanted.to_string(),
            None => plugged().into_iter().next().ok_or("no tablet on the cable")?.serial,
        };
        let context = Context::new()?;
        let accessory = |context: &Context| context.devices().ok()?.iter().find(|d| serial(d) == wanted && d.device_descriptor()
            .map(|dd| is_accessory(dd.vendor_id(), dd.product_id())).unwrap_or(false));
        if let Some(device) = accessory(&context) {
            // Still an accessory from an earlier session, maybe mid-frame: a reset starts both ends clean.
            if let Ok(handle) = device.open() {
                let _ = handle.reset();
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if accessory(&context).is_none() {
            let device = context.devices()?.iter().find(|d| android(d) && serial(d) == wanted).ok_or("no tablet on the cable")?;
            let handle = device.open().map_err(|e| format!("the tablet's USB: {e} (is the udev rule installed?)"))?;
            let timeout = Duration::from_secs(2);
            let mut version = [0u8; 2];
            handle.read_control(0xC0, 51, 0, 0, &mut version, timeout)?;
            if u16::from_le_bytes(version) == 0 {
                return Err("the tablet cannot be an accessory (Android Open Accessory)".into());
            }
            for (i, text) in IDENTITY.iter().enumerate() {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                handle.write_control(0x40, 52, 0, i as u16, &bytes, timeout)?;
            }
            handle.write_control(0x40, 53, 0, 0, &[], timeout)?;
            eprintln!("usb: asked the tablet to be Screenlet's accessory");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let device = loop {
            if let Some(device) = accessory(&context) {
                break device;
            }
            if Instant::now() > deadline {
                return Err("the tablet did not come back as an accessory".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let handle = device.open().map_err(|e| format!("the accessory: {e} (is the udev rule installed?)"))?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(0)?;
        let config = device.active_config_descriptor()?;
        let (mut input, mut out, mut packet) = (0, 0, 512);
        for interface in config.interfaces().filter(|i| i.number() == 0) {
            for setting in interface.descriptors() {
                for ep in setting.endpoint_descriptors().filter(|e| e.transfer_type() == TransferType::Bulk) {
                    match ep.direction() {
                        Direction::In => input = ep.address(),
                        Direction::Out => {
                            out = ep.address();
                            packet = ep.max_packet_size() as usize;
                        }
                    }
                }
            }
        }
        if input == 0 || out == 0 {
            return Err("the accessory has no bulk endpoints".into());
        }
        let cable = Arc::new(Cable {
            serial: wanted, handle, out, packet, writing: Mutex::new(()), streams: Mutex::new(HashMap::new()), next: Mutex::new(0),
            running: AtomicBool::new(true), on_datagram: Mutex::new(None),
        });
        // HELLO until the app reads it (Android asks its user the first time), then its answer.
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut hello = Vec::new();
        frame(HELLO, 0, &[], &mut hello);
        while cable.write_timeout(&hello, Duration::from_secs(1)).is_err() {
            if Instant::now() > deadline {
                return Err("Screenlet did not open on the tablet (allow it there)".into());
            }
        }
        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; READ];
        'answer: loop {
            if Instant::now() > deadline {
                return Err("Screenlet's app did not answer on the cable".into());
            }
            match cable.handle.read_bulk(input, &mut chunk, Duration::from_secs(1)) {
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                Err(rusb::Error::Timeout) => continue,
                Err(e) => return Err(e.into()),
            }
            for (kind, _, _) in frames(&mut buffer)? {
                if kind == HELLO {
                    break 'answer;
                }
            }
        }
        eprintln!("usb: Screenlet's app answers on the cable");
        let reader = cable.clone();
        std::thread::Builder::new().name("screenlet-usb-in".into()).spawn(move || reader.read_loop(input, buffer))?;
        Ok(cable)
    }

    fn write_timeout(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let _held = self.writing.lock().unwrap();
        let mut at = 0;
        while at < data.len() {
            at += self.handle.write_bulk(self.out, &data[at..], timeout)?;
        }
        // A transfer ends on a short packet: one that ends on a full packet needs an empty one.
        if data.len().is_multiple_of(self.packet) {
            self.handle.write_bulk(self.out, &[], timeout)?;
        }
        Ok(())
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.write_timeout(data, Duration::from_secs(5)).inspect_err(|_| self.close())
    }

    fn read_loop(self: Arc<Self>, input: u8, mut buffer: Vec<u8>) {
        let mut chunk = vec![0u8; READ];
        while self.running.load(Ordering::Relaxed) {
            match self.handle.read_bulk(input, &mut chunk, Duration::from_millis(500)) {
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                Err(rusb::Error::Timeout) => continue,
                Err(e) => {
                    eprintln!("usb: the cable's link is lost: {e}");
                    break;
                }
            }
            let Ok(found) = frames(&mut buffer) else { break };
            for (kind, connection, data) in found {
                match kind {
                    DATAGRAM => {
                        if let Some(callback) = self.on_datagram.lock().unwrap().as_mut() {
                            callback(&data);
                        }
                    }
                    DATA => {
                        if let Some(stream) = self.streams.lock().unwrap().get(&connection) {
                            let _ = stream.send(data);
                        }
                    }
                    CLOSE => {
                        self.streams.lock().unwrap().remove(&connection);
                    }
                    _ => {}
                }
            }
        }
        self.close();
    }

    /// A new stream to the app, for a TLS link.
    pub fn connect(self: &Arc<Self>) -> Result<CableStream> {
        let id = {
            let mut next = self.next.lock().unwrap();
            *next = *next % 255 + 1;
            *next
        };
        let (tx, rx) = channel();
        self.streams.lock().unwrap().insert(id, tx);
        let mut open = Vec::new();
        frame(OPEN, id, &[], &mut open);
        self.write(&open)?;
        Ok(CableStream { cable: self.clone(), id, rx, pending: Vec::new(), at: 0 })
    }

    /// One video frame's datagrams in as few transfers as the app takes.
    pub fn send_datagrams<'a>(&self, datagrams: impl Iterator<Item = &'a [u8]>, batch: &mut Vec<u8>) -> Result<()> {
        batch.clear();
        for datagram in datagrams {
            if !batch.is_empty() && batch.len() + HEADER + datagram.len() > BATCH {
                self.write(batch)?;
                batch.clear();
            }
            frame(DATAGRAM, 0, datagram, batch);
        }
        if !batch.is_empty() {
            self.write(batch)?;
        }
        Ok(())
    }

    pub fn alive(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if self.running.swap(false, Ordering::Relaxed) {
            self.streams.lock().unwrap().clear();
            let _ = self.handle.release_interface(0);
        }
    }
}

/// A TLS connection's bytes over the cable. Reads time out (WouldBlock) after
/// a second, so the link can ping meanwhile.
pub struct CableStream {
    cable: Arc<Cable>,
    id: u8,
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    at: usize,
}

impl Read for CableStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == self.pending.len() {
            match self.rx.recv_timeout(Duration::from_secs(1)) {
                Ok(data) => {
                    self.pending = data;
                    self.at = 0;
                }
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::WouldBlock.into()),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for CableStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut data = Vec::with_capacity(HEADER + buf.len());
        let n = buf.len().min(MAX_FRAME);
        frame(DATA, self.id, &buf[..n], &mut data);
        self.cable.write(&data).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for CableStream {
    fn drop(&mut self) {
        if self.cable.streams.lock().unwrap().remove(&self.id).is_some() {
            let mut close = Vec::new();
            frame(CLOSE, self.id, &[], &mut close);
            let _ = self.cable.write(&close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_matches_the_vector() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../vectors.json")).unwrap();
        let mut out = Vec::new();
        frame(DATAGRAM, 0, &crate::protocol::unhex(v["cable_frame"]["data"].as_str().unwrap()).unwrap(), &mut out);
        assert_eq!(crate::protocol::hex(&out), v["cable_frame"]["frame"].as_str().unwrap());
    }

    #[test]
    fn frames_across_reads() {
        let mut stream = Vec::new();
        frame(DATA, 3, &[1, 2, 3], &mut stream);
        frame(DATAGRAM, 0, &[9; 40], &mut stream);
        let mut buffer = stream[..5].to_vec();
        assert!(frames(&mut buffer).unwrap().is_empty());
        buffer.extend_from_slice(&stream[5..15]);
        assert_eq!(frames(&mut buffer).unwrap(), vec![(DATA, 3, vec![1, 2, 3])]);
        buffer.extend_from_slice(&stream[15..]);
        let second = frames(&mut buffer).unwrap();
        assert_eq!((second[0].0, second[0].2.len()), (DATAGRAM, 40));
        assert!(buffer.is_empty());
        let mut huge = vec![DATA, 1, 0x7f, 0xff, 0xff, 0xff];
        assert!(frames(&mut huge).is_err());
    }
}
