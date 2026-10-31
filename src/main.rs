#![allow(clippy::type_complexity)] // closures and channels say what they carry

mod cable;
mod input;
mod link;
mod mutter;
mod nearby;
mod pen;
mod protocol;
mod session;
mod settings;
mod sizes;
mod ui;
mod uhid;
mod video;

use session::{Mode, Path, Session};
use std::time::{Duration, Instant};

const USAGE: &str = "screenlet [--background]           the window (or not yet: at login, from the toggle)
screenlet --show-cable SECONDS [OPTIONS]   the desktop on the tablet on the cable, for a while,
screenlet --show-wifi SECONDS [OPTIONS]    or on the first paired tablet on the network,
                                   with the latency logged stage by stage every 2 s
    options: mirror (the first monitor), h264 or hevc, latency, balanced or clarity,
             30, 60, 90 or 120 (Hz; the tablet's otherwise), native, WxH, small or large
             (packets), pattern (a test pattern instead of the desktop), serial=SERIAL
             (which tablet on the cable, as --tablets lists them; the first otherwise)
screenlet --tablets                the tablets on the cable
screenlet --pair-cable | --pair-wifi       pair, comparing the code here and on the tablet
screenlet --bench SECONDS [mirror]         this PC's part alone: the test pattern (or, with
                                   mirror, the first monitor through Mutter) encoded,
                                   sealed and sent to itself, its stages measured";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        None | Some("--background") => {
            ui::run(args.get(1).is_some());
            Ok(())
        }
        Some("--tablets") => {
            for plug in cable::plugged() {
                println!("{} on the cable, serial={}", plug.name, plug.serial);
            }
            Ok(())
        }
        Some("--show-cable") | Some("--show-wifi") => show(&args),
        Some("--pair-cable") | Some("--pair-wifi") => pair(args[1] == "--pair-cable"),
        Some("--bench") => bench(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10), args.get(3).map(String::as_str) == Some("mirror")),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("screenlet: {e}");
        std::process::exit(1);
    }
}

