<p align="center">
  <img src="packaging/encaje.svg" width="112" height="112" alt="Encaje" />
</p>

<h1 align="center">Encaje</h1>

<p align="center"><strong>English</strong> · <a href="README.es.md">Español</a></p>

<p align="center">
  <strong>Use your Android tablet as a second monitor for your Linux PC.</strong><br />
  Or control it with your PC's mouse, keyboard and clipboard. For GNOME, KDE Plasma and Niri (Wayland),
  over USB cable or WiFi, no root needed. A free, open-source Spacedesk alternative for Linux.
</p>

<div align="center">

[![Download for Linux][badge-linux]][linux-appimage]
[![Download for Android][badge-android]][android-apk]

</div>

<p align="center">
  <a href="https://github.com/sthifen/encaje/releases/latest"><img src="https://img.shields.io/github/v/release/sthifen/encaje?label=version&color=4F8CFF" alt="Latest version" /></a>
  <a href="https://github.com/sthifen/encaje/releases"><img src="https://img.shields.io/github/downloads/sthifen/encaje/total?label=downloads&color=1E3A8A" alt="Downloads" /></a>
  <a href="https://github.com/sthifen/encaje/releases/latest"><img src="https://img.shields.io/github/downloads/sthifen/encaje/latest/total?label=latest%20version%20downloads&color=2563EB" alt="Downloads of the latest version" /></a>
  <a href="https://github.com/sthifen/encaje/stargazers"><img src="https://img.shields.io/github/stars/sthifen/encaje?style=flat&label=stars&color=A78BFA" alt="Stars" /></a>
</p>

![Encaje: a window moves from the laptop to the tablet; then the mouse crosses over to Android and the clipboard is shared](promo/out/encaje-demo.gif)

<p align="center">
  <a href="#install-no-compiling">Install</a> ·
  <a href="#phone-or-tablet-over-usb-like-spacedesk">USB</a> ·
  <a href="#phone-or-tablet-over-wifi">WiFi</a> ·
  <a href="#mouse-and-keyboard-only---input-only">Mouse and keyboard</a> ·
  <a href="https://github.com/sthifen/encaje/releases/latest">All downloads</a>
</p>

## Features

- **Second monitor:** the tablet becomes a real extra display. Drag windows onto it and use it with touch.
- **Up to 120 fps** at the tablet's native resolution, which adapts automatically (rotation included).
- **USB or WiFi:** USB uses `adb reverse` with low latency; WiFi uses WebRTC, or any browser.
- **Mouse and keyboard mode:** the tablet keeps running Android, and your PC's mouse and keyboard move over to it, like Barrier or Deskflow, with nothing to install on the tablet. Copy and paste works both ways.
- **Hardware encoding** with NVENC (NVIDIA) or VA-API (Intel/AMD), falling back to the CPU.
- **First-run guide** that walks you through enabling USB debugging.

## How it works

```
Virtual monitor ──► PipeWire ──► H.264 (NVENC / VA-API / OpenH264)
 (GNOME or KDE)       (BGRx)            │
                                        ├─► WebSocket + WebCodecs ── USB (adb reverse)
                                        └─► WebRTC ───────────────── WiFi / LAN
Viewer ──(touch, mouse, keyboard)──► WebSocket ──► desktop input
```

The only desktop-specific code lives in [`src/backend/`](src/backend):

| Desktop | Virtual monitor | Input | Screen layout |
|---|---|---|---|
| GNOME | Mutter's `RecordVirtual` (D-Bus) | `org.gnome.Mutter.RemoteDesktop` | Encaje saves and restores it |
| KDE Plasma 6 | KWin's `zkde_screencast_unstable_v1` (Wayland) | `org_kde_kwin_fake_input` | KWin remembers it |
| Niri | Free DRM connector + `RecordMonitor` (D-Bus) | `/dev/uinput` | Encaje saves it and applies it with `niri msg` |

