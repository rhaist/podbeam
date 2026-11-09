//! Stream Windows system audio to a HomePod over AirPlay 2 (HomeKit transient pairing, NTP timing).
//! Usage: podbeam [name-filter | ip[:port]] [latency-ms, default 80]
//! Without a target (or with "") it runs as a Windows tray icon with a device menu.

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use cpal::SizedSample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use num_bigint::BigUint;
use plist::{Dictionary, Value};
use sha2::{Digest, Sha512};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Res<T> = Result<T, Box<dyn std::error::Error>>;
type Pcm = Arc<Mutex<VecDeque<i16>>>; // interleaved stereo, 44.1 kHz

const RATE: u32 = 44100;
const FRAMES: usize = 352; // frames per RTP packet ("spf")
const MS: usize = RATE as usize * 2 / 1000; // interleaved samples per millisecond
const HISTORY: usize = 512; // sent packets kept for retransmission (~4 s)

#[cfg(windows)]
mod tray;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Res<()> {
    let mut args = std::env::args().skip(1);
    let target = args.next().unwrap_or_default();
    let latency_ms: u32 = args
        .next()
        .map_or(Ok(80), |v| v.parse())
        .ok()
        .filter(|ms| (20..=2000).contains(ms))
        .ok_or("latency must be 20..2000 ms")?;
    #[cfg(windows)]
    if target.is_empty() {
        return tray::run(latency_ms);
    }
    let addr = match (target.parse::<SocketAddr>(), target.parse::<IpAddr>()) {
        (Ok(a), _) => a,
        (_, Ok(ip)) => (ip, 7000).into(),
        _ => find(&target, &discover()?)?,
    };
    play(addr, latency_ms, &Arc::default())
}

/// Shared between a streaming session and whoever started it (the command line or the tray).
#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
    status: Mutex<String>,
    error: Mutex<Option<String>>,
}

impl Ctl {
    fn say(&self, msg: String) {
        println!("{msg}");
        *self.status.lock().unwrap() = msg;
    }

    fn fail(&self, msg: String) {
        self.say(format!("error: {msg}"));
        *self.error.lock().unwrap() = Some(msg);
        self.stop.store(true, Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

/// Capture Windows audio and stream it to `addr` until `ctl.stop` is set or the connection fails.
fn play(addr: SocketAddr, latency_ms: u32, ctl: &Arc<Ctl>) -> Res<()> {
    let pcm = Pcm::default();
    let stream = capture(pcm.clone())?;
    stream.play()?;
    stream_to(addr, pcm, latency_ms * RATE / 1000, ctl)
}

/// Browse `_raop._tcp` for 3 s and return every receiver found, sorted by name.
fn discover() -> Res<Vec<(String, SocketAddr)>> {
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let rx = mdns.browse("_raop._tcp.local.")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = Vec::new();
    println!("searching for AirPlay devices...");
    while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        let mdns_sd::ServiceEvent::ServiceResolved(s) = ev else {
            continue;
        };
        // Instance names look like "AABBCCDDEEFF@Living Room._raop._tcp.local."
        let full = s.get_fullname();
        let name = full
            .split_once('@')
            .map_or(full, |(_, n)| n)
            .trim_end_matches("._raop._tcp.local.");
        if let Some(ip) = s.get_addresses_v4().into_iter().next() {
            found.push((name.to_string(), SocketAddr::from((ip, s.get_port()))));
        }
    }
    let _ = mdns.shutdown();
    found.sort();
    found.dedup_by(|a, b| a.0 == b.0);
    Ok(found)
}

/// The first device whose name contains `filter` (case-insensitive).
fn find(filter: &str, devices: &[(String, SocketAddr)]) -> Res<SocketAddr> {
    let hit = devices
        .iter()
        .find(|(n, _)| n.to_lowercase().contains(&filter.to_lowercase()));
    let names = devices
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    hit.map(|d| d.1).ok_or_else(|| format!("no AirPlay device matching '{filter}' (found: {names}; same network? Windows network profile Private?)").into())
}

/// WASAPI loopback: an input stream opened on the default *output* device captures what Windows plays.
fn capture(pcm: Pcm) -> Res<cpal::Stream> {
    let dev = cpal::default_host()
        .default_output_device()
        .ok_or("no default output device")?;
    let cfg = dev.default_output_config()?;
    match cfg.sample_format() {
        cpal::SampleFormat::F32 => loopback::<f32>(&dev, cfg.config(), pcm),
        cpal::SampleFormat::I16 => loopback::<i16>(&dev, cfg.config(), pcm),
        f => Err(format!("unsupported output sample format {f}").into()),
    }
}

fn loopback<T: SizedSample>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    pcm: Pcm,
) -> Res<cpal::Stream>
where
    f32: cpal::FromSample<T>,
{
    let ch = cfg.channels as usize;
    let step = cfg.sample_rate as f64 / RATE as f64;
    let (mut prev, mut pos) = ([0f32; 2], 0f64); // resampler state carried across callbacks
    // shortcut: linear-interpolation resampling (usually 48k -> 44.1k), fine on a speaker; use a sinc resampler (rubato) if you hear aliasing.
    let on_data = move |data: &[T], _: &cpal::InputCallbackInfo| {
        let n = data.len() / ch;
        // Frame i of this callback, with index 0 meaning the last frame of the previous one.
        let frame = |i: usize| {
            if i == 0 {
                prev
            } else {
                [
                    data[(i - 1) * ch].to_sample(),
                    data[(i - 1) * ch + ch.min(2) - 1].to_sample(),
                ]
            }
        };
        let mut q = pcm.lock().unwrap();
        while pos < n as f64 {
            let (i, t) = (pos as usize, pos.fract() as f32);
            let (a, b) = (frame(i), frame(i + 1));
            q.extend(
                (0..2).map(|c| ((a[c] + (b[c] - a[c]) * t).clamp(-1.0, 1.0) * 32767.0) as i16),
            );
            pos += step;
        }
        pos -= n as f64;
        if n > 0 {
            prev = frame(n);
        }
    };
    // WASAPI flags a "discontinuity" whenever playback starts or stops; the gap is harmless here.
    let on_err = |e: cpal::Error| {
        if !e.to_string().contains("underrun or overrun") {
            eprintln!("capture error: {e}")
        }
    };
    Ok(dev.build_input_stream(cfg, on_data, on_err, None)?)
}

// ---------------------------------------------------------------- crypto

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random generator failed");
    b
}

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    parts.iter().for_each(|p| h.update(p));
    h.finalize().into()
}

