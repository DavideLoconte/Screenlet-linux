<p align="center"><img src="docs/icon.png" width="128" alt="Screenlet icon"></p>

<h1 align="center">Screenlet for GNOME</h1>

<p align="center">Your Android tablet as a second display for GNOME on Wayland,<br>
with touch and pen, over a USB-C cable first and Wi-Fi when there is none.</p>

---

This is the Linux half of Screenlet. The tablet runs
[Screenlet Companion](https://github.com/DavideLoconte/Screenlet-companion). There is also
[Screenlet for macOS](https://github.com/DavideLoconte/Screenlet-macos) and
[Screenlet for Windows](https://github.com/DavideLoconte/Screenlet-windows). They all speak
the [Screenlet protocol](PROTOCOL.md). Written in
Rust: GTK 4 and libadwaita, GStreamer, and Mutter's own D-Bus APIs, so no
screen-sharing dialog appears.

## Use it

1. Install the package and the companion on the tablet.
2. Plug the tablet in, or have it on the same Wi-Fi, and click **Connect** in
   Screenlet's window. The first time, check that both screens show the same six digits.
   With two on the cable, each has its own line; one shows the desktop at a time.
3. The **Tablet** toggle in Quick Settings (the GNOME Shell extension, turn it on
   in *Extensions*) starts and stops it. When closed, the window keeps Screenlet in
   the background. Showing the desktop as soon as a paired tablet is plugged in is an
   option in the settings.

The window has the same settings as Screenlet for macOS: Extend or Mirror (one
of this PC's monitors), Resolution, Optimize for (lowest latency, balanced,
sharpest), the connection (automatic, Wi-Fi, cable), and, under Advanced,
refresh rate, codec, bitrate, largest frame, packets and keyframes. The latency
is shown live, stage by stage.

- **Fingers** are real touches, so GNOME's own gestures work and nothing waits.
- **The pen** is a real pen tablet with pressure, tilt and hover, mapped onto
  the tablet's monitor. A small root helper makes it (D-Bus activated; polkit
  allows only the session in front of the PC, with no password). Without the
  helper, the pen drives the pointer.
- **With no shared network**, a paired tablet runs its own Wi-Fi Direct group
  and the window offers to join it (through NetworkManager).

## Install

Download the `.rpm` (Fedora) or `.deb` (Ubuntu 24.04 and later) from
[Releases](../../releases). The package installs:
- the app;
- the pen helper with its D-Bus and polkit files;
- a udev rule so you can use the tablet on the cable;
- the GNOME Shell extension.

From source, with the libraries in `Containerfile`:

```bash
cargo build --release && sudo tools/package.sh install
```

It needs GNOME 46 or later on Wayland, and one of these hardware encoders,
which Screenlet tries in this order:
- Intel's VA low-power encoder (`vah264lpenc`, from `gstreamer1-plugins-bad`);
- NVIDIA's NVENC (`nvh264enc`);
- Intel's QSV;
- Intel's VA.

Without one of them, x264 or OpenH264 encodes in software. HEVC is used
where both sides have it in hardware.

## Latency

- **Capture**: Mutter's PipeWire stream at the tablet's refresh rate, with
  one frame queued at most (an older one is dropped).
- **Conversion on the GPU**: to the encoder's format and size, VA's for
  Intel and CUDA's for NVIDIA.
- **Encode**: the encoder set for latency (NVENC P1 with the
  ultra-low-latency tune, VA's fastest target usage, no B-frames, one
  reference).
- **Packets**: sealed with AES-GCM into buffers kept for them (0.5 µs a
  packet) and sent with one system call per frame (`sendmmsg`), or framed
  into as few USB transfers as fit.
- **Backpressure**: a frame is not encoded while the network has more
  than 60 ms of the bitrate waiting. The bitrate follows the delay the
  tablet measures, as Screenlet for macOS's does.

Measured on this PC (Intel UHD 630 and GTX 1660 Ti, GNOME 50) at 1920×1200
and 60 Hz, before the network and the tablet:

| Encoder | Into the encoder | Encoder | Capture to sent |
|---|---|---|---|
| NVENC H.264, CUDA conversion | 3.5 ms | 2.9 ms | 6.5 ms |
| NVENC HEVC, CUDA conversion | 3.6 ms | 2.6 ms | 6.2 ms |
| NVENC H.264, CPU conversion | 2.9 ms | 3.8 ms | 6.9 ms |
| x264 (software) | 3.7 ms | 3.0 ms | 6.7 ms |

```bash
screenlet --bench 10 [mirror]           # this PC's stages alone (mirror: through Mutter)
screenlet --show-cable 20 [options]     # on the tablet, latency logged stage by stage
screenlet --show-wifi 20 [options]      # options: mirror h264 hevc latency clarity 60 120 small large pattern
SCREENLET_ENCODER=nvh265enc SCREENLET_ENCODER_PROPS="preset=p2" screenlet --bench 10
```

## Build and test

```bash
tools/dev.sh cargo test        # in a Fedora container with the libraries (podman; DOCKER=docker)
tools/dev.sh tools/package.sh rpm
```

The tests check the shared protocol vectors (`vectors.json`), the cable's
framing, sizes and bitrate, touch and pen, and the cost of sealing. CI builds
the RPM on Fedora and the DEB on Ubuntu. A `v*` tag publishes them in a
release, with the extension.

## To do

The list of tablets on the cable was written without Linux at hand: CI builds and tests
it, nothing more. To try here:

- With one tablet: its own name in the window, Connect, Disconnect, and it still works
  with the desktop shown on plugging in.
- `screenlet --tablets` lists it by name and serial, with no udev rule needed for that.
- With two tablets: a line each, with their names, and Connect shows the desktop on the
  right one; `--show-cable 20 serial=SERIAL` likewise.
- With two, Connect on the other one after a session: the first goes back to what it
  was and the second takes over, with one session at a time.
- With two, nothing starts by itself on plugging in, and the Quick Settings toggle
  opens the window.
- Two of the same model show the last digits of their serials.
- A tablet with MediaTek's USB id, as a Bigme HiBreak Pro: seen, allowed by the udev rule
  (`70-screenlet.rules`, reinstalled), the cable and the picture. Its decoder needed the
  companion's fix for its largest size.
- Whether the bitrate falls to its lowest on such a tablet while the whole picture moves,
  as on Windows, though the cable has room to spare.

## License

MIT. Screenlet started as a fork of
[Plus One](https://github.com/GabrieleSantangelo/PlusOne) by Gabriele Santangelo.

## AI disclosure

Much of this code was written by Claude, Anthropic's AI model, through Claude Code, under
the author's direction. The author chose what to build and tested it on real hardware.
