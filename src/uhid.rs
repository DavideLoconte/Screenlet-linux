//! The pen tablet's virtual HID device on /dev/uhid: its descriptor and the
//! kernel's events. Shared by the app (pen.rs) and the pen helper.

pub const VENDOR: u16 = 0x04E8; // GNOME maps a tablet by these
pub const PRODUCT: u16 = 0x5350;
pub const NAME: &str = "Screenlet Pen";
pub const REPORT_ID: u8 = 0x14;
const UHID_DESTROY: u32 = 1;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const BUS_VIRTUAL: u16 = 0x06;

/// Digitizer / Pen / Stylus: tip, barrel, invert, eraser and in-range bits,
/// X and Y 0..32767, pressure 0..4096, X and Y tilt -90..90 (report 0x14).
pub const S_PEN: &[u8] = &[
    0x05, 0x0d, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x14, 0x09, 0x20, 0xa1, 0x00, 0x09, 0x42, 0x09, 0x44, 0x09, 0x3c,
    0x09, 0x45, 0x09, 0x32, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81, 0x02, 0x95, 0x03, 0x81, 0x03,
    0x05, 0x01, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0xa4, 0x55, 0x0d, 0x65, 0x13, 0x35, 0x00, 0x46, 0x3a, 0x20,
    0x27, 0xff, 0x7f, 0x00, 0x00, 0x81, 0x02, 0x09, 0x31, 0x46, 0x2c, 0x18, 0x27, 0xff, 0x7f, 0x00, 0x00, 0x81,
    0x02, 0xb4, 0x05, 0x0d, 0x09, 0x30, 0x26, 0x00, 0x10, 0x81, 0x02, 0x75, 0x08, 0x09, 0x3d, 0x15, 0xa6, 0x25,
    0x5a, 0x81, 0x02, 0x09, 0x3e, 0x81, 0x02, 0xc0, 0xc0,
];

/// struct uhid_event { u32 type; struct uhid_create2_req } (packed).
pub fn create_event() -> Vec<u8> {
    let mut e = UHID_CREATE2.to_le_bytes().to_vec();
    let field = |text: &str, size: usize| { let mut f = text.as_bytes().to_vec(); f.resize(size, 0); f };
    e.extend(field(NAME, 128));
    e.extend(field("screenlet", 64));
    e.extend(field("", 64));
    e.extend((S_PEN.len() as u16).to_le_bytes());
    e.extend(BUS_VIRTUAL.to_le_bytes());
    e.extend((VENDOR as u32).to_le_bytes());
    e.extend((PRODUCT as u32).to_le_bytes());
    e.extend(1u32.to_le_bytes());
    e.extend(0u32.to_le_bytes());
    e.extend(S_PEN);
    e
}

/// struct uhid_event { u32 type; struct uhid_input2_req { u16 size; data } }.
pub fn input_event(report: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend(UHID_INPUT2.to_le_bytes());
    out.extend((report.len() as u16).to_le_bytes());
    out.extend(report);
}

pub fn destroy_event() -> [u8; 4] {
    UHID_DESTROY.to_le_bytes()
}

#[cfg(test)]
mod tests {
    #[test]
    fn create_event_is_the_kernels_struct() {
        let e = super::create_event();
        assert_eq!(e.len(), 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + super::S_PEN.len());
        assert_eq!(&e[..4], &11u32.to_le_bytes());
        assert_eq!(&e[4..17], b"Screenlet Pen");
    }
}