fn hkdf(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf::Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm)
        .expand(info.as_bytes(), &mut out)
        .unwrap();
    out
}

fn chacha_nonce(counter: u64) -> chacha20poly1305::Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n.into()
}

/// One direction of an encrypted HAP channel: frames of `u16 LE length | ciphertext | tag`, length is the AAD.
struct Cipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

impl Cipher {
    fn new(key: [u8; 32]) -> Self {
        Cipher {
            aead: ChaCha20Poly1305::new(&key.into()),
            counter: 0,
        }
    }

    fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + 18 * plain.len().div_ceil(1024));
        for block in plain.chunks(1024) {
            let len = (block.len() as u16).to_le_bytes();
            out.extend_from_slice(&len);
            out.extend(
                self.aead
                    .encrypt(
                        &chacha_nonce(self.counter),
                        Payload {
                            msg: block,
                            aad: &len,
                        },
                    )
                    .unwrap(),
            );
            self.counter += 1;
        }
        out
    }

    fn open(&mut self, len: [u8; 2], ct_and_tag: &[u8]) -> io::Result<Vec<u8>> {
        let plain = self.aead.decrypt(
            &chacha_nonce(self.counter),
            Payload {
                msg: ct_and_tag,
                aad: &len,
            },
        );
        self.counter += 1;
        plain.map_err(|_| io::Error::other("HAP frame failed authentication"))
    }
}

/// TCP reader that transparently decrypts HAP frames once `cipher` is set.
struct Decrypting {
    tcp: TcpStream,
    cipher: Option<Cipher>,
    buf: Vec<u8>,
    pos: usize,
}

impl Decrypting {
    fn new(tcp: TcpStream, cipher: Option<Cipher>) -> BufReader<Self> {
        BufReader::new(Decrypting {
            tcp,
            cipher,
            buf: vec![],
            pos: 0,
        })
    }
}

impl Read for Decrypting {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let Some(c) = &mut self.cipher else {
            return self.tcp.read(out);
        };
        if self.pos == self.buf.len() {
            let mut len = [0u8; 2];
            self.tcp.read_exact(&mut len)?;
            let mut ct = vec![0u8; u16::from_le_bytes(len) as usize + 16];
            self.tcp.read_exact(&mut ct)?;
            (self.buf, self.pos) = (c.open(len, &ct)?, 0);
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

const N_3072: &str = "
    FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DD
    EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED
    EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F
    83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B
    E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA0510
    15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7
    ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200C
    BBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

fn srp_group() -> (BigUint, BigUint) {
    (
        BigUint::parse_bytes(N_3072.replace(char::is_whitespace, "").as_bytes(), 16).unwrap(),
        BigUint::from(5u8),
    )
}

fn pad384(x: &BigUint) -> Vec<u8> {
    let b = x.to_bytes_be();
    [vec![0u8; 384 - b.len()], b].concat()
}

struct Srp {
    a_pub: Vec<u8>,
    m1: [u8; 64],
    m2: [u8; 64], // proof we expect back from the receiver
    key: [u8; 64],
}

/// SRP-6a client as used by HomeKit pair-setup (RFC 5054 3072-bit group, SHA-512).
fn srp_client(user: &str, pin: &str, salt: &[u8], b_pub: &[u8], a_priv: &[u8]) -> Res<Srp> {
    let (n, g) = srp_group();
    let num = |h: [u8; 64]| BigUint::from_bytes_be(&h);
    let b = BigUint::from_bytes_be(b_pub);
    if &b % &n == BigUint::ZERO {
        return Err("SRP: receiver sent an invalid public key".into());
    }
    let a = BigUint::from_bytes_be(a_priv);
    let a_pub = g.modpow(&a, &n);
    let k = num(sha512(&[&n.to_bytes_be(), &pad384(&g)]));
    let u = num(sha512(&[&pad384(&a_pub), &pad384(&b)]));
    let x = num(sha512(&[
        salt,
        &sha512(&[format!("{user}:{pin}").as_bytes()]),
    ]));
    let kv = (&k * g.modpow(&x, &n)) % &n;
    let key = sha512(&[&((&b + &n - kv) % &n)
        .modpow(&(&a + &u * &x), &n)
        .to_bytes_be()]);

    let (hn, hg) = (sha512(&[&n.to_bytes_be()]), sha512(&[&g.to_bytes_be()]));
    let hxor: Vec<u8> = hn.iter().zip(hg).map(|(x, y)| x ^ y).collect();
    let a_pub = a_pub.to_bytes_be();
    // Hash the values as they travel on the wire (salt and B as received, A as sent).
    let m1 = sha512(&[
        &hxor,
        &sha512(&[user.as_bytes()]),
        salt,
        &a_pub,
        b_pub,
        &key,
    ]);
    Ok(Srp {
        m2: sha512(&[&a_pub, &m1, &key]),
        a_pub,
        m1,
        key,
    })
}

/// HAP TLV8: values over 255 bytes are split into consecutive items of the same type.
fn tlv(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(t, v) in items {
        for chunk in v.chunks(255) {
            out.extend([t, chunk.len() as u8]);
            out.extend_from_slice(chunk);
        }
    }
    out
}

fn tlv_get(mut data: &[u8], t: u8) -> Vec<u8> {
    let mut out = Vec::new();
    while data.len() >= 2 && data.len() >= 2 + data[1] as usize {
        let (typ, len) = (data[0], data[1] as usize);
        if typ == t {
            out.extend_from_slice(&data[2..2 + len]);
        }
        data = &data[2 + len..];
    }
    out
}

// ---------------------------------------------------------------- RTSP

/// Read one RTSP message: (first line, CSeq header, body). Sizes are capped: the peer is untrusted.
fn read_msg(rd: &mut impl BufRead) -> Res<(String, Option<String>, Vec<u8>)> {
    let line = |rd: &mut dyn BufRead| -> io::Result<String> {
        let mut l = String::new();
        rd.take(8192).read_line(&mut l)?;
        Ok(l)
    };
    let first = line(rd)?;
    if first.is_empty() {
        return Err("connection closed by receiver".into());
    }
    let (mut cseq, mut len) = (None, 0usize);
    loop {
        let line = line(rd)?;
        if line.trim().is_empty() {
            break;
        }
        match line
            .split_once(':')
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim()))
        {
            Some((k, v)) if k == "content-length" => len = v.parse()?,
            Some((k, v)) if k == "cseq" => cseq = Some(v.to_string()),
            _ => {}
        }
    }
    if len > 1 << 20 {
        return Err(format!("receiver sent a {len}-byte body").into());
    }
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body)?;
    Ok((first.trim().to_string(), cseq, body))
}

