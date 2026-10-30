//! Capture and encoding with GStreamer: the PipeWire stream Mutter gives (or
//! a test pattern), a hardware encoder set for latency, each frame to
//! on_frame(bytes, keyframe, captured ns) from the streaming thread.

use crate::link::Result;
use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const KEYFRAME_SECONDS: u32 = 10;

#[derive(Clone, Copy, PartialEq)]
enum Gpu {
    Cpu,
    Va,
    Cuda,
}

/// An encoder: its element, what it takes, and how it gets it.
#[derive(Clone, Copy)]
struct Encoder {
    element: &'static str,
    codec: &'static str,      // PROTOCOL.md's name: "avc", "hevc"
    hardware: bool,
    /// Where the frame is turned into the encoder's format and size: on the
    /// CPU (videoconvert), or on the GPU (VA's vapostproc, CUDA's convert).
    gpu: Gpu,
    format: &'static str,
    /// Properties set when the element has them (they differ between GStreamer versions).
    settings: &'static [(&'static str, &'static str)],
}

const NVENC: &[(&str, &str)] = &[("preset", "p1"), ("tune", "ultra-low-latency"), ("rc-mode", "cbr"),
    ("zero-reorder-delay", "true"), ("zerolatency", "true"), ("bframes", "0"), ("b-frames", "0"), ("aud", "false")];
const VA: &[(&str, &str)] = &[("rate-control", "cbr"), ("b-frames", "0"), ("ref-frames", "1"),
    ("target-usage", "7"), ("aud", "false")];
const QSV: &[(&str, &str)] = &[("rate-control", "cbr"), ("low-latency", "true"), ("b-frames", "0"),
    ("ref-frames", "1"), ("target-usage", "7")];
const OPENH264: &[(&str, &str)] = &[("complexity", "low"), ("rate-control", "bitrate"), ("usage-type", "screen")];
const X264: &[(&str, &str)] = &[("tune", "zerolatency"), ("speed-preset", "ultrafast"), ("bframes", "0")];

/// In order of preference for each codec. Intel's low-power encoder first
/// (measured at 1920x1200@60: 3.8 ms to the output, against NVENC's 4.7 and
/// the NVIDIA GPU woken), then NVENC, Intel's others, software H.264 last.
const ENCODERS: &[Encoder] = &[
    Encoder { element: "vah264lpenc", codec: "avc", hardware: true, gpu: Gpu::Va, format: "NV12", settings: VA },
    Encoder { element: "nvh264enc", codec: "avc", hardware: true, gpu: Gpu::Cuda, format: "NV12", settings: NVENC },
    Encoder { element: "nvcudah264enc", codec: "avc", hardware: true, gpu: Gpu::Cuda, format: "NV12", settings: NVENC },
    Encoder { element: "qsvh264enc", codec: "avc", hardware: true, gpu: Gpu::Cpu, format: "NV12", settings: QSV },
    Encoder { element: "vah264enc", codec: "avc", hardware: true, gpu: Gpu::Va, format: "NV12", settings: VA },
    Encoder { element: "vah265lpenc", codec: "hevc", hardware: true, gpu: Gpu::Va, format: "NV12", settings: VA },
    Encoder { element: "nvh265enc", codec: "hevc", hardware: true, gpu: Gpu::Cuda, format: "NV12", settings: NVENC },
    Encoder { element: "nvcudah265enc", codec: "hevc", hardware: true, gpu: Gpu::Cuda, format: "NV12", settings: NVENC },
    Encoder { element: "qsvh265enc", codec: "hevc", hardware: true, gpu: Gpu::Cpu, format: "NV12", settings: QSV },
    Encoder { element: "vah265enc", codec: "hevc", hardware: true, gpu: Gpu::Va, format: "NV12", settings: VA },
    Encoder { element: "x264enc", codec: "avc", hardware: false, gpu: Gpu::Cpu, format: "I420", settings: X264 },
    Encoder { element: "openh264enc", codec: "avc", hardware: false, gpu: Gpu::Cpu, format: "I420", settings: OPENH264 },
];

fn usable(e: &Encoder) -> bool {
    let _ = gst::init();
    let converter = match e.gpu {
        Gpu::Cpu => "videoconvert",
        Gpu::Va => "vapostproc",
        Gpu::Cuda => "cudaconvertscale",
    };
    gst::ElementFactory::find(e.element).is_some() && gst::ElementFactory::find(converter).is_some()
        && std::env::var("SCREENLET_ENCODER").map_or(true, |only| only == e.element)
}

/// The codecs this PC encodes in hardware, the preferred first (HEVC first
/// for clarity), or software H.264 alone: PROTOCOL.md §3's "codecs".
pub fn codecs(clarity: bool) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for e in ENCODERS.iter().filter(|e| e.hardware && usable(e)) {
        if !found.iter().any(|c| c == e.codec) {
            found.push(e.codec.into());
        }
    }
    if clarity {
        found.sort_by_key(|c| c != "hevc");
    } else {
        found.sort_by_key(|c| c != "avc");
    }
    if found.is_empty() {
        found.push("avc".into());
    }
    found
}

