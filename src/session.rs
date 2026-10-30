//! A display session with a paired tablet (PROTOCOL.md §3): the link, the
//! screen recorded and encoded, its frames to the app, the app's input and
//! reports back; until the app hangs up or stop(). As Screenlet for macOS's.

use crate::cable::Cable;
use crate::input::Input;
use crate::link::{Link, Result, Stream};
use crate::mutter;
use crate::pen::Pen;
use crate::protocol::{self, Opener, Sealer, Stats, FROM_APP, FROM_PC, LARGE_PAYLOAD, PAYLOAD};
use crate::sizes::{self, Rate, Size};
use crate::video::{self, Source, Video};
use serde_json::json;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How the desktop reaches the tablet; "auto" and 0 leave each to the preset and the tablet.
#[derive(Clone, Debug)]
pub struct Mode {
    pub resolution: String,     // "auto", "tablet" (native) or "WxH"
    pub mirror: Option<String>, // a monitor's connector, or a new display
    pub priority: String,       // "latency", "balanced", "clarity"
    pub fps: u32,               // 0: the tablet's refresh rate
    pub codec: String,          // "auto", "avc", "hevc"
    pub kbps: u32,
    pub vbv_frames: f64,
    pub packets: String,        // "auto", "large", "small"
    pub keyframe_seconds: u32,
}

impl Default for Mode {
    fn default() -> Mode {
        Mode { resolution: "auto".into(), mirror: None, priority: "balanced".into(), fps: 0, codec: "auto".into(),
               kbps: 0, vbv_frames: 0.0, packets: "auto".into(), keyframe_seconds: 0 }
    }
}

#[derive(Clone)]
pub enum Path {
    /// A tablet on the network (or its Wi-Fi Direct group), and the id it announced.
    Network(SocketAddr, Option<String>),
    Cable(Arc<Cable>),
}

impl Path {
    pub fn stream(&self) -> Result<Box<dyn Stream>> {
        Ok(match self {
            Path::Network(address, _) => {
                let tcp = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
                tcp.set_nodelay(true)?; // pings and pairing at once, not after a delayed ACK
                tcp.set_read_timeout(Some(Duration::from_secs(1)))?;
                Box::new(tcp)
            }
            Path::Cable(cable) => Box::new(cable.connect()?) as Box<dyn Stream>,
        })
    }

    fn expect(&self) -> Option<&str> {
        match self {
            Path::Network(_, id) => id.as_deref(),
            Path::Cable(_) => None, // any paired tablet: TLS proves which
        }
    }
}

/// Pairs with the app on path (ask shows the code, returns the user's answer): its name.
pub fn pair(path: &Path, ask: &dyn Fn(&str, &str) -> bool) -> Result<String> {
    let mut link = Link::open(path.stream()?, None, Some(ask))?;
    let _ = link.send(&json!({"type": "bye"}));
    Ok(link.name)
}

/// One report, for the window: as the app measured it, and this PC's part.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub fps: u32,
    pub lost: u32,
    pub latency: f64,
    pub latency90: f64,
    pub encoded: Option<f64>,
    pub sending: Option<f64>,
    pub decoding: f64,
    pub stages: [Option<f64>; protocol::STAGES],
    pub kbps: u32,
    pub cap: u32,
    pub skipped: u32,
}

#[derive(Default)]
pub struct Status {
    pub state: String,
    pub started: Option<(Size, u32, String)>, // size, fps, encoder
    pub screen: Option<Size>,                 // the tablet's panel, for the window's resolutions
    pub report: Option<Report>,
    pub paused: bool,
    /// Ended: with an error, or None when stopped or hung up.
    pub ended: Option<Option<String>>,
}

pub struct Session {
    stop: Arc<AtomicBool>,
    pub status: Arc<Mutex<Status>>,
}

pub static STATS_EVERY: AtomicU32 = AtomicU32::new(30); // log one report in this many, and any with losses

