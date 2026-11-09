# podbeam

Stream whatever your PC plays to a HomePod over AirPlay 2: a tray icon on Windows and
Linux (right-click, pick a speaker), or one command. A single small executable, no
installer, no background service.

> podbeam is an independent project. It is not affiliated with, endorsed by or supported
> by Apple. AirPlay and HomePod are trademarks of Apple Inc.

Tested with HomePods from Windows 11 and from Debian with PipeWire 1.6.

## Features

- No PIN, no pairing step: podbeam uses the guest access HomePods offer on your network.
- About 125 ms from the PC to the speaker (measured). On Linux with PipeWire, video players
  are told this delay and keep the picture in sync, and only the HomePod plays: the PC's
  speakers go quiet while streaming and come back afterwards.
- The volume and mute keys control the speaker.
- Encrypted end to end; lost packets are resent; resampling and dither are clean to the
  16-bit limit.

## Install

Download from the [latest release](https://github.com/rhaist/podbeam/releases/latest); each
file has a `.sha256` next to it.

**Windows:** run `podbeam.exe` (check it with `Get-FileHash .\podbeam.exe`). It is not
code-signed, so SmartScreen may warn on first start.

**Linux (x86_64, glibc 2.35+):**

```sh
sha256sum -c podbeam-linux-x86_64.sha256
chmod +x podbeam-linux-x86_64
./podbeam-linux-x86_64
```

**Build it yourself** with Rust 1.90 or newer: `cargo build --release`. Windows also needs
the Visual Studio C++ build tools; Debian/Ubuntu needs `libasound2-dev` and `pkg-config`.

## Usage

**Tray:** start podbeam without arguments and right-click the grey dot in the tray to pick
a speaker; it turns green while streaming. The menu also has the latency, Stop, **Open
log** and Quit. On Linux, started from a terminal, the tray moves to the background. It
shows in Cinnamon, KDE, XFCE and waybar (Hyprland, Sway); GNOME needs the AppIndicator
extension. To start it at login, add `exec-once = podbeam` to the Hyprland config, or put
a `.desktop` file with `Exec=podbeam` in `~/.config/autostart`.

**Command line:**

```
podbeam "Living Room"         # first device whose name contains this text
podbeam 192.168.1.50          # by address, skips discovery
podbeam "Living Room" 120     # with 120 ms latency (80 to 2000)
```

When something happened, podbeam prints a line every 10 s, e.g. `last 10 s: 3 packets
resent (network loss), 0 capture dropouts, capture buffer 20 ms`. Many resends mean weak
Wi-Fi: raise the latency.

**Log:** the tray writes everything to `%LOCALAPPDATA%\podbeam\podbeam.log` (Windows) or
`~/.local/state/podbeam/podbeam.log` (Linux). Attach it when you report a problem.

## Requirements

- Windows 10 or 11, or Linux with PipeWire (`pipewire-pulse`) or PulseAudio.
- PC and speaker on the same network with multicast DNS allowed: on Windows set the network
  to **Private**, on Linux allow UDP 5353. Across VLANs, use an mDNS reflector or the
  speaker's IP address.
- In the Home app, *Home Settings → Speakers & TV → Allow Speaker & TV Access* must be
  *Everyone* or *Anyone on the Same Network*; otherwise the speaker may refuse to pair.

## Security notes

- The guest pairing uses a fixed public PIN, so anyone on your network can stream to the
  speaker. That is how its guest access works, not a podbeam weakness.
- podbeam stores no credentials, keys or settings, and treats everything from the network
  as untrusted.

## Limitations

- One speaker at a time; no multi-room or stereo pairs. HomePods only (AirPlay 2 with
  transient pairing).
- On Windows, and on Linux without PipeWire, the PC's own speakers keep playing too.
- AirPlay from third-party senders is 16-bit/44.1 kHz, and the HomePod applies its own EQ,
  so the chain is not bit-perfect.

## Credits

The protocol work stands on the shoulders of
[OwnTone](https://github.com/owntone/owntone-server),
[pyatv](https://github.com/postlund/pyatv),
[libraop](https://github.com/philippe44/libraop) and
[shairport-sync](https://github.com/mikebrady/shairport-sync), used as references; podbeam
contains no code from them.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
