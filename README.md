# podbeam

Stream whatever your Windows PC plays to a HomePod, over AirPlay 2. podbeam runs as a
tray icon: right-click, pick a speaker, done. It is a single small executable with no
installer and no background service.

> podbeam is an independent project. It is not affiliated with, endorsed by or supported
> by Apple. AirPlay and HomePod are trademarks of Apple Inc.

Tested with a HomePod (2nd generation) on Windows 11.

## Features

- Captures all system audio (WASAPI loopback). No virtual audio driver needed.
- Finds AirPlay receivers on the local network via mDNS.
- Pairs with HomePods using HomeKit transient pairing, the password-less pairing a
  HomePod accepts from devices outside your Home. No PIN to type, nothing to install.
- Everything on the wire is encrypted: control channel, event channel and audio.
- Low latency: 80 ms speaker buffer by default (adjustable from 40 to 400 ms), plus a
  small capture buffer that sizes itself to your PC.
- Resends lost packets when the speaker asks for them.

## Install

Download `podbeam.exe` from the [latest release](https://github.com/rhaist/podbeam/releases/latest)
and run it. Each release lists the file's SHA-256, so you can check the download:

```powershell
Get-FileHash .\podbeam.exe
```

The executable is not code-signed, so Windows SmartScreen may warn on first start.

Or build it yourself (Rust 1.90 or newer; on Windows also the Visual Studio C++ build tools):

```
cargo build --release
```

## Usage

**Tray:** start `podbeam.exe` without arguments. A grey dot appears in the notification
area; right-click it to choose a speaker. The dot turns green while streaming. The menu
also has the latency setting, Stop and Quit.

**Command line:**

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

- Windows 10 or 11. The streaming code itself is portable, but the tray and the audio
  capture are only tested on Windows.
- PC and speaker on the same network, with multicast DNS allowed. On Windows, set the
  network profile to **Private**. Across VLANs, use an mDNS reflector or pass the
  speaker's IP address directly.
- In the Home app, *Home Settings → Speakers & TV → Allow Speaker & TV Access* must be
  *Everyone* or *Anyone on the Same Network*. With *Only People Sharing This Home* the
  speaker may refuse to pair.

## How it works

1. **Capture:** a WASAPI loopback stream on the default output device, resampled to
   44.1 kHz stereo.
2. **Pairing:** HomeKit transient pair-setup (SRP-6a, 3072-bit group, SHA-512, fixed PIN
   3939). The SRP session key is fed into HKDF-SHA-512 to derive the channel keys.
3. **Session:** RTSP over the encrypted channel (ChaCha20-Poly1305), with binary-plist
   `SETUP` requests for the session and a realtime ALAC stream, NTP timing and a
   `/feedback` keep-alive.
4. **Audio:** 352-frame ALAC packets over UDP/RTP, each encrypted with
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
- No volume control yet; streams start at about a third of full volume.
- Only AirPlay 2 receivers that support transient pairing (HomePods). AirPlay 1-only
  receivers are not supported.
- Linear-interpolation resampling: fine for a speaker, not audiophile grade.

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