struct Rtsp {
    tcp: TcpStream,
    rd: BufReader<Decrypting>,
    out: Option<Cipher>,
    cseq: u32,
    id: String,
    url: String,
}

impl Rtsp {
    /// Send a request and return (status, body).
    fn req(
        &mut self,
        method: &str,
        uri: &str,
        extra: &str,
        body: Option<(&str, &[u8])>,
    ) -> Res<(u16, Vec<u8>)> {
        self.cseq += 1;
        let uri = if uri.is_empty() { &self.url } else { uri };
        let mut msg = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: AirPlay/381.13\r\nDACP-ID: {id}\r\nActive-Remote: 1\r\n{extra}",
            self.cseq,
            id = self.id
        );
        if let Some((ct, _)) = body {
            msg += &format!("Content-Type: {ct}\r\n");
        }
        let b = body.map_or(&[][..], |(_, b)| b);
        let msg = [
            format!("{msg}Content-Length: {}\r\n\r\n", b.len()).as_bytes(),
            b,
        ]
        .concat();
        match &mut self.out {
            Some(c) => self.tcp.write_all(&c.seal(&msg))?,
            None => self.tcp.write_all(&msg)?,
        }
        let (status, _, body) = read_msg(&mut self.rd)?;
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok());
        Ok((
            code.ok_or_else(|| format!("bad reply to {method}: {status:?}"))?,
            body,
        ))
    }

    fn ok(
        &mut self,
        method: &str,
        uri: &str,
        extra: &str,
        body: Option<(&str, &[u8])>,
    ) -> Res<Vec<u8>> {
        match self.req(method, uri, extra, body)? {
            (200, b) => Ok(b),
            (s, _) => Err(format!("receiver answered {method} {uri} with {s}").into()),
        }
    }

    fn setup(&mut self, body: Dictionary) -> Res<Dictionary> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &Value::Dictionary(body))?;
        let reply = self.ok(
            "SETUP",
            "",
            "",
            Some(("application/x-apple-binary-plist", &buf)),
        )?;
        Ok(plist::from_bytes::<Value>(&reply)?
            .into_dictionary()
            .ok_or("SETUP reply is not a plist dictionary")?)
    }

    /// HomeKit transient pair-setup (fixed PIN 3939, M1..M4), then switch the connection to encrypted mode.
    fn pair_transient(&mut self) -> Res<[u8; 64]> {
        let pair = |r: &mut Self, body: &[u8]| {
            r.ok(
                "POST",
                "/pair-setup",
                "X-Apple-HKP: 4\r\n",
                Some(("application/octet-stream", body)),
            )
        };
        // M1: method 0 (pair-setup), state 1, flags (type 0x13) = transient
        let m2 = pair(self, &tlv(&[(0, &[0]), (6, &[1]), (0x13, &[0x10])]))?;
        if !tlv_get(&m2, 7).is_empty() {
            return Err(format!("pair-setup error {:?}; set AirPlay access to 'Everyone on this network' in the Home app", tlv_get(&m2, 7)).into());
        }
        let srp = srp_client(
            "Pair-Setup",
            "3939",
            &tlv_get(&m2, 2),
            &tlv_get(&m2, 3),
            &random::<32>(),
        )?;
        let m4 = pair(self, &tlv(&[(6, &[3]), (3, &srp.a_pub), (4, &srp.m1)]))?;
        if tlv_get(&m4, 4) != srp.m2 {
            return Err(format!(
                "pair-setup: receiver proof mismatch (error {:?})",
                tlv_get(&m4, 7)
            )
            .into());
        }
        self.out = Some(Cipher::new(hkdf(
            &srp.key,
            "Control-Salt",
            "Control-Write-Encryption-Key",
        )));
        self.rd.get_mut().cipher = Some(Cipher::new(hkdf(
            &srp.key,
            "Control-Salt",
            "Control-Read-Encryption-Key",
        )));
        Ok(srp.key)
    }
}

