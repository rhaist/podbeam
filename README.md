# podbeam

Stream whatever your PC plays to a HomePod, over AirPlay 2. On Windows podbeam runs as a
tray icon: right-click, pick a speaker, done. On Linux it is a command-line tool that
works with PulseAudio and PipeWire. Either way it is a single small executable with no
installer and no background service.

> podbeam is an independent project. It is not affiliated with, endorsed by or supported
> by Apple. AirPlay and HomePod are trademarks of Apple Inc.

Tested with a HomePod (2nd generation) on Windows 11, and with a HomePod on Debian with
PipeWire 1.6. Linux audio capture is also tested against PulseAudio in CI.

## Features

- Captures all system audio: WASAPI loopback on Windows, the monitor of the default
  output on PulseAudio/PipeWire. No virtual audio driver needed.
- Finds AirPlay receivers on the local network via mDNS.
- Pairs with HomePods using HomeKit transient pairing, the password-less pairing a
  HomePod accepts from devices outside your Home. No PIN to type, nothing to install.
- Everything on the wire is encrypted: control channel, event channel and audio.
- Transparent conversion: high-quality sinc resampling with clock-drift correction and
  TPDF dither, measured at the 16-bit limit (87 dB) across the audio band.
- Low latency: 80 ms speaker buffer by default (adjustable from 40 to 400 ms), plus a
  small capture buffer that sizes itself to your PC.
- Resends lost packets when the speaker asks for them.
- Follows your system volume: the keyboard's volume and mute keys (or the volume slider)
  control the speaker.

## Install

Download from the [latest release](https://github.com/rhaist/podbeam/releases/latest).
Each file has a `.sha256` file next to it, so you can check the download.

**Windows:** run `podbeam.exe`. Check it with `Get-FileHash .\podbeam.exe`. The
executable is not code-signed, so Windows SmartScreen may warn on first start.

**Linux (x86_64, glibc 2.35+):**

```sh
sha256sum -c podbeam-linux-x86_64.sha256
chmod +x podbeam-linux-x86_64
./podbeam-linux-x86_64 "Living Room"
```

**Build it yourself** with Rust 1.90 or newer. On Windows you also need the Visual Studio
C++ build tools; on Debian/Ubuntu `libasound2-dev` and `pkg-config`.

```
cargo build --release
```

## Usage

**Tray (Windows):** start `podbeam.exe` without arguments. A grey dot appears in the notification
area; right-click it to choose a speaker. The dot turns green while streaming. The menu
also has the latency setting, Stop, Quit and **Open log**. The tray writes everything it
reports, including errors, to `%LOCALAPPDATA%\podbeam\podbeam.log` (moved to
`podbeam.log.1` once it passes 1 MB). Attach it when you report a problem.

**Command line (Windows and Linux):**

```
podbeam "Living Room"         # first device whose name contains this text
podbeam 192.168.1.50          # by address, skips discovery
podbeam "Living Room" 120     # with 120 ms speaker latency
```

Every 10 seconds podbeam prints a status line when something happened, for example:

```
last 10 s: 3 packets resent (network loss), 0 capture dropouts, capture buffer 20 ms
```

Many resends mean your Wi-Fi is struggling: raise the latency. Capture dropouts are
handled automatically by growing the capture buffer.

## Requirements

- Windows 10 or 11, or Linux with PulseAudio or PipeWire (with `pipewire-pulse`, the
  default on current distributions). On Linux podbeam records the monitor of the
  default output, so play audio on that output.
- PC and speaker on the same network, with multicast DNS allowed. On Windows, set the
  network profile to **Private**; on Linux, allow UDP 5353 in your firewall. Across
  VLANs, use an mDNS reflector or pass the speaker's IP address directly.
- In the Home app, *Home Settings → Speakers & TV → Allow Speaker & TV Access* must be
  *Everyone* or *Anyone on the Same Network*. With *Only People Sharing This Home* the
  speaker may refuse to pair.

## How it works

1. **Capture:** a WASAPI loopback stream on the default output device (Windows) or the
   default sink's monitor source (PulseAudio/PipeWire).
2. **Conversion:** a 256-tap sinc resampler brings the audio to AirPlay's 44.1 kHz. Its
   ratio is nudged by at most ±0.1 % to absorb the drift between the sound card's clock
   and the network clock, so there are no skips or dropouts. TPDF dither then reduces it
   to 16 bit. Measured: everything that is not the signal sits 87 dB below it from 1 to
   15 kHz, the limit of dithered 16-bit audio, with a flat frequency response.
3. **Pairing:** HomeKit transient pair-setup (SRP-6a, 3072-bit group, SHA-512, fixed PIN
   3939). The SRP session key is fed into HKDF-SHA-512 to derive the channel keys.
4. **Session:** RTSP over the encrypted channel (ChaCha20-Poly1305), with binary-plist
   `SETUP` requests for the session and a realtime ALAC stream, NTP timing and a
   `/feedback` keep-alive.
5. **Audio:** 352-frame ALAC packets over UDP/RTP, each encrypted with
   ChaCha20-Poly1305, plus sync packets once per second.

## Security notes

- Transient pairing uses a public, fixed PIN. Anyone on your network can stream to the
  speaker this way. That is how the speaker's guest access works, not a podbeam weakness.
- podbeam stores nothing: no credentials, no keys, no settings. Keys live in memory for
  one session.
- All replies from the network are treated as untrusted (sizes are capped, every
  encrypted frame is authenticated).

## Limitations

- One speaker at a time; no multi-room or stereo pairs.
- No tray icon on Linux yet; use the command line.
- The speaker follows the system volume, so your PC's own speakers change with it, and
  muting the PC mutes the speaker too. Where the system volume cannot be read, podbeam
  starts at about a third of full volume.
- Only AirPlay 2 receivers that support transient pairing (HomePods). AirPlay 1-only
  receivers are not supported.
- AirPlay carries 16-bit/44.1 kHz (CD quality) from third-party senders, and the HomePod
  applies its own room correction and EQ, so the chain cannot be bit-perfect end to end.

## Credits

The protocol work stands on the shoulders of
[OwnTone](https://github.com/owntone/owntone-server),
[pyatv](https://github.com/postlund/pyatv),
[libraop](https://github.com/philippe44/libraop) and
[shairport-sync](https://github.com/mikebrady/shairport-sync). podbeam contains no code
from these projects; they were used as protocol references.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
