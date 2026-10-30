//! The tablet's fingers and pen on the desktop. Fingers are real touches
//! (Mutter's NotifyTouch*: GNOME's own gestures, nothing decided here, so
//! nothing waits); the pen is a pen tablet with pressure and tilt (pen.rs),
//! or the pointer when this PC has no way to make one.

use crate::mutter::{Session, BTN_LEFT, BTN_RIGHT};
use crate::pen::Pen;
use crate::protocol::{Input as Packet, PEN_SAMPLE};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const SILENCE: Duration = Duration::from_millis(400); // the app repeats its state every 0.1 s

/// Where events go: Mutter, or a list (the tests).
pub trait Desktop: Send {
    fn touch_down(&self, slot: u32, x: f64, y: f64);
    fn touch_motion(&self, slot: u32, x: f64, y: f64);
    fn touch_up(&self, slot: u32);
    fn pointer_move(&self, x: f64, y: f64);
    fn pointer_button(&self, button: i32, pressed: bool);
}

impl Desktop for Arc<Session> {
    fn touch_down(&self, slot: u32, x: f64, y: f64) { Session::touch_down(self, slot, x, y) }
    fn touch_motion(&self, slot: u32, x: f64, y: f64) { Session::touch_motion(self, slot, x, y) }
    fn touch_up(&self, slot: u32) { Session::touch_up(self, slot) }
    fn pointer_move(&self, x: f64, y: f64) { Session::pointer_move(self, x, y) }
    fn pointer_button(&self, button: i32, pressed: bool) { Session::pointer_button(self, button, pressed) }
}

pub struct Input<D: Desktop> {
    desktop: D,
    size: (f64, f64),
    down: HashMap<u8, (f64, f64)>,
    pen: Arc<OnceLock<Pen>>,
    pointer_buttons: u8, // the pen as the pointer: tip 1, barrel 2
    heard: Instant,
}

impl<D: Desktop> Input<D> {
    /// pen: the pen tablet, once it is made (the pointer until then).
    pub fn new(desktop: D, size: (i32, i32), pen: Arc<OnceLock<Pen>>) -> Input<D> {
        Input { desktop, size: (size.0 as f64, size.1 as f64), down: HashMap::new(), pen, pointer_buttons: 0, heard: Instant::now() }
    }

    pub fn busy(&self) -> bool {
        !self.down.is_empty() || self.pointer_buttons != 0
    }

    /// An INPUT body; false if malformed.
    pub fn packet(&mut self, body: &[u8]) -> bool {
        let Some(input) = Packet::parse(body) else { return false };
        self.heard = Instant::now();
        let fingers: HashMap<u8, (f64, f64)> =
            input.fingers.iter().map(|&(id, x, y)| (id, (x * self.size.0, y * self.size.1))).collect();
        self.touch(&fingers);
        for sample in input.pen.as_chunks::<PEN_SAMPLE>().0 {
            match self.pen.get() {
                Some(pen) => pen.report(sample),
                None => self.pointer_pen(sample),
            }
        }
        true
    }

    fn touch(&mut self, fingers: &HashMap<u8, (f64, f64)>) {
        let lifted: Vec<u8> = self.down.keys().filter(|id| !fingers.contains_key(id)).copied().collect();
        for id in lifted {
            self.desktop.touch_up(id as u32);
            self.down.remove(&id);
        }
        for (&id, &at) in fingers {
            match self.down.insert(id, at) {
                None => self.desktop.touch_down(id as u32, at.0, at.1),
                Some(was) if was != at => self.desktop.touch_motion(id as u32, at.0, at.1),
                _ => {}
            }
        }
    }

    /// Without a pen tablet: hover moves the pointer, the tip is the left button, the barrel the right.
    fn pointer_pen(&mut self, sample: &[u8]) {
        let bits = sample[0];
        let near = bits & 0x10 != 0;
        if near {
            let x = u16::from_le_bytes([sample[1], sample[2]]) as f64 / 32767.0 * self.size.0;
            let y = u16::from_le_bytes([sample[3], sample[4]]) as f64 / 32767.0 * self.size.1;
            self.desktop.pointer_move(x, y);
        }
        for (mask, button) in [(1u8, BTN_LEFT), (2, BTN_RIGHT)] {
            let pressed = near && bits & mask != 0;
            if pressed != (self.pointer_buttons & mask != 0) {
                self.desktop.pointer_button(button, pressed);
                self.pointer_buttons ^= mask;
            }
        }
    }

    /// Every few ms: the tablet gone quiet, everything lifts.
    pub fn check(&mut self) {
        if self.heard.elapsed() >= SILENCE && self.busy() {
            self.close();
        }
    }

    pub fn close(&mut self) {
        self.touch(&HashMap::new());
        self.pointer_pen(&[0; PEN_SAMPLE]);
        if let Some(pen) = self.pen.get() {
            pen.report(&[0; PEN_SAMPLE]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default, Clone)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Desktop for Log {
        fn touch_down(&self, slot: u32, x: f64, y: f64) { self.0.lock().unwrap().push(format!("down {slot} {x:.0} {y:.0}")) }
        fn touch_motion(&self, slot: u32, x: f64, y: f64) { self.0.lock().unwrap().push(format!("motion {slot} {x:.0} {y:.0}")) }
        fn touch_up(&self, slot: u32) { self.0.lock().unwrap().push(format!("up {slot}")) }
        fn pointer_move(&self, x: f64, y: f64) { self.0.lock().unwrap().push(format!("move {x:.0} {y:.0}")) }
        fn pointer_button(&self, button: i32, pressed: bool) { self.0.lock().unwrap().push(format!("button {button:x} {pressed}")) }
    }

    fn fingers(list: &[(u8, u16, u16)]) -> Vec<u8> {
        let mut body = vec![crate::protocol::INPUT, list.len() as u8];
        for &(id, x, y) in list {
            body.push(id);
            body.extend(x.to_be_bytes());
            body.extend(y.to_be_bytes());
        }
        body.push(0);
        body
    }

    #[test]
    fn fingers_are_touches_at_once() {
        let log = Log::default();
        let mut input = Input::new(log.clone(), (1000, 1000), Arc::default());
        assert!(input.packet(&fingers(&[(1, 32768, 16384)])));
        assert!(input.packet(&fingers(&[(1, 32768, 16384), (2, 0, 0)])));
        assert!(input.packet(&fingers(&[(2, 65535, 0)])));
        assert!(input.packet(&fingers(&[])));
        assert_eq!(*log.0.lock().unwrap(), ["down 1 500 250", "down 2 0 0", "up 1", "motion 2 1000 0", "up 2"]);
        assert!(!input.packet(&[crate::protocol::INPUT, 11, 0]));
    }

    #[test]
    fn the_vectors_pen_drives_the_pointer_without_a_tablet() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../vectors.json")).unwrap();
        let body = crate::protocol::unhex(v["input"]["body"].as_str().unwrap()).unwrap();
        let log = Log::default();
        let mut input = Input::new(log.clone(), (1000, 1000), Arc::default());
        assert!(input.packet(&body));
        assert!(log.0.lock().unwrap().contains(&"button 110 true".to_string()));
        input.close();
        assert!(log.0.lock().unwrap().ends_with(&["button 110 false".to_string()]));
    }
}