/// The HomePod pushes status events over this channel and waits for a 200 to each.
fn serve_events(mut tcp: TcpStream, key: [u8; 64]) -> Res<()> {
    let mut out = Cipher::new(hkdf(&key, "Events-Salt", "Events-Read-Encryption-Key"));
    let mut rd = Decrypting::new(
        tcp.try_clone()?,
        Some(Cipher::new(hkdf(
            &key,
            "Events-Salt",
            "Events-Write-Encryption-Key",
        ))),
    );
    loop {
        let (first, cseq, _) = read_msg(&mut rd)?;
        let cseq = cseq.map(|c| format!("CSeq: {c}\r\n")).unwrap_or_default();
        tcp.write_all(
            &out.seal(format!("RTSP/1.0 200 OK\r\n{cseq}Content-Length: 0\r\n\r\n").as_bytes()),
        )?;
        eprintln!("event: {first}");
    }
}

// ---------------------------------------------------------------- streaming

/// NTP timestamp (seconds since 1900, 32.32 fixed point), big-endian.
fn ntp_now() -> [u8; 8] {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (((d.as_secs() + 2_208_988_800) << 32) | frac).to_be_bytes()
}

/// One uncompressed ("escape") ALAC stereo frame, which every ALAC decoder accepts.
fn alac(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2 + 4);
    let (mut acc, mut n) = (0u64, 0u32);
    let mut put = |v: u64, bits: u32| {
        acc = (acc << bits) | v;
        n += bits;
        while n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
        }
    };
    // tag=CPE(1), instance 0, 12 unused bits, has-size 0, shift 0, escape(uncompressed) 1
    for (v, bits) in [(1, 3), (0, 4), (0, 12), (0, 1), (0, 2), (1, 1)] {
        put(v, bits);
    }
    samples.iter().for_each(|&s| put(s as u16 as u64, 16));
    put(7, 3); // END tag
    put(0, 7); // pad to a byte boundary (23 + 16k + 3 bits)
    out
}

/// RTP packet with ChaCha20-Poly1305 payload: header | ciphertext | tag | 8-byte nonce; AAD = timestamp+SSRC.
fn audio_packet(
    aead: &ChaCha20Poly1305,
    first: bool,
    seq: u16,
    rtp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut pkt = vec![0x80, if first { 0xe0 } else { 0x60 }];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&rtp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    let mut nonce = [0u8; 12];
    nonce[4..6].copy_from_slice(&seq.to_le_bytes());
    let aad: [u8; 8] = pkt[4..12].try_into().unwrap();
    pkt.extend(
        aead.encrypt(
            &nonce.into(),
            Payload {
                msg: payload,
                aad: &aad,
            },
        )
        .unwrap(),
    );
    pkt.extend_from_slice(&nonce[4..]);
    pkt
}

fn hex(b: &[u8], sep: &str) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(sep)
}

fn dict<const N: usize>(items: [(&str, Value); N]) -> Dictionary {
    items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn int(n: impl Into<u64>) -> Value {
    Value::Integer(n.into().into())
}

/// Answer the receiver's clock-sync requests: echo its send time, add our receive/send times.
fn serve_timing(sock: UdpSocket, ctl: Arc<Ctl>) {
    let mut b = [0u8; 128];
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    while !ctl.stopped() {
        if let Ok((32.., from)) = sock.recv_from(&mut b) {
            let now = ntp_now();
            let reply = [
                &[0x80, 0xd3, 0x00, 0x07, 0, 0, 0, 0][..],
                &b[24..32],
                &now,
                &now,
            ]
            .concat();
            let _ = sock.send_to(&reply, from);
        }
    }
}

/// Resend packets the receiver reports lost (8-byte request: 80 d5 xx xx <first seq> <count>).
fn serve_resends(
    ctrl: UdpSocket,
    audio: UdpSocket,
    to: SocketAddr,
    history: Arc<Mutex<Vec<Vec<u8>>>>,
    resent: Arc<AtomicU32>,
    ctl: Arc<Ctl>,
) {
    let mut b = [0u8; 64];
    let _ = ctrl.set_read_timeout(Some(Duration::from_millis(500)));
    while !ctl.stopped() {
        if !matches!(ctrl.recv_from(&mut b), Ok((8, _))) || b[..2] != [0x80, 0xd5] {
            continue;
        }
        let (first, count) = (
            u16::from_be_bytes([b[4], b[5]]),
            u16::from_be_bytes([b[6], b[7]]),
        );
        let h = history.lock().unwrap();
        for seq in (0..count.min(64)).map(|i| first.wrapping_add(i)) {
            let pkt = &h[seq as usize % HISTORY];
            if pkt.len() > 4 && pkt[2..4] == seq.to_be_bytes() {
                let _ = audio.send_to(pkt, to);
                resent.fetch_add(1, Relaxed);
            }
        }
    }
}

/// Our own jitter buffer between WASAPI capture and the network, sized automatically. The HomePod's
/// latency is fixed for the session, but this part can change any time without the receiver noticing.
struct Cushion {
    ms: usize,     // fill level to reach before sending real audio again
    primed: bool,  // currently sending captured audio
    dry: u32,      // packets sent as silence since the queue ran dry
    calm: u32,     // packets sent since the last dropout
    glitches: u32, // dropouts since the last stats line
}

impl Cushion {
    const MIN_MS: usize = 10; // must hold at least one packet (8 ms)
    const MAX_MS: usize = 100;

    fn new() -> Self {
        Cushion {
            ms: 15,
            primed: false,
            dry: u32::MAX,
            calm: 0,
            glitches: 0,
        }
    }

    /// One packet of audio, or None to send silence while the queue refills.
    fn take(&mut self, q: &mut VecDeque<i16>) -> Option<Vec<i16>> {
        // shortcut: capture and network clocks drift, so a backlog well above the target is dropped
        // (an inaudible skip every few minutes); a drift-tracking resampler would avoid it.
        if q.len() > (self.ms + 50) * MS {
            let excess = q.len() - (self.ms + 10) * MS;
            q.drain(..excess);
        }
        if self.primed && q.len() < FRAMES * 2 {
            (self.primed, self.dry) = (false, 0);
        }
        if !self.primed {
            self.dry = self.dry.saturating_add(1);
            if q.len() < self.ms * MS {
                return None;
            }
            self.primed = true;
            // Dry for under 200 ms means capture arrived too late, not that Windows paused playback: grow by 5 ms.
            if self.dry <= 25 {
                (self.ms, self.calm, self.glitches) =
                    ((self.ms + 5).min(Self::MAX_MS), 0, self.glitches + 1);
            }
        }
        self.calm += 1;
        if self.calm >= 3750 {
            // 30 s without a dropout: give back 1 ms.
            (self.ms, self.calm) = ((self.ms - 1).max(Self::MIN_MS), 0);
        }
        Some(q.drain(..FRAMES * 2).collect())
    }
}

/// Ends a session whichever way stream_to returns: stops the helper threads, then sends TEARDOWN.
struct EndSession {
    ctl: Arc<Ctl>,
    events: Option<TcpStream>,
    rtsp: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EndSession {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Relaxed);
        if let Some(t) = &self.events {
            let _ = t.shutdown(Shutdown::Both);
        }
        if let Some(h) = self.rtsp.take() {
            let _ = h.join();
        }
    }
}