fn first_paired(within: Duration) -> link::Result<Path> {
    let found = nearby::browse();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(t) = found.lock().unwrap().values().find(|t| link::paired::by_id(&t.id).is_some()) {
            return Ok(Path::Network(t.address, Some(t.id.clone())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("no paired tablet on the network".into())
}

fn show(args: &[String]) -> link::Result<()> {
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut mode = Mode::default();
    let mut pattern = false;
    let mut serial = None;
    for word in args.iter().skip(3) {
        match word.as_str() {
            on if on.starts_with("serial=") => serial = Some(on["serial=".len()..].to_string()),
            "mirror" => mode.mirror = mutter::monitors()?.1.into_iter().find(|m| !m.connector.starts_with("Meta-")).map(|m| m.connector),
            "h264" => mode.codec = "avc".into(),
            "hevc" => mode.codec = "hevc".into(),
            "latency" | "balanced" | "clarity" => mode.priority = word.clone(),
            "30" | "60" | "90" | "120" => mode.fps = word.parse()?,
            "native" => mode.resolution = "tablet".into(),
            "small" | "large" => mode.packets = word.clone(),
            "pattern" => pattern = true,
            other => mode.resolution = other.into(),
        }
    }
    session::STATS_EVERY.store(2, std::sync::atomic::Ordering::Relaxed);
    let path = if args[1] == "--show-cable" { Path::Cable(cable::Cable::open(serial.as_deref())?) } else { first_paired(Duration::from_secs(10))? };
    let session = Session::start(path, mode, pattern, None);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !session.ended() {
        std::thread::sleep(Duration::from_millis(100));
    }
    session.stop();
    while !session.ended() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let ended = session.status.lock().unwrap().ended.clone().flatten();
    match ended {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn pair(over_cable: bool) -> link::Result<()> {
    let path = if over_cable { Path::Cable(cable::Cable::open(None)?) } else {
        let found = nearby::browse();
        std::thread::sleep(Duration::from_secs(3));
        let tablets = found.lock().unwrap();
        let t = tablets.values().next().ok_or("no tablet on the network")?;
        Path::Network(t.address, None)
    };
    let ask = |name: &str, code: &str| {
        println!("Pair with {name} only if it shows {} {}. Pair? [y/N]", &code[..3], &code[3..]);
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
    };
    println!("Paired with {}", session::pair(&path, &ask)?);
    Ok(())
}

/// This PC's stages alone, with no tablet: the test pattern at 1920x1200@60
/// through the encoder, packetized, sealed and sent to a socket here, where
/// they are opened and put together again. Reports each stage's median.
fn bench(seconds: u64, mirror: bool) -> link::Result<()> {
    use protocol::*;
    use std::sync::{Arc, Mutex};
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let to = socket.local_addr()?;
    let out = std::net::UdpSocket::bind("127.0.0.1:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let key = [7u8; 32];
    let converted: Arc<Mutex<Vec<f64>>> = Arc::default();
    let encoded: Arc<Mutex<Vec<f64>>> = Arc::default();
    let sent: Arc<Mutex<Vec<f64>>> = Arc::default();
    let (c, e, s) = (converted.clone(), encoded.clone(), sent.clone());
    let mut sealer = Sealer::new(&key, FROM_PC);
    let mut number = 0u32;
    let mut body = vec![0u8; VIDEO_HEADER + LARGE_PAYLOAD];
    let mut packet = vec![0u8; MAX_PACKET];
    let codec = video::codecs(false).remove(0);
    let (screencast, source, size) = if mirror {
        let monitor = mutter::monitors()?.1.into_iter().find(|m| !m.connector.starts_with("Meta-")).ok_or("no monitor")?;
        let screencast = mutter::Session::start(Some(&monitor.connector))?;
        let node = screencast.node;
        (Some(screencast), video::Source::PipeWire(node, None), sizes::fit(monitor.size, (1920, 1200)))
    } else {
        (None, video::Source::Pattern, (1920, 1200))
    };
    let video = video::Video::new(source, &codec, size, 60, 20000, 4.0, 10, Box::new(|| false),
        Box::new(move |bytes, keyframe, captured, into| {
            c.lock().unwrap().push((into - captured) as f64 / 1e6);
            e.lock().unwrap().push((video::now() - into) as f64 / 1e6);
            number += 1;
            let count = bytes.len().div_ceil(LARGE_PAYLOAD);
            for (i, chunk) in bytes.chunks(LARGE_PAYLOAD).enumerate() {
                body[..VIDEO_HEADER].copy_from_slice(&video_header(keyframe, number, i as u16, count as u16, captured));
                body[VIDEO_HEADER..VIDEO_HEADER + chunk.len()].copy_from_slice(chunk);
                let n = sealer.seal(&body[..VIDEO_HEADER + chunk.len()], &mut packet);
                let _ = out.send_to(&packet[..n], to);
            }
            s.lock().unwrap().push((video::now() - captured) as f64 / 1e6);
        }))?;
    println!("bench: {} ({codec}) at {}x{}@60 for {seconds} s{}", video.name, size.0, size.1,
             if mirror { ", the first monitor through Mutter" } else { "" });
    video.start()?;
    let mut opener = Opener::new(&key, FROM_PC);
    let (mut buffer, mut plain) = (vec![0u8; 65536], vec![0u8; 65536]);
    let mut whole = Vec::new(); // capture -> last packet opened here
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let Ok(n) = socket.recv(&mut buffer) else { continue };
        let Some(b) = opener.open(&buffer[..n], &mut plain) else { continue };
        let (index, count) = (u16::from_be_bytes([b[6], b[7]]), u16::from_be_bytes([b[8], b[9]]));
        if index + 1 == count {
            let captured = i64::from_be_bytes(b[10..18].try_into()?);
            whole.push((video::now() - captured) as f64 / 1e6);
        }
    }
    drop(video);
    drop(screencast);
    let median = |v: &mut Vec<f64>| { v.sort_by(f64::total_cmp); v.get(v.len() / 2).copied().unwrap_or(f64::NAN) };
    let p90 = |v: &Vec<f64>| v.get(v.len() * 9 / 10).copied().unwrap_or(f64::NAN);
    let (mut converted, mut encoded, mut sent) =
        (converted.lock().unwrap().clone(), encoded.lock().unwrap().clone(), sent.lock().unwrap().clone());
    println!("frames: {}", encoded.len());
    println!("capture -> into encoder:   {:.2} ms (90%: {:.2})", median(&mut converted), p90(&converted));
    println!("encoder:                   {:.2} ms (90%: {:.2})", median(&mut encoded), p90(&encoded));
    println!("capture -> sent:           {:.2} ms (90%: {:.2})", median(&mut sent), p90(&sent));
    println!("capture -> opened here:    {:.2} ms (90%: {:.2})", median(&mut whole), p90(&whole));
    Ok(())
}