impl Session {
    /// pattern: a moving test pattern instead of the desktop (no input).
    pub fn start(path: Path, mode: Mode, pattern: bool) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status { state: "Connecting…".into(), ..Default::default() }));
        let (s, st) = (stop.clone(), status.clone());
        std::thread::Builder::new().name("screenlet-session".into()).spawn(move || {
            let result = run(&path, mode, pattern, &s, &st);
            s.store(true, Ordering::Relaxed);
            let mut status = st.lock().unwrap();
            status.ended = Some(result.err().map(|e| e.to_string()));
        }).expect("a thread");
        Session { stop, status }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn ended(&self) -> bool {
        self.status.lock().unwrap().ended.is_some()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The display's datagrams, over UDP or the cable.
enum Datagrams {
    Udp(UdpSocket, SocketAddr),
    Cable(Arc<Cable>),
}

impl Datagrams {
    fn send(&self, packets: &[&[u8]], batch: &mut Vec<u8>) {
        match self {
            Datagrams::Udp(socket, to) => send_many(socket, to, packets),
            Datagrams::Cable(cable) => { let _ = cable.send_datagrams(packets.iter().copied(), batch); }
        }
    }

    /// Bytes waiting in the socket to leave (TIOCOUTQ), or None over the cable.
    fn queued(&self) -> Option<usize> {
        let Datagrams::Udp(socket, _) = self else { return None };
        let mut n: libc::c_int = 0;
        (unsafe { libc::ioctl(socket.as_raw_fd(), libc::TIOCOUTQ, &mut n) } == 0).then_some(n as usize)
    }
}

/// A frame's packets in one system call (sendmmsg) where it can.
fn send_many(socket: &UdpSocket, to: &SocketAddr, packets: &[&[u8]]) {
    let SocketAddr::V4(v4) = to else {
        for p in packets {
            let _ = socket.send_to(p, to);
        }
        return;
    };
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: v4.port().to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
        sin_zero: [0; 8],
    };
    let mut iovecs: Vec<libc::iovec> = packets.iter()
        .map(|p| libc::iovec { iov_base: p.as_ptr() as *mut _, iov_len: p.len() }).collect();
    let mut headers: Vec<libc::mmsghdr> = iovecs.iter_mut().map(|iov| {
        let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
        h.msg_hdr.msg_name = &address as *const _ as *mut _;
        h.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
        h.msg_hdr.msg_iov = iov;
        h.msg_hdr.msg_iovlen = 1;
        h
    }).collect();
    let mut at = 0;
    while at < headers.len() {
        let n = unsafe { libc::sendmmsg(socket.as_raw_fd(), headers[at..].as_mut_ptr(), (headers.len() - at) as u32, 0) };
        if n <= 0 {
            break; // the queue full or the network gone: the app asks for a keyframe
        }
        at += n as usize;
    }
}