/// `latency` is in samples: how far behind real time the HomePod plays.
fn stream_to(addr: SocketAddr, pcm: Pcm, latency: u32, ctl: &Arc<Ctl>) -> Res<()> {
    let mut end = EndSession {
        ctl: ctl.clone(),
        events: None,
        rtsp: None,
    };
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    tcp.set_nodelay(true)?;
    let local = tcp.local_addr()?.ip();
    let id: [u8; 8] = random();
    let sid = u32::from_be_bytes(random());
    let mut r = Rtsp {
        rd: Decrypting::new(tcp.try_clone()?, None),
        tcp,
        out: None,
        cseq: 0,
        id: hex(&id, ""),
        url: format!("rtsp://{local}/{sid}"),
    };
    let key = r.pair_transient()?;
    ctl.say(format!("paired with {addr}"));

    let timing = UdpSocket::bind("0.0.0.0:0")?;
    let ctrl = UdpSocket::bind("0.0.0.0:0")?;
    let u: [u8; 16] = random();
    let session = r.setup(dict([
        ("deviceID", Value::String(hex(&id[2..], ":"))),
        (
            "sessionUUID",
            Value::String(format!(
                "{}-{}-{}-{}-{}",
                hex(&u[..4], ""),
                hex(&u[4..6], ""),
                hex(&u[6..8], ""),
                hex(&u[8..10], ""),
                hex(&u[10..], "")
            )),
        ),
        ("timingPort", int(timing.local_addr()?.port())),
        ("timingProtocol", Value::String("NTP".into())),
    ]))?;
    let event_port = session
        .get("eventPort")
        .and_then(Value::as_unsigned_integer)
        .ok_or("SETUP reply has no eventPort")?;
    match TcpStream::connect_timeout(
        &(addr.ip(), event_port as u16).into(),
        Duration::from_secs(5),
    ) {
        Ok(tcp) => {
            end.events = Some(tcp.try_clone()?);
            let c = ctl.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve_events(tcp, key)
                    && !c.stopped()
                {
                    eprintln!("event channel closed: {e}");
                }
            });
        }
        Err(e) => eprintln!("no event channel ({e}), continuing without"),
    }
    let c = ctl.clone();
    std::thread::spawn(move || serve_timing(timing, c));

    r.ok("RECORD", "", "", None)?;

    let shk: [u8; 32] = key[..32].try_into()?; // audio key, the same choice OwnTone makes
    let stream = r.setup(dict([(
        "streams",
        Value::Array(vec![Value::Dictionary(dict([
            ("type", int(96u8)),              // realtime audio
            ("ct", int(2u8)),                 // ALAC
            ("audioFormat", int(0x40000u32)), // ALAC 44100/16/2
            ("audioMode", Value::String("default".into())),
            ("spf", int(FRAMES as u64)),
            ("sr", int(RATE)),
            // The HomePod buffers up to latencyMax; pin both ends to what we want.
            ("latencyMin", int(latency)),
            ("latencyMax", int(latency)),
            ("shk", Value::Data(shk.to_vec())),
            ("controlPort", int(ctrl.local_addr()?.port())),
            ("isMedia", Value::Boolean(true)),
            ("supportsDynamicStreamID", Value::Boolean(false)),
            ("streamConnectionID", int(sid)),
        ]))]),
    )]))?;
    let s0 = stream
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_dictionary)
        .ok_or("stream SETUP reply has no streams")?;
    let port = |k: &str| {
        s0.get(k)
            .and_then(Value::as_unsigned_integer)
            .map(|p| SocketAddr::from((addr.ip(), p as u16)))
            .ok_or(format!("stream SETUP reply has no {k}"))
    };
    let (audio_to, ctrl_to) = (port("dataPort")?, port("controlPort")?);

    // AirPlay volume is -30 (min) .. 0 dB (max); start at roughly a third so nothing blasts.
    r.ok(
        "SET_PARAMETER",
        "",
        "",
        Some(("text/parameters", b"volume: -20.000000\r\n")),
    )?;
    ctl.say(format!(
        "streaming to {addr} with {} ms latency",
        latency * 1000 / RATE
    ));

    // Keep-alive in its own thread so a slow reply never delays audio; the HomePod ends the session without it.
    r.tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    let c = ctl.clone();
    end.rtsp = Some(std::thread::spawn(move || {
        let mut last = Instant::now();
        while !c.stopped() {
            std::thread::sleep(Duration::from_millis(100));
            if last.elapsed() >= Duration::from_secs(2) {
                if let Err(e) = r.req("POST", "/feedback", "", None) {
                    return c.fail(format!("lost the HomePod ({e})"));
                }
                last = Instant::now();
            }
        }
        let _ = r.req("TEARDOWN", "", "", None);
    }));

    let aead = ChaCha20Poly1305::new(&shk.into());
    let audio = UdpSocket::bind("0.0.0.0:0")?;
    let history = Arc::new(Mutex::new(vec![Vec::new(); HISTORY]));
    let resent = Arc::new(AtomicU32::new(0));
    {
        let (ctrl, audio, history, resent, c) = (
            ctrl.try_clone()?,
            audio.try_clone()?,
            history.clone(),
            resent.clone(),
            ctl.clone(),
        );
        std::thread::spawn(move || serve_resends(ctrl, audio, audio_to, history, resent, c));
    }

    let (mut seq, mut rtp, ssrc) = (
        u16::from_be_bytes(random()),
        u32::from_be_bytes(random()),
        u32::from_be_bytes(random()),
    );
    let start = Instant::now();
    let (mut sent, mut last_stats, mut cushion) = (0u64, Instant::now(), Cushion::new());
    let mut shown_ms = cushion.ms;
    while !ctl.stopped() {
        let due = start + Duration::from_secs_f64(sent as f64 * FRAMES as f64 / RATE as f64);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if sent % 125 == 0 {
            // About once a second: "frame rtp-latency is playing at NTP now"; the first one carries the extension bit.
            let sync = [
                &[if sent == 0 { 0x90 } else { 0x80 }, 0xd4, 0x00, 0x07][..],
                &rtp.wrapping_sub(latency).to_be_bytes(),
                &ntp_now(),
                &rtp.to_be_bytes(),
            ]
            .concat();
            ctrl.send_to(&sync, ctrl_to)?;
        }

        let samples = cushion
            .take(&mut pcm.lock().unwrap())
            .unwrap_or_else(|| vec![0; FRAMES * 2]);
        let pkt = audio_packet(&aead, sent == 0, seq, rtp, ssrc, &alac(&samples));
        audio.send_to(&pkt, audio_to)?;
        history.lock().unwrap()[seq as usize % HISTORY] = pkt;
        (seq, rtp, sent) = (
            seq.wrapping_add(1),
            rtp.wrapping_add(FRAMES as u32),
            sent + 1,
        );

        if last_stats.elapsed() >= Duration::from_secs(10) {
            // Many resends: raise the latency argument. The capture buffer sizes itself.
            let (rs, gl) = (
                resent.swap(0, Relaxed),
                std::mem::take(&mut cushion.glitches),
            );
            if rs + gl > 0 || cushion.ms != shown_ms {
                ctl.say(format!("last 10 s: {rs} packets resent (network loss), {gl} capture dropouts, capture buffer {} ms", cushion.ms));
                shown_ms = cushion.ms;
            }
            last_stats = Instant::now();
        }
    }
    drop(end); // stop helpers and send TEARDOWN before reporting
    ctl.error
        .lock()
        .unwrap()
        .take()
        .map_or(Ok(()), |e| Err(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alac_frame_layout() {
        let s: Vec<i16> = (0..FRAMES * 2)
            .map(|i| (i as i16).wrapping_mul(97) ^ -21846)
            .collect();
        let f = alac(&s);
        assert_eq!(f.len(), (23 + FRAMES * 2 * 16 + 3).div_ceil(8));
        let bit = |i: usize| (f[i / 8] >> (7 - i % 8)) & 1;
        let read =
            |at: usize, bits: usize| (0..bits).fold(0u32, |a, k| a << 1 | bit(at + k) as u32);
        assert_eq!(read(0, 3), 1); // CPE
        assert_eq!(read(22, 1), 1); // escape flag
        for (k, &x) in s.iter().enumerate() {
            assert_eq!(read(23 + k * 16, 16) as u16, x as u16);
        }
        assert_eq!(read(23 + s.len() * 16, 3), 7); // END
    }

    /// Stopping a session must also end its helper threads, or every device switch in the tray leaks them.
    #[test]
    fn helpers_exit_on_stop() {
        let ctl = Arc::<Ctl>::default();
        let (c1, c2) = (ctl.clone(), ctl.clone());
        let sock = || UdpSocket::bind("127.0.0.1:0").unwrap();
        let (to, history) = (
            sock().local_addr().unwrap(),
            Arc::new(Mutex::new(vec![Vec::new(); HISTORY])),
        );
        let t = std::thread::spawn(move || serve_timing(sock(), c1));
        let r = std::thread::spawn(move || {
            serve_resends(sock(), sock(), to, history, Arc::default(), c2)
        });
        ctl.stop.store(true, Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !(t.is_finished() && r.is_finished()) {
            assert!(
                Instant::now() < deadline,
                "helper threads still running after stop"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn find_by_name_part() {
        let d: Vec<(String, SocketAddr)> = ["Kitchen", "Living Room"]
            .iter()
            .zip([10u8, 11])
            .map(|(n, i)| (n.to_string(), ([192, 0, 2, i], 7000).into()))
            .collect();
        assert_eq!(find("living", &d).unwrap(), d[1].1);
        assert!(
            find("bedroom", &d)
                .unwrap_err()
                .to_string()
                .contains("Kitchen, Living Room")
        );
        assert!(find("x", &[]).is_err());
    }

    /// Capture arrives in 30 ms bursts while packets leave every 8 ms: the buffer must grow until the
    /// dropouts stop, ignore a real pause, and creep back down once the bursts stop.
    #[test]
    fn cushion_adapts_to_bursty_capture() {
        // Simulate `from..to` ms; returns the times (ms) of every dropout. Panics if audio was thrown away.
        fn run(
            c: &mut Cushion,
            q: &mut VecDeque<i16>,
            from: usize,
            to: usize,
            burst_ms: usize,
        ) -> Vec<usize> {
            let (mut glitches, mut produced, mut consumed, queued) = (vec![], 0, 0, q.len());
            for t in from..to {
                if burst_ms > 0 && t % burst_ms == 0 {
                    q.extend(std::iter::repeat_n(1i16, burst_ms * MS));
                    produced += burst_ms * MS;
                }
                if t % 8 == 0 {
                    let before = c.glitches;
                    consumed += c.take(q).map_or(0, |s| s.len());
                    if c.glitches > before {
                        glitches.push(t);
                    }
                }
            }
            assert_eq!(
                queued + produced,
                consumed + q.len(),
                "bursts must not be trimmed as drift"
            );
            glitches
        }
        let (mut c, mut q) = (Cushion::new(), VecDeque::new());
        let g = run(&mut c, &mut q, 0, 60_000, 30);
        assert!(
            !g.is_empty() && g.iter().all(|&t| t < 30_000),
            "dropouts: {g:?}"
        );
        assert!(
            c.ms > Cushion::MIN_MS && c.ms <= 50,
            "buffer settled at {} ms",
            c.ms
        );
        let ms = c.ms;
        let g = [
            run(&mut c, &mut q, 60_000, 62_000, 0),
            run(&mut c, &mut q, 62_000, 63_000, 30),
        ]
        .concat(); // 2 s pause
        assert!(
            c.ms <= ms && g.is_empty(),
            "a pause must not grow the buffer ({} ms, {g:?})",
            c.ms
        );
        assert!(run(&mut c, &mut q, 63_000, 400_000, 2).is_empty()); // smooth capture again
        assert!(c.ms < ms, "buffer should shrink back, still {} ms", c.ms);
    }

    #[test]
    fn tlv_fragments_long_values() {
        let long = [7u8; 384];
        let t = tlv(&[(6, &[3]), (3, &long), (4, &[1, 2])]);
        assert_eq!(t.len(), 3 + (2 + 255) + (2 + 129) + 4);
        assert_eq!(tlv_get(&t, 3), long);
        assert_eq!(tlv_get(&t, 4), [1, 2]);
        assert!(tlv_get(&t, 7).is_empty());
    }

    /// Plays the server side of SRP-6a and checks both ends derive the same key and proofs.
    #[test]
    fn srp_matches_server_side() {
        let (n, g) = srp_group();
        let salt: [u8; 16] = random();
        let x = BigUint::from_bytes_be(&sha512(&[&salt, &sha512(&[b"Pair-Setup:3939"])]));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&sha512(&[&n.to_bytes_be(), &pad384(&g)]));
        let b = BigUint::from_bytes_be(&random::<32>());
        let b_pub = (&k * &v + g.modpow(&b, &n)) % &n;

        let srp = srp_client(
            "Pair-Setup",
            "3939",
            &salt,
            &b_pub.to_bytes_be(),
            &random::<32>(),
        )
        .unwrap();

        let a = BigUint::from_bytes_be(&srp.a_pub);
        let u = BigUint::from_bytes_be(&sha512(&[&pad384(&a), &pad384(&b_pub)]));
        assert_eq!(
            srp.key,
            sha512(&[&(&a * v.modpow(&u, &n)).modpow(&b, &n).to_bytes_be()])
        );
        assert_eq!(srp.m2, sha512(&[&srp.a_pub, &srp.m1, &srp.key]));
        let wrong = srp_client(
            "Pair-Setup",
            "0000",
            &salt,
            &b_pub.to_bytes_be(),
            &random::<32>(),
        )
        .unwrap();
        assert_ne!(srp.m1, wrong.m1);
    }

    /// Vector produced by srptools (the SRP library pyatv pairs HomePods with), so a shared mistake can't hide.
    #[test]
    fn srp_matches_srptools() {
        let h = |s: &str| {
            BigUint::parse_bytes(s.as_bytes(), 16)
                .unwrap()
                .to_bytes_be()
        };
        let srp = srp_client(
            "Pair-Setup",
            "3939",
            &h("a53bddcff65e99d3"),
            &h(B_SRPTOOLS),
            &h("bd8d6e14e373c1abc6145a8f09bcf03178a844732253d34a701b4f4d3fe29224"),
        )
        .unwrap();
        assert_eq!(
            srp.key.to_vec(),
            h(
                "1f7109e9bab061ade006c0756e32d428f6c0dbc5c42fc356aec9b8341d256972d4b5e96a4e8a044edbf3aa0719be098cf4e301fef88291d7b95e7da69e8ce4aa"
            )
        );
        assert_eq!(
            srp.m1.to_vec(),
            h(
                "1df93029e1ded9a632ccb7a08018f470aa9a585b5231d3aecb8f0e25f92a79c14a00a18a263bd0dafcf716f86f97089ea19ad75c3c9bff42bcb3285de5f09aa7"
            )
        );
        assert_eq!(
            srp.m2.to_vec(),
            h(
                "db13b5530af105187207c58c09cd4fd9532ba76842643f45806f47d18fa56ab7bc82ef415c1fc82ed2e2dc82642b0e5874634eb573f1f46f69d95488293ec61d"
            )
        );
    }

    const B_SRPTOOLS: &str = "18c9c5d09a3b5e26fc1c745f267841cb7f6f109d4bcc628b281b249539772b34d8398bdb3718f89773f65c15ed02f7f98abe5f76941ac80c57d39a6efc8393185aabfefb5cd3698010b5dba311bf32689f190be15cda358c35c451bc80cefb80dd61b45ee06c311181e1a95b5c7c6e9a043b511fa71859719df0c025a1326c1b081e508641055ebcbaa29f1a0b2b6ce382d54101eb590e8edeeccb4a196502619548b36078967052b675c7b539eb97550820a3b11dacde2b975cc95ca18a8be3d31814c1e2e0a6542926933671036ecf6fd935b13fbf231392438529e2640acc0268fd26dcf3d6913a83ccd74b5da15c3893c9d5c15c0980df4c7d0251f7052359878a7f14b99a1d1b9baf608876474bcfa4aabd3bf646865e1794d45f25458cea85ad94dfc4d32611c4c1afe8be2a31bdd7d427cf5373792919701afb4456520ccb5fc6ee5c63b7cea093035632554295cb5555391e42f40a0eb9d133e0db47f56382097b6b7bbca35555d48c5d4ce7e791f21fe3b2219a422d6ab22da53e85";

    #[test]
    fn read_msg_rejects_huge_bodies() {
        let ok = read_msg(&mut &b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nContent-Length: 2\r\n\r\nhi"[..])
            .unwrap();
        assert_eq!(
            ok,
            ("RTSP/1.0 200 OK".into(), Some("4".into()), b"hi".to_vec())
        );
        assert!(
            read_msg(&mut &b"RTSP/1.0 200 OK\r\nContent-Length: 99999999999\r\n\r\n"[..]).is_err()
        );
    }

    fn serve_once(bytes: Vec<u8>) -> TcpStream {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || l.accept().unwrap().0.write_all(&bytes));
        TcpStream::connect(addr).unwrap()
    }

    #[test]
    fn hap_channel_roundtrip() {
        let key: [u8; 32] = random();
        let msg = vec![42u8; 2500]; // spans three 1024-byte frames
        let wire = Cipher::new(key).seal(&msg);
        assert_eq!(wire.len(), msg.len() + 3 * 18);
        let mut got = vec![0u8; msg.len()];
        Decrypting::new(serve_once(wire.clone()), Some(Cipher::new(key)))
            .read_exact(&mut got)
            .unwrap();
        assert_eq!(got, msg);
        // A flipped bit must be rejected, not silently decrypted.
        let mut bad = wire;
        bad[5] ^= 1;
        assert!(
            Decrypting::new(serve_once(bad), Some(Cipher::new(key)))
                .read_exact(&mut got)
                .is_err()
        );
    }

    #[test]
    fn audio_packet_layout() {
        let aead = ChaCha20Poly1305::new(&random::<32>().into());
        let pkt = audio_packet(&aead, true, 0x1234, 7, 9, b"hello");
        assert_eq!(&pkt[..4], &[0x80, 0xe0, 0x12, 0x34]);
        assert_eq!(pkt.len(), 12 + 5 + 16 + 8);
        assert_eq!(&pkt[pkt.len() - 8..], &[0x34, 0x12, 0, 0, 0, 0, 0, 0]);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&pkt[pkt.len() - 8..]);
        let plain = aead
            .decrypt(
                &nonce.into(),
                Payload {
                    msg: &pkt[12..pkt.len() - 8],
                    aad: &pkt[4..12],
                },
            )
            .unwrap();
        assert_eq!(plain, b"hello");
    }

    #[test]
    fn resend_serves_requested_packets() {
        let (ctrl, audio, sink) = (
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        );
        sink.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let history = Arc::new(Mutex::new(vec![Vec::new(); HISTORY]));
        for seq in [65534u16, 65535, 0] {
            history.lock().unwrap()[seq as usize % HISTORY] =
                vec![0x80, 0x60, (seq >> 8) as u8, seq as u8, 1];
        }
        let (resent, ctrl_addr, to) = (
            Arc::new(AtomicU32::new(0)),
            ctrl.local_addr().unwrap(),
            sink.local_addr().unwrap(),
        );
        let r = resent.clone();
        std::thread::spawn(move || serve_resends(ctrl, audio, to, history, r, Arc::default()));
        // Ask for 65535 and 0 (wrapping) plus 1, which was never sent.
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(&[0x80, 0xd5, 0, 1, 0xff, 0xff, 0, 3], ctrl_addr)
            .unwrap();
        let mut b = [0u8; 16];
        for want in [[0xff, 0xff], [0, 0]] {
            let n = sink.recv(&mut b).unwrap();
            assert_eq!(&b[2..4], &want);
            assert_eq!(n, 5);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(resent.load(Relaxed), 2);
    }
}