On KDE, KWin only grants those protocols to programs with a desktop entry
that requests them (like krfb); Encaje creates one at startup in
`~/.local/share/applications/dev.encaje.kwin.desktop`. The monitor size is
changed with `kscreen-doctor`, which ships with Plasma.

On Niri (experimental, contributed by [@Leimsoto](https://github.com/Leimsoto)), Encaje turns on a free DRM connector (`HDMI-A-*` or `DP-*`) in
`/sys/class/drm/card*-*/status`, publishes it over D-Bus (`org.gnome.Mutter.ScreenCast`)
and injects input through `/dev/uinput`. To avoid `sudo`, your user must be
in the `video` and `input` groups and have this rule in
`/etc/udev/rules.d/99-drm-connector-status.rules`:

```udev
ACTION=="add|change", SUBSYSTEM=="drm", KERNEL=="card[0-9]*-*", RUN+="/bin/sh -c 'chgrp video /sys%p/status && chmod 0664 /sys%p/status'"
```

## Install (no compiling)

Download from the [latest release](../../releases/latest):

- **`Encaje-x86_64.AppImage`**: a single file.
  `chmod +x Encaje-x86_64.AppImage` and double-click it.
- **`encaje-*-x86_64.tar.gz`**: binary + menu shortcut.
  Extract it and run `./install.sh` (installs to `~/.local`, no sudo;
  `./install.sh --uninstall` removes it).
- **`encaje.apk`**: the Android app.

Opening it from the menu shows a window to pick the mode: the tablet as a
second monitor, or with the PC's mouse and keyboard. Closing the window
stops it. The first time, a guide explains how to enable USB debugging and
moves on by itself when it detects the tablet (the **?** button reopens it).
From a terminal (`encaje-launch`) the server starts directly, with its log
in view. Default options (see [Options](#options)) go in
`~/.config/encaje.conf`, for example:

```
--fps 60 --bitrate 20000
```

Requirements: GNOME, KDE Plasma 6 or Niri (experimental) on Wayland, with
PipeWire and GStreamer (installed by default). Works on any distro with glibc 2.34 or newer:
Zorin OS 17, Ubuntu 22.04+, Linux Mint 21+, Pop!_OS 22.04, Debian 12,
Fedora, Arch, openSUSE…
Hardware encoding needs your GPU's GStreamer plugins:
`nvcodec` (NVIDIA) or `va` (`vah264lpenc` / `vah264enc`, Intel/AMD) in
`gstreamer1-plugins-bad`; without them, the CPU is used.

### If something fails: bug report

In the Encaje window, the third round button (top left) generates a
**bug report**; it turns orange if the server fails to start. From a
terminal:

```bash
encaje --diagnostico      # or: ./Encaje-x86_64.AppImage --diagnostico
```

The report (`~/.local/state/encaje/informe-*.md`) collects the distro, the
desktop and session, the glibc, GTK, GStreamer and PipeWire versions,
whether the compositor offers the APIs Encaje uses, the encoders, the GPU,
the connected phones and the end of the log
(`~/.local/state/encaje/encaje.log`). A pre-filled GitHub issue opens: just
describe what happened and drag the file in. Reports in English are welcome.

The packages are built with `packaging/build-dist.sh`.

## Build from source

```bash
scripts/setup-deps.sh          # once: PipeWire headers without sudo
cargo build --release
./target/release/encaje --width 2340 --height 1080 --fps 60
```

A new monitor appears in *Settings → Displays*; you can drag windows onto
it. It disappears when the program exits (Ctrl+C).

### Phone or tablet over USB (like Spacedesk)

1. Enable *Developer options → USB debugging* and plug in the cable.
2. Install the app: `android/build-apk.sh --install` (or copy
   `android/encaje.apk` to the device).
3. Open **Encaje**: it finds the PC by itself and goes full screen.

The server runs `adb reverse tcp:8787 tcp:8787` on every connected device
(and repeats it if the device reconnects). Over USB it uses WebSocket +
WebCodecs, because `adb reverse` only forwards TCP.

**If USB is slow, check the cable.** With a poor-quality cable (or a
charge-only one) the device negotiates USB 1.1 at 12 Mbit/s instead of
480 Mbit/s. Check with:

```bash
cat /sys/bus/usb/devices/*/speed  # next to the SAMSUNG_Android device, etc.
lsusb -t                          # the device should show up at 480M
```

The server adapts anyway: the viewer acknowledges every frame, the server
measures the real delay and lowers the bitrate before it piles up (on a
simulated 7 Mbit/s link the queueing delay drops from ~3 s to ~11 ms).

### Phone or tablet over WiFi

In the app, type the PC's IP in the *WiFi* box, or open
`http://PC-IP:8787` in any browser. It uses WebRTC.

### Mouse and keyboard only (`--input-only`)

The tablet keeps showing **its own Android** and no screen is streamed:
the virtual monitor only tells the desktop where the tablet is. Move the
mouse towards it and the Android pointer appears on the tablet; move back
and the mouse returns to the PC. Like Barrier or Deskflow, but with nothing
to install on the tablet.

```bash
./target/release/encaje --input-only    # or --solo-entrada
```

1. Connect the tablet over USB with *USB debugging* enabled.
2. In *Settings → Displays*, place the new monitor where the tablet is
   (to the right, above…).
3. To type on the tablet, **click on it**: the keyboard goes where you
   last clicked, as with any window.

How it works: a small server is launched on the tablet over adb
(`android/server`, with *shell* permissions, no root and no app) that
creates a virtual HID mouse and keyboard. Android treats them as real
peripherals, so it draws its pointer and understands shortcuts and
special keys. Android 16 ignores absolute HID mice, so the mouse is
relative: pointer acceleration is turned off during the session (and
restored on exit), and the PC keeps track of where the pointer is.

**Shared clipboard.** Whatever you copy on the PC (text or images) can be
pasted on the tablet, and the other way around. On the PC it uses the X11
clipboard, which Mutter keeps in sync with Wayland's; on the tablet, the
server uses Android's clipboard with *shell* permissions. Pasting an
**image** copied on the PC into the tablet needs the app installed:
Android only stores images as `content://` URIs, and the app
(`ClipProvider`) is what serves it. Text works without it.

- The virtual monitor matches the tablet's size in "mouse units", so the
  pointer moves at the same speed as a mouse plugged into it. Rotating the
  tablet changes the monitor's shape.
- Android decides the keyboard layout: if your keyboard isn't US English,
  choose it once on the tablet in *Settings → General management →
  Physical keyboard → Encaje Keyboard*.
- GNOME keeps some shortcuts for itself (the Super key, Alt+Tab).
- With several devices connected, pick one with `ANDROID_SERIAL=…`.
- On Android 15 tablets where pointer acceleration can't be turned off
  (e.g. Lenovo Tab M11), very fast mouse movements can drift slightly;
  leaving and re-entering the tablet fixes it.

## Resolution and performance

The virtual monitor's resolution **adapts to the screen that connects**:
the viewer sends its size in physical pixels and its orientation, and
Mutter changes the monitor mode on the fly (also when the tablet rotates).
Every device gets its exact aspect ratio, with no black bars.

In the viewer's menu (⋮ → *Resolution*):

| Option | Pixels (2340×1080 phone) | |
|---|---|---|
| Native | 2340×1080 | Sharpest |
| 75 % | 1755×810 | ~56 % of the pixels |
| 50 % | 1170×540 | ~25 % of the pixels; ideal for slow links |
| Other… | whatever you type | e.g. `1600x900` |

Lowering the scale reduces the work at **every** stage (GNOME draws less,
the GPU encodes less and less data travels over USB/WiFi); the image is
stretched to full screen, and text and windows look bigger. The choice is
saved on each device. If several connect at once, the last one wins.

Measured with a Galaxy Tab S8+ (120 Hz) over USB, NVENC: ~119 fps at
2800×1752, 2100×1314 and 1400×876, with 11, 8 and 6 ms to decode and no
resyncs.

## Options

| Option | Default | |
|---|---|---|
| `--width`, `--height` | automatic | Fixes the resolution and ignores the viewers'. |
| `--fps` | automatic | Fixes the frames per second. Without it the viewer's display rate is used (60, 90, 120 Hz…), and it lowers itself if the decoder can't keep up. |
| `--max-fps` | 120 | Maximum when the rate is automatic. |
| `--bitrate` | 12000 | kbit/s. |
| `--encoder` | `auto` | `nvenc`, `vaapi`, `openh264`. `auto` tries hardware and falls back to the CPU. |
| `--port` | 8787 | HTTP port. |
| `--no-usb` | | Don't run `adb reverse`. |
| `--input-only` | | Mouse and keyboard only; see [above](#mouse-and-keyboard-only---input-only). |
| `--diagnostico` | | Bug report; see [above](#if-something-fails-bug-report). |

`RUST_LOG=debug` shows more detail. Every 5 s the log records the capture
and send fps, the encoding cost and the frame age, plus what the viewer
measures: decode and draw time, decoder queue and resyncs (also in the
viewer's ⋮ menu).

### Smooth on modest tablets: the native viewer

The Android app has its own viewer (`NativeViewerActivity`): it receives
the same stream as the web viewer, decodes it with **MediaCodec in
low-latency mode** (`KEY_LOW_LATENCY` and each vendor's keys) and draws it
**straight to the screen**, with no WebView, canvas or JavaScript in
between. It opens by default; in the launcher you can untick *Native
viewer* to use the browser one, and its ⋮ menu has *Use the web viewer*,
*Keyboard*, the resolution and the statistics.

The viewer measures its display's refresh rate and the PC streams at that
rate; the app also requests the display mode with the most hertz (many
tablets keep apps at 60 Hz otherwise). If the decoder can't keep up (it
queues frames or takes more than two frames to return each one), the next
lower rate is requested (90 → 75 → 60…) and after a minute it tries again.

Measured on a Lenovo Tab M11 (MediaTek MT8786, 90 Hz display, over USB):

| Viewer | Resolution | fps | Decode delay | Queue | Resyncs |
|---|---|---|---|---|---|
| Web, forced 90 fps | 960×600 | ~81 | 110–135 ms | 14–15 frames | 4–5 every 5 s |
| Web, automatic | 960×600 | 60 | ~14.5 ms | 0 | 0 |
| **Native, automatic** | **1920×1200** | **89.4** | **~14.7 ms** | **1** | **0** |

The server also disables Nagle (`TCP_NODELAY`) on every connection,
coalesces keyframe requests (at most one every 250 ms, even with viewers
constantly joining and leaving) and adds `max_num_reorder_frames = 0` to
NVENC's SPS, so decoders that honour it don't hold frames back.

## Measured performance (2340×1080, GTX 1650 + Alder Lake i7)

| Encoder | Cost per frame | fps sent |
|---|---|---|
| NVENC | ~1.5 ms (async) + ~1.5 ms copy | ≈ captured (~50) |
| OpenH264 (CPU) | 30–80 ms | 10–20 |

OpenH264 is only reasonable up to ~1280×720.

At 2340×1080 and 90 fps with NVENC: 89.3 fps sent, ~7–8 ms from frame
arrival to encoded, 11.3 ms median frame interval and 14.4 ms at p99.

## Tests

```bash
cargo test
# Headless stress test: synthetic image + viewers, slow viewers,
# constant reconnects and an input flood (with garbage); fails if the
# server dies or its memory or thread count grows.
tests/stress/run-local.sh target/release/encaje 60 --fps 90 --width 2340 --height 1080
# The packaged binary (built on Ubuntu 22.04) on Ubuntu 22.04 and
# 24.04, Debian 12, Fedora, Arch and openSUSE, with podman or docker:
packaging/compat/build.sh && tests/distros/run.sh 30
```

## Layout

| Path | |
|---|---|
| `src/backend/mod.rs` | Desktop detection and the common `VirtualDisplay` interface. |
| `src/backend/gnome.rs` | GNOME: virtual monitor and input (D-Bus to Mutter). |
| `src/backend/mutter_layout.rs` | GNOME: monitor layout (`DisplayConfig`). |
| `src/backend/kde.rs` | KDE Plasma: virtual monitor and input (KWin Wayland protocols). |
| `src/backend/niri.rs` | Niri: DRM connector activation, PipeWire screencast and `/dev/uinput` input. |
| `src/backend/layout.rs` | Shared: neighbouring screen and the tablet's saved position. |
| `src/capture.rs` | PipeWire consumer (DMA-BUF); keeps only the latest frame. |
| `src/encoder.rs` | H.264 backends, on-demand keyframes, statistics. |
| `src/h264.rs` | Rewrites the SPS so the decoder doesn't hold frames back. |
| `src/diagnose.rs` | Log in `~/.local/state/encaje/` and bug report. |
| `src/gui.rs`, `src/guide.rs` | Mode window and first-run guide (GTK 4). |
| `packaging/compat/` | Build container (Ubuntu 22.04) for the packages. |
| `tests/` | Stress and distro tests. |
| `src/server.rs` | HTTP (axum): viewer, WebRTC `/offer`, `/ws/video`, `/ws/input`. |
| `src/rtc_peer.rs` | One `PeerConnection` (webrtc-rs) per viewer, no trickle ICE. |
| `src/adb.rs` | USB mode (automatic `adb reverse`). |
| `src/input_only.rs` | `--input-only` mode: tablet server and pointer position. |
| `src/grab_window.rs` | Invisible window over the virtual monitor that captures mouse and keyboard. |
| `src/clipboard.rs` | PC clipboard (X11 + XFixes) for `--input-only` mode. |
| `android/server/` | UHID and clipboard server for the tablet; `android/build-apk.sh` builds `input-server.jar`. |
| `web/index.html` | Viewer (embedded in the binary). |
| `android/` | App: USB/WiFi launcher, native viewer (MediaCodec) and web viewer (WebView). Built without Gradle. |

## Limitations

- Only GNOME, KDE Plasma and Niri on Wayland. Hyprland/Sway (wlroots) or
  X11 (Cinnamon, XFCE) would need another backend in `src/backend/`.
- KDE: tested with a headless KWin 6.7, not yet in a Plasma session with
  the tablet.
- Niri: needs a free HDMI/DP connector on the GPU and the udev rule above;
  the virtual monitor runs at 60 Hz and the keyboard is translated with
  the US/Spanish layout.
- No audio.
- The user interface is in Spanish for now.
- WebRTC doesn't react to the browser's PLIs; it recovers with the
  periodic IDR (every 2 s).

## Contributing

Issues and pull requests are welcome, in English or Spanish. Niri support
came from a contributor, and testing on other desktops, distros and tablets
helps a lot.

## Promotional material

Images for social media (banner, posts, stories) and video in
[`promo/out/`](promo/out); the MP4 videos are in the
[releases](../../releases/latest). Regenerate them with `python3 promo/gen.py`
(needs ImageMagick with librsvg and ffmpeg).

[badge-linux]: https://img.shields.io/badge/Linux-AppImage-1E3A8A?style=for-the-badge&logo=linux&logoColor=white
[badge-android]: https://img.shields.io/badge/Android-APK-3DDC84?style=for-the-badge&logo=android&logoColor=white
[linux-appimage]: https://github.com/sthifen/encaje/releases/latest/download/Encaje-x86_64.AppImage
[android-apk]: https://github.com/sthifen/encaje/releases/latest/download/encaje.apk