pub enum Source {
    /// Mutter's stream, and the size to ask for: a virtual monitor takes it
    /// (None: a monitor as it is, scaled here).
    PipeWire(u32, Option<(i32, i32)>),
    /// A moving pattern, for tests and measures without a desktop.
    Pattern,
}

/// bytes, keyframe, captured, into the encoder (ns, the monotonic clock).
pub type OnFrame = Box<dyn FnMut(&[u8], bool, i64, i64) + Send>;

pub struct Video {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    keyframe: Arc<AtomicBool>,
    fps: u32,
    vbv_frames: f64,
    /// Frames not encoded since this was last read: the network behind.
    pub skipped: Arc<AtomicU32>,
    pub name: &'static str,
}

fn set(element: &gst::Element, name: &str, value: &str) {
    if element.find_property(name).is_some() {
        element.set_property_from_str(name, value);
    }
}

/// The monotonic clock in ns, as the pipeline's clock and the pings use.
pub fn now() -> i64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

impl Video {
    /// skip(): asked before each frame goes into the encoder; true leaves it
    /// out (the network is behind: a frame not encoded breaks nothing, a
    /// frame queued makes every later one late).
    #[allow(clippy::too_many_arguments)]
    pub fn new(source: Source, codec: &str, size: (i32, i32), fps: u32, kbps: u32, vbv_frames: f64, keyframe_seconds: u32,
               skip: Box<dyn Fn() -> bool + Send + Sync>, mut on_frame: OnFrame) -> Result<Video> {
        gst::init()?;
        let found = ENCODERS.iter().find(|e| e.codec == codec && usable(e)).ok_or(format!("no {codec} encoder here"))?;
        // For measuring: SCREENLET_CONVERT=cpu converts on the CPU whatever the encoder.
        let cpu = Encoder { gpu: Gpu::Cpu, ..*found };
        let e = if std::env::var("SCREENLET_CONVERT").as_deref() == Ok("cpu") { &cpu } else { found };
        let pipeline = gst::Pipeline::new();
        let make = |factory: &str| gst::ElementFactory::make(factory).build().map_err(|_| format!("no {factory} in GStreamer"));
        let src = match source {
            Source::PipeWire(node, _) => {
                let src = make("pipewiresrc")?;
                src.set_property("path", node.to_string());
                set(&src, "do-timestamp", "true");
                set(&src, "always-copy", "false");
                // A still desktop sends no frames: one again now and then repairs a loss.
                set(&src, "keepalive-time", "250");
                src
            }
            Source::Pattern => {
                let src = make("videotestsrc")?;
                src.set_property("is-live", true);
                set(&src, "pattern", "ball");
                src
            }
        };
        // Mutter refreshes the virtual monitor at the stream's max-framerate.
        let caps = gst::Caps::builder("video/x-raw").field("format", "BGRA")
            .field("max-framerate", gst::Fraction::new(fps as i32, 1)).field("framerate", gst::FractionRange::new(
                gst::Fraction::new(0, 1), gst::Fraction::new(fps as i32, 1)));
        let caps = match source {
            Source::Pattern => gst::Caps::builder("video/x-raw").field("format", "BGRA").field("width", size.0)
                .field("height", size.1).field("framerate", gst::Fraction::new(fps as i32, 1)).build(),
            Source::PipeWire(_, Some(asked)) => caps.field("width", asked.0).field("height", asked.1).build(),
            Source::PipeWire(_, None) => caps.build(),
        };
        let filter = make("capsfilter")?;
        filter.set_property("caps", &caps);
        // One frame waits at most: an older one is dropped, never queued.
        let queue = make("queue")?;
        set(&queue, "max-size-buffers", "1");
        set(&queue, "max-size-bytes", "0");
        set(&queue, "max-size-time", "0");
        set(&queue, "leaky", "downstream");
        // To the encoder's format and the stream's size (a mirrored monitor is scaled): on the GPU for VA.
        let mut convert = Vec::new();
        let memory = match e.gpu {
            Gpu::Cpu => "memory:SystemMemory",
            Gpu::Va => "memory:VAMemory",
            Gpu::Cuda => "memory:CUDAMemory",
        };
        let raw = gst::Caps::builder("video/x-raw").features([memory])
            .field("format", e.format).field("width", size.0).field("height", size.1)
            .field("colorimetry", "bt709").build();
        if e.gpu == Gpu::Va {
            convert.push(make("vapostproc")?);
        } else if e.gpu == Gpu::Cuda {
            convert.push(make("cudaupload")?);
            convert.push(make("cudaconvertscale")?);
        } else {
            let c = make("videoconvertscale").or_else(|_| make("videoconvert"))?;
            set(&c, "n-threads", "4");
            convert.push(c);
        }
        let to = make("capsfilter")?;
        to.set_property("caps", &raw);
        convert.push(to);
        let encoder = make(e.element)?;
        for (name, value) in e.settings {
            set(&encoder, name, value);
        }
        // For measuring: other settings, "name=value,..." (bench, tuning).
        if let Ok(extra) = std::env::var("SCREENLET_ENCODER_PROPS") {
            for pair in extra.split(',').filter_map(|p| p.split_once('=')) {
                set(&encoder, pair.0.trim(), pair.1.trim());
            }
        }
        set(&encoder, "key-int-max", &(fps * keyframe_seconds).to_string());
        set(&encoder, "gop-size", &(fps * keyframe_seconds).to_string());
        let parse = make(if codec == "hevc" { "h265parse" } else { "h264parse" })?;
        set(&parse, "config-interval", "-1");
        let stream = make("capsfilter")?;
        stream.set_property("caps", gst::Caps::builder(if codec == "hevc" { "video/x-h265" } else { "video/x-h264" })
            .field("stream-format", "byte-stream").field("alignment", "au").build());
        let sink = gst_app::AppSink::builder().sync(false).build();
        sink.set_property("async", false);
        let mut chain: Vec<&gst::Element> = vec![&src, &filter, &queue];
        chain.extend(convert.iter());
        chain.extend([&encoder, &parse, &stream, sink.upcast_ref()]);
        pipeline.add_many(chain.iter().copied())?;
        gst::Element::link_many(chain.iter().copied())?;

        let video = Video { pipeline, encoder, keyframe: Arc::new(AtomicBool::new(false)), fps, vbv_frames,
                            skipped: Arc::new(AtomicU32::new(0)), name: e.element };
        video.rate(kbps);

        // When each frame going in was captured, in order: one comes out per
        // one in (no B frames), and a flush drops those held.
        let captured: Arc<Mutex<VecDeque<(i64, i64)>>> = Arc::default();
        let pad = video.encoder.static_pad("sink").ok_or("the encoder has no sink pad")?;
        let (times, keyframe, skipped) = (captured.clone(), video.keyframe.clone(), video.skipped.clone());
        // GStreamer's VA encoders answer a force-key-unit with an I frame that is
        // no IDR, without SPS: they are flushed instead, which starts a new GOP.
        let flush = e.gpu == Gpu::Va;
        let base = video.pipeline.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if skip() {
                skipped.fetch_add(1, Ordering::Relaxed);
                return gst::PadProbeReturn::Drop;
            }
            if keyframe.swap(false, Ordering::Relaxed) {
                if flush {
                    let segment = pad.sticky_event::<gst::event::Segment>(0);
                    pad.send_event(gst::event::FlushStart::new());
                    pad.send_event(gst::event::FlushStop::new(false));
                    if let Some(segment) = segment {
                        pad.send_event(segment);
                    }
                    times.lock().unwrap().clear();
                } else {
                    pad.send_event(gst_video_force_key_unit());
                }
            }
            // The frame's age on the pipeline's clock (PipeWire's own, not
            // the monotonic one): capture time = now less that age.
            let at = info.buffer().and_then(|b| b.pts()).and_then(|pts| {
                let segment = pad.sticky_event::<gst::event::Segment>(0)?;
                let segment = segment.segment().downcast_ref::<gst::ClockTime>()?.clone();
                let stamped = segment.to_running_time(pts)?.nseconds() as i64 + base.base_time()?.nseconds() as i64;
                let age = base.clock()?.time().nseconds() as i64 - stamped;
                (0..1_000_000_000).contains(&age).then(|| now() - age)
            }).unwrap_or_else(now);
            let mut times = times.lock().unwrap();
            times.push_back((at, now()));
            while times.len() > 16 {
                times.pop_front();
            }
            gst::PadProbeReturn::Ok
        });
        sink.set_callbacks(gst_app::AppSinkCallbacks::builder().new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let (at, into) = captured.lock().unwrap().pop_front().unwrap_or_else(|| (now(), now()));
            on_frame(map.as_slice(), keyframe, at, into);
            Ok(gst::FlowSuccess::Ok)
        }).build());
        Ok(video)
    }

    pub fn start(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn pause(&self, paused: bool) {
        let _ = self.pipeline.set_state(if paused { gst::State::Paused } else { gst::State::Playing });
    }

    pub fn keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// The bitrate in kbit/s, and the largest frame: vbv_frames average ones.
    pub fn rate(&self, kbps: u32) {
        set(&self.encoder, "bitrate", &kbps.to_string());
        let kbit = (kbps as f64 / self.fps as f64 * self.vbv_frames) as u32;
        let ms = (1000.0 * self.vbv_frames / self.fps as f64) as u32;
        for (name, value) in [("vbv-buffer-size", kbit), ("cpb-size", kbit), ("cpb-length", ms), ("vbv-buf-capacity", ms)] {
            set(&self.encoder, name, &value.to_string());
        }
    }

    /// The pipeline's first error, if it failed.
    pub fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(e) = message.view() {
                return Some(format!("{}: {}", e.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error()));
            }
        }
        None
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// GstForceKeyUnit, all headers: the event gst-video builds, made by hand.
fn gst_video_force_key_unit() -> gst::Event {
    gst::event::CustomDownstream::new(gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build())
}