fn run(path: &Path, mut mode: Mode, pattern: bool, stop: &Arc<AtomicBool>, status: &Arc<Mutex<Status>>) -> Result<()> {
    let say = |text: String| status.lock().unwrap().state = text;
    let mut link = Link::open(path.stream()?, path.expect(), None)?;
    let tablet = link.name.clone();
    say(format!("Connecting to {tablet}…"));
    let mut key = [0u8; 32];
    openssl::rand::rand_bytes(&mut key)?;
    let wifi = matches!(path, Path::Network(..));
    let udp = match path {
        Path::Network(..) => {
            let socket = UdpSocket::bind("0.0.0.0:0")?;
            // Room for a keyframe at the highest bitrate, no more: a deeper
            // queue only makes the next frames later when the network slows.
            let size: libc::c_int = 1 << 20;
            unsafe { libc::setsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, &size as *const _ as *const _, 4) };
            Some(socket)
        }
        Path::Cable(_) => None,
    };
    // The codecs this PC encodes in hardware, the preferred first.
    let mut offered = video::codecs(mode.priority == "clarity");
    if mode.codec != "auto" && offered.contains(&mode.codec) {
        offered.sort_by_key(|c| *c != mode.codec);
    }
    let port = udp.as_ref().map_or(9, |s| s.local_addr().map_or(9, |a| a.port()));
    let reply = link.request(&json!({"type": "display", "key": protocol::hex(&key), "port": port, "codecs": offered}))?;
    if reply["type"] == "refused" {
        return Err(format!("{tablet} refused: {}", reply["reason"].as_str().unwrap_or("?")).into());
    }
    if reply["type"] != "display-ready" {
        return Err(format!("the app answered {}", reply["type"]).into());
    }
    let codec = reply["codec"].as_str().filter(|c| offered.iter().any(|o| o == c))
        .ok_or_else(|| format!("the app chose {}, not one of {offered:?}", reply["codec"]))?.to_string();
    let screen = (reply["width"].as_i64().unwrap_or(1920) as i32, reply["height"].as_i64().unwrap_or(1200) as i32);
    let decoder = reply["decoders"][&codec].as_u64().map(|f| f as u32);
    status.lock().unwrap().screen = Some(screen);
    eprintln!("app display: {tablet} shows {}x{} at {} Hz, decodes {}", screen.0, screen.1, reply["refresh"], reply["decoders"]);
    let mirrored = match &mode.mirror {
        Some(connector) => mutter::monitors()?.1.into_iter().find(|m| &m.connector == connector),
        None => None,
    };
    if mode.mirror.is_some() && mirrored.is_none() {
        mode.mirror = None; // that monitor is gone: a new display instead
    }
    let mut size = mirrored.as_ref().map_or_else(|| sizes::choose(&mode.resolution, screen, wifi), |m| sizes::fit(m.size, screen));
    let fps = if mode.fps > 0 { mode.fps } else { sizes::fps(reply["refresh"].as_f64(), decoder) };
    // Automatic: no more pixels a second than the tablet's decoder takes.
    if mode.mirror.is_none() && mode.resolution == "auto" {
        if let Some(d) = decoder.filter(|&d| d < fps) {
            let rate = screen.0 as i64 * screen.1 as i64 * d as i64;
            size = sizes::all(screen).into_iter().find(|s| s.0 as i64 * s.1 as i64 * fps as i64 <= rate).unwrap_or(size);
        }
    }
    let (preset_kbps, preset_vbv) = sizes::priority(&mode.priority);
    let target = if mode.kbps > 0 { mode.kbps } else { sizes::bitrate(preset_kbps, size) };
    let vbv = if mode.vbv_frames > 0.0 { mode.vbv_frames } else { preset_vbv };
    let mut rate = Rate::new(target, wifi);

    let datagrams = Arc::new(match (path, udp) {
        (Path::Network(address, _), Some(socket)) => {
            let to = SocketAddr::new(address.ip(), reply["port"].as_u64().unwrap_or(0) as u16);
            Datagrams::Udp(socket, to)
        }
        (Path::Cable(cable), _) => Datagrams::Cable(cable.clone()),
        _ => unreachable!(),
    });
    let sealer = Arc::new(Mutex::new(Sealer::new(&key, FROM_PC)));
    let payload = Arc::new(AtomicUsize::new(if mode.packets == "small" { PAYLOAD } else { LARGE_PAYLOAD }));
    let fallback = wifi && mode.packets == "auto";

    // The screen: a new monitor (Mutter takes the size from the stream), or one mirrored.
    let screencast = if pattern { None } else { Some(Arc::new(mutter::Session::start(mode.mirror.as_deref())?)) };
    let source = match &screencast {
        Some(s) => Source::PipeWire(s.node, mode.mirror.is_none().then_some(size)),
        None => Source::Pattern,
    };

    // Frames from the encoder's thread to the sender, which seals and sends them.
    let (frames_tx, frames_rx): (SyncSender<(Vec<u8>, bool, i64, i64)>, Receiver<_>) = mpsc::sync_channel(8);
    let waiting = Arc::new(AtomicUsize::new(0));
    let kbps_now = Arc::new(AtomicU32::new(rate.kbps));
    let (d, w, k) = (datagrams.clone(), waiting.clone(), kbps_now.clone());
    // Before a frame goes into the encoder: the network behind (more than
    // 60 ms of the bitrate waiting to leave, or frames waiting for the
    // cable), the frame is left out.
    let skip = Box::new(move || match d.queued() {
        Some(bytes) => bytes > (64 << 10).max(k.load(Ordering::Relaxed) as usize * 60 / 8),
        None => w.load(Ordering::Relaxed) >= 2,
    });
    let w = waiting.clone();
    let on_frame = Box::new(move |bytes: &[u8], keyframe: bool, captured: i64, _into: i64| {
        w.fetch_add(1, Ordering::Relaxed);
        let _ = frames_tx.send((bytes.to_vec(), keyframe, captured, video::now()));
    });
    let keyframe_seconds = if mode.keyframe_seconds > 0 { mode.keyframe_seconds } else { video::KEYFRAME_SECONDS };
    let video = Arc::new(Video::new(source, &codec, size, fps, rate.kbps, vbv, keyframe_seconds, skip, on_frame)?);
    eprintln!("app display: {}x{}@{fps} {codec} ({}) {} kbit/s{}", size.0, size.1, video.name, rate.kbps,
              if wifi { " over Wi-Fi" } else { " over the cable" });
    video.start()?;

    // Measured here: capture -> encoded -> sent, for each report.
    let timings: Arc<Mutex<(Vec<f64>, Vec<f64>, usize)>> = Arc::default();
    let sender = {
        let (datagrams, sealer, payload, timings, waiting, stop) =
            (datagrams.clone(), sealer.clone(), payload.clone(), timings.clone(), waiting.clone(), stop.clone());
        std::thread::Builder::new().name("screenlet-send".into()).spawn(move || {
            let mut number = 0u32;
            let mut body = vec![0u8; protocol::VIDEO_HEADER + LARGE_PAYLOAD];
            let mut sealed: Vec<u8> = Vec::new();
            let mut batch = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let Ok((bytes, keyframe, captured, encoded_at)) = frames_rx.recv_timeout(Duration::from_millis(200)) else { continue };
                waiting.fetch_sub(1, Ordering::Relaxed);
                number = number.wrapping_add(1);
                let unit = payload.load(Ordering::Relaxed);
                let count = bytes.len().div_ceil(unit).max(1);
                sealed.clear();
                sealed.resize(count * (protocol::NONCE + protocol::VIDEO_HEADER + unit + protocol::TAG), 0);
                let mut ends = Vec::with_capacity(count);
                let mut at = 0;
                {
                    let mut sealer = sealer.lock().unwrap();
                    for (index, chunk) in bytes.chunks(unit).enumerate() {
                        let header = protocol::video_header(keyframe, number, index as u16, count as u16, captured);
                        body[..protocol::VIDEO_HEADER].copy_from_slice(&header);
                        body[protocol::VIDEO_HEADER..protocol::VIDEO_HEADER + chunk.len()].copy_from_slice(chunk);
                        let n = sealer.seal(&body[..protocol::VIDEO_HEADER + chunk.len()], &mut sealed[at..]);
                        ends.push((at, at + n));
                        at += n;
                    }
                }
                let packets: Vec<&[u8]> = ends.iter().map(|&(a, b)| &sealed[a..b]).collect();
                datagrams.send(&packets, &mut batch);
                let now = video::now();
                let mut t = timings.lock().unwrap();
                t.0.push((encoded_at - captured) as f64 / 1e6);
                t.1.push((now - captured) as f64 / 1e6);
                t.2 += bytes.len();
            }
        })?
    };

    // The app's packets: from the UDP socket, or the cable's reading thread.
    let (from_app_tx, from_app) = mpsc::channel::<Vec<u8>>();
    match &*datagrams {
        Datagrams::Cable(cable) => {
            let tx = Mutex::new(from_app_tx);
            *cable.on_datagram.lock().unwrap() = Some(Box::new(move |d: &[u8]| { let _ = tx.lock().unwrap().send(d.to_vec()); }));
        }
        Datagrams::Udp(socket, to) => {
            let (socket, app, stop) = (socket.try_clone()?, to.ip(), stop.clone());
            socket.set_read_timeout(Some(Duration::from_millis(200)))?;
            std::thread::Builder::new().name("screenlet-udp-in".into()).spawn(move || {
                let mut buffer = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = socket.recv_from(&mut buffer) {
                        if from.ip() == app && from_app_tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            })?;
        }
    }

    // The pen, once its monitor is known to GNOME: mapped onto it.
    let pen: Arc<OnceLock<Pen>> = Arc::default();
    if let Some(screencast) = &screencast {
        let (pen, extend, mirrored_edid, stop) = (pen.clone(), mode.mirror.is_none(), mirrored.as_ref().map(|m| m.edid.clone()), stop.clone());
        let _ = screencast;
        std::thread::spawn(move || {
            for _ in 0..10 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(300));
                // HiDPI from 2560 wide: Mutter forgets a virtual monitor's scale, so it is set each time.
                if extend && sizes::scale(size) > 1.0 {
                    let _ = mutter::set_virtual_scale(2.0);
                }
                let edid = if extend { mutter::virtual_monitor().map(|m| m.edid) } else { mirrored_edid.clone() };
                if edid.is_some() {
                    if let Some(p) = Pen::open(edid.as_ref()) {
                        let _ = pen.set(p);
                    }
                    return;
                }
            }
        });
    }

    {
        let mut s = status.lock().unwrap();
        s.started = Some((size, fps, video.name.to_string()));
        s.state = format!("On {tablet}, {}x{}", size.0, size.1);
    }

    let receiver = {
        let (video, datagrams, sealer, status, timings, stop, kbps_now, payload) =
            (video.clone(), datagrams.clone(), sealer, status.clone(), timings.clone(), stop.clone(), kbps_now, payload);
        let mut input = screencast.as_ref().map(|s| Input::new(s.clone(), size, pen.clone()));
        let tablet = tablet.clone();
        std::thread::Builder::new().name("screenlet-feedback".into()).spawn(move || {
            let mut opener = Opener::new(&key, FROM_APP);
            let mut plain = [0u8; 2048];
            let mut last_keyframe = Instant::now() - Duration::from_secs(1);
            let mut shown = true;
            let mut reports = 0u32;
            let mut reported_at = Instant::now();
            let mut batch = Vec::new();
            let mut keyframe = |video: &Video| {
                if last_keyframe.elapsed() >= Duration::from_millis(250) {
                    last_keyframe = Instant::now();
                    video.keyframe();
                }
            };
            while !stop.load(Ordering::Relaxed) {
                // Input checked every 5 ms while something touches, rarely otherwise.
                let busy = input.as_ref().is_some_and(|i| i.busy());
                let packet = match from_app.recv_timeout(Duration::from_millis(if busy { 5 } else { 250 })) {
                    Ok(p) => Some(p),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if let Some(input) = input.as_mut() {
                    input.check();
                }
                let Some(packet) = packet else { continue };
                let Some(body) = opener.open(&packet, &mut plain) else { continue };
                match body[0] {
                    protocol::INPUT => {
                        if let Some(input) = input.as_mut() {
                            if !input.packet(body) {
                                eprintln!("app display: a malformed input packet");
                            }
                        }
                    }
                    protocol::KEYFRAME => keyframe(&video),
                    protocol::PING if body.len() == 9 || body.len() == 10 => {
                        let mut pong = [0u8; 17];
                        pong[0] = protocol::PONG;
                        pong[1..9].copy_from_slice(&body[1..9]);
                        pong[9..17].copy_from_slice(&video::now().to_be_bytes());
                        let mut out = [0u8; 64];
                        let n = sealer.lock().unwrap().seal(&pong, &mut out);
                        datagrams.send(&[&out[..n]], &mut batch);
                        if body.len() == 10 && (body[9] != 0) != shown {
                            // Not on the tablet's screen: nothing captured nor encoded meanwhile.
                            shown = body[9] != 0;
                            video.pause(!shown);
                            if !shown {
                                if let Some(input) = input.as_mut() {
                                    input.close();
                                }
                            } else {
                                keyframe(&video);
                            }
                            status.lock().unwrap().paused = !shown;
                        }
                    }
                    protocol::STATS => {
                        let Some(stats) = Stats::parse(body) else { continue };
                        let (mut encoded, mut sent, bytes) = std::mem::take(&mut *timings.lock().unwrap());
                        encoded.sort_by(f64::total_cmp);
                        sent.sort_by(f64::total_cmp);
                        let kbps = (bytes as f64 * 8.0 / reported_at.elapsed().as_secs_f64() / 1000.0) as u32;
                        reported_at = Instant::now();
                        let whole = stats.stages[0].unwrap_or_default();
                        // Frames went and none came out: a network that drops IP fragments. Small packets from now on.
                        if fallback && stats.shown == 0 && !sent.is_empty() && payload.load(Ordering::Relaxed) != PAYLOAD {
                            payload.store(PAYLOAD, Ordering::Relaxed);
                            eprintln!("app display: no frame arrives in large packets: small ones");
                            keyframe(&video);
                        }
                        if stats.shown > 0 {
                            if let Some(next) = rate.report(whole[0], stats.lost) {
                                video.rate(next);
                                kbps_now.store(next, Ordering::Relaxed);
                                eprintln!("app display: frames late ({:.0} ms): {next} kbit/s", whole[0]);
                            }
                        }
                        let median = |v: &[f64]| v.get(v.len() / 2).copied();
                        let report = Report {
                            fps: stats.shown, lost: stats.lost, latency: whole[0], latency90: whole[1],
                            encoded: median(&encoded), sending: median(&sent), decoding: stats.decoding,
                            stages: stats.stages.map(|s| s.map(|s| s[0])), kbps, cap: rate.kbps,
                            skipped: video.skipped.swap(0, Ordering::Relaxed),
                        };
                        reports += 1;
                        if stats.lost > 0 || reports % STATS_EVERY.load(Ordering::Relaxed) == 1 {
                            let f = |v: Option<f64>| v.map_or("?".into(), |v| format!("{v:.1}"));
                            eprintln!("app display: {} fps, lost {}; capture to decoded {:.1} ms (90%: {:.1}, max {:.1}); \
                                       stages: encoded {}, sent {}, first packet {}, spread {}, decoder wait {}, decode {}; \
                                       {} kbit/s (at most {}){}", stats.shown, stats.lost, whole[0], whole[1], whole[2],
                                      f(report.encoded), f(report.sending), f(report.stages[1]), f(report.stages[2]),
                                      f(report.stages[3]), f(report.stages[4]), kbps, rate.kbps,
                                      if report.skipped > 0 { format!("; {} not encoded (the network behind)", report.skipped) } else { String::new() });
                        }
                        let mut s = status.lock().unwrap();
                        s.report = Some(report);
                        s.state = format!("On {tablet}");
                    }
                    _ => {}
                }
            }
            if let Some(input) = input.as_mut() {
                input.close();
            }
        })?
    };

    // Until the app hangs up (Back on the tablet), the pipeline fails, or stop().
    let failure: Mutex<Option<String>> = Mutex::new(None);
    let result = link.hold(|| {
        if let Some(error) = video.error() {
            *failure.lock().unwrap() = Some(error);
            return true;
        }
        stop.load(Ordering::Relaxed) || matches!(&*datagrams, Datagrams::Cable(c) if !c.alive())
    });
    stop.store(true, Ordering::Relaxed);
    let _ = receiver.join();
    let _ = sender.join();
    if let Datagrams::Cable(cable) = &*datagrams {
        *cable.on_datagram.lock().unwrap() = None;
    }
    drop(video);
    drop(screencast);
    result?;
    match failure.into_inner().unwrap() {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}
