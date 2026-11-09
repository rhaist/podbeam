//! A fake AirPlay 2 receiver for end-to-end tests: the receiver side of transient pairing, the
//! encrypted RTSP and event channels, and an audio socket that decrypts and decodes what podbeam
//! sends. It runs the real `stream_to` against something that behaves like a HomePod, without one.
//! It knows the transient PIN and decrypts the audio because a real receiver does too; every key it
//! uses is random per test run.

use super::*;
use std::net::TcpListener;

/// How the fake misbehaves, to test podbeam's error paths.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Behaves like a HomePod.
    Normal,
    /// Refuses pairing, like a HomePod with AirPlay restricted to people in the Home.
    RefusePairing,
    /// Answers pairing with a proof that does not match, like an impostor.
    WrongProof,
    /// Hangs up once streaming has started, like a HomePod that rebooted.
    HangUp,
    /// Never answers TEARDOWN but keeps the connection open, like a HomePod that dropped off Wi-Fi.
    IgnoreTeardown,
}

#[derive(Default)]
pub struct Report {
    pub requests: Vec<String>,     // first line of every RTSP request, in order
    pub volumes: Vec<f32>,         // every "volume:" value set
    pub left: Vec<i16>,            // decoded left channel of every audio packet, in sequence order
    pub seq_gaps: u32,             // audio packets missing between first and last
    pub nonces: Vec<[u8; 8]>,      // nonce trailer of every audio packet, in arrival order
    pub syncs: u32,                // sync packets on the control port
    pub sync_rtp: Vec<(u32, u32)>, // the two RTP fields of every sync packet
    pub latency: (u64, u64),       // latencyMin and latencyMax from the stream SETUP
    pub event_answered: bool,      // podbeam acknowledged the event we pushed
}

/// Start the fake on 127.0.0.1; returns its address and the report it fills while podbeam talks to it.
pub fn start(mode: Mode) -> (SocketAddr, Arc<Mutex<Report>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let report = Arc::new(Mutex::new(Report::default()));
    let r = report.clone();
    std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let _ = serve(tcp, mode, r);
    });
    (addr, report)
}

fn reply(
    tcp: &mut TcpStream,
    out: &mut Option<Cipher>,
    cseq: &Option<String>,
    ctype: &str,
    body: &[u8],
) -> Res<()> {
    let cseq = cseq
        .as_deref()
        .map(|c| format!("CSeq: {c}\r\n"))
        .unwrap_or_default();
    let ctype = if body.is_empty() {
        String::new()
    } else {
        format!("Content-Type: {ctype}\r\n")
    };
    let msg = [
        format!(
            "RTSP/1.0 200 OK\r\n{cseq}{ctype}Content-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
        body,
    ]
    .concat();
    match out {
        Some(c) => tcp.write_all(&c.seal(&msg))?,
        None => tcp.write_all(&msg)?,
    }
    Ok(())
}

fn plist_bytes(d: Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &Value::Dictionary(d)).unwrap();
    buf
}

/// Inverse of `alac()`: the samples of one uncompressed ALAC frame.
fn alac_decode(f: &[u8]) -> Vec<i16> {
    let bit = |i: usize| (f[i / 8] >> (7 - i % 8)) & 1;
    let read = |at: usize, bits: usize| (0..bits).fold(0u32, |a, k| a << 1 | bit(at + k) as u32);
    assert_eq!(
        (read(0, 3), read(22, 1)),
        (1, 1),
        "not a stereo escape frame"
    );
    (0..FRAMES * 2)
        .map(|k| read(23 + k * 16, 16) as u16 as i16)
        .collect()
}

fn serve(mut tcp: TcpStream, mode: Mode, report: Arc<Mutex<Report>>) -> Res<()> {
    let (n, g) = srp_group();
    let mut rd = Decrypting::new(tcp.try_clone()?, None);
    let mut out: Option<Cipher> = None;
    let log = |line: &str| report.lock().unwrap().requests.push(line.to_string());

    // Pair-setup M1 -> M2: salt and our public key B = k*v + g^b.
    let (first, cseq, m1) = read_msg(&mut rd)?;
    log(&first);
    assert_eq!(
        (tlv_get(&m1, 0), tlv_get(&m1, 6), tlv_get(&m1, 0x13)),
        (vec![0], vec![1], vec![0x10]),
        "not a transient M1"
    );
    if mode == Mode::RefusePairing {
        return reply(
            &mut tcp,
            &mut out,
            &cseq,
            "application/octet-stream",
            &tlv(&[(6, &[2]), (7, &[2])]),
        );
    }
    let salt: [u8; 16] = random();
    let x = BigUint::from_bytes_be(&sha512(&[&salt, &sha512(&[b"Pair-Setup:3939"])]));
    let v = g.modpow(&x, &n);
    let k = BigUint::from_bytes_be(&sha512(&[&n.to_bytes_be(), &pad384(&g)]));
    let b = BigUint::from_bytes_be(&random::<32>());
    let b_pub = (&k * &v + g.modpow(&b, &n)) % &n;
    let b_bytes = b_pub.to_bytes_be();
    reply(
        &mut tcp,
        &mut out,
        &cseq,
        "application/octet-stream",
        &tlv(&[(6, &[2]), (2, &salt), (3, &b_bytes)]),
    )?;

    // M3 -> M4: check the client's proof, send ours, derive the session key.
    let (first, cseq, m3) = read_msg(&mut rd)?;
    log(&first);
    let a_bytes = tlv_get(&m3, 3);
    let a = BigUint::from_bytes_be(&a_bytes);
    let u = BigUint::from_bytes_be(&sha512(&[&pad384(&a), &pad384(&b_pub)]));
    let key = sha512(&[&(&a * v.modpow(&u, &n)).modpow(&b, &n).to_bytes_be()]);
    let (hn, hg) = (sha512(&[&n.to_bytes_be()]), sha512(&[&g.to_bytes_be()]));
    let hxor: Vec<u8> = hn.iter().zip(hg).map(|(x, y)| x ^ y).collect();
    let m1_expected = sha512(&[
        &hxor,
        &sha512(&[b"Pair-Setup"]),
        &salt,
        &a_bytes,
        &b_bytes,
        &key,
    ]);
    assert_eq!(
        tlv_get(&m3, 4),
        m1_expected,
        "client proof wrong: podbeam's SRP disagrees with the receiver side"
    );
    let mut proof = sha512(&[&a_bytes, &m1_expected, &key]);
    if mode == Mode::WrongProof {
        proof[0] ^= 1;
    }
    reply(
        &mut tcp,
        &mut out,
        &cseq,
        "application/octet-stream",
        &tlv(&[(6, &[4]), (4, &proof)]),
    )?;
    rd.get_mut().cipher = Some(Cipher::new(hkdf(
        &key,
        "Control-Salt",
        "Control-Write-Encryption-Key",
    )));
    out = Some(Cipher::new(hkdf(
        &key,
        "Control-Salt",
        "Control-Read-Encryption-Key",
    )));

    let events = TcpListener::bind("127.0.0.1:0")?;
    let (data, ctrl) = (
        UdpSocket::bind("127.0.0.1:0")?,
        UdpSocket::bind("127.0.0.1:0")?,
    );
    let (mut shk, mut collecting) = (None, false);
    loop {
        let (first, cseq, body) = match read_msg(&mut rd) {
            Ok(m) => m,
            Err(_) => return Ok(()), // podbeam hung up
        };
        log(&first);
        let method = first.split(' ').next().unwrap_or("");
        if method == "TEARDOWN" && mode == Mode::IgnoreTeardown {
            std::thread::sleep(Duration::from_secs(10));
            return Ok(());
        }
        let resp = match method {
            "SETUP" => {
                let req = plist::from_bytes::<Value>(&body)?
                    .into_dictionary()
                    .unwrap();
                if req.contains_key("timingProtocol") {
                    // Session SETUP: hand out the event port, then push one event once podbeam connects.
                    let (ev, report) = (events.try_clone()?, report.clone());
                    let ev_addr = ev.local_addr()?;
                    std::thread::spawn(move || {
                        let Ok((mut ev_tcp, _)) = ev.accept() else {
                            return;
                        };
                        let mut ev_out =
                            Cipher::new(hkdf(&key, "Events-Salt", "Events-Write-Encryption-Key"));
                        let ev_in =
                            Cipher::new(hkdf(&key, "Events-Salt", "Events-Read-Encryption-Key"));
                        let mut ev_rd = Decrypting::new(ev_tcp.try_clone().unwrap(), Some(ev_in));
                        let _ = ev_tcp.write_all(&ev_out.seal(
                            b"POST /command RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n",
                        ));
                        if let Ok((status, cseq, _)) = read_msg(&mut ev_rd) {
                            report.lock().unwrap().event_answered =
                                status.contains("200") && cseq.as_deref() == Some("1");
                        }
                    });
                    plist_bytes(dict([
                        ("eventPort", int(ev_addr.port())),
                        ("timingPort", int(0u8)),
                    ]))
                } else {
                    let s = req["streams"].as_array().unwrap()[0]
                        .as_dictionary()
                        .unwrap();
                    shk = Some(<[u8; 32]>::try_from(s["shk"].as_data().unwrap()).unwrap());
                    let samples = |k: &str| s[k].as_unsigned_integer().unwrap();
                    report.lock().unwrap().latency = (samples("latencyMin"), samples("latencyMax"));
                    let ports = dict([
                        ("type", int(96u8)),
                        ("dataPort", int(data.local_addr()?.port())),
                        ("controlPort", int(ctrl.local_addr()?.port())),
                    ]);
                    plist_bytes(dict([(
                        "streams",
                        Value::Array(vec![Value::Dictionary(ports)]),
                    )]))
                }
            }
            "SET_PARAMETER" => {
                let text = String::from_utf8_lossy(&body);
                if let Some(v) = text.trim().strip_prefix("volume:") {
                    report.lock().unwrap().volumes.push(v.trim().parse()?);
                }
                vec![]
            }
            _ => vec![], // RECORD, POST /feedback, TEARDOWN
        };
        reply(
            &mut tcp,
            &mut out,
            &cseq,
            "application/x-apple-binary-plist",
            &resp,
        )?;
        if method == "SET_PARAMETER" {
            // Streaming has started: collect audio in the background.
            if mode == Mode::HangUp {
                std::thread::sleep(Duration::from_millis(300));
                return Ok(()); // dropping the socket closes it
            }
            if !collecting {
                collecting = true;
                let (data, ctrl, shk) = (
                    data.try_clone()?,
                    ctrl.try_clone()?,
                    shk.expect("stream SETUP came first"),
                );
                let report = report.clone();
                std::thread::spawn(move || collect_audio(data, ctrl, shk, &report));
            }
        }
        if method == "TEARDOWN" {
            return Ok(());
        }
    }
}

fn collect_audio(data: UdpSocket, ctrl: UdpSocket, shk: [u8; 32], report: &Mutex<Report>) {
    let aead = ChaCha20Poly1305::new(&shk.into());
    let _ = data.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = ctrl.set_nonblocking(true);
    let mut packets: Vec<(u16, Vec<i16>)> = Vec::new();
    let mut b = [0u8; 2048];
    while let Ok(len) = data.recv(&mut b) {
        let pkt = &b[..len];
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&pkt[len - 8..]);
        let plain = aead
            .decrypt(
                &nonce.into(),
                Payload {
                    msg: &pkt[12..len - 8],
                    aad: &pkt[4..12],
                },
            )
            .expect("audio packet failed authentication");
        packets.push((u16::from_be_bytes([pkt[2], pkt[3]]), alac_decode(&plain)));
        report
            .lock()
            .unwrap()
            .nonces
            .push(pkt[len - 8..].try_into().unwrap());
        while let Ok(n) = ctrl.recv(&mut b) {
            if n == 20 && b[1] == 0xd4 {
                let field = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
                let mut r = report.lock().unwrap();
                r.syncs += 1;
                r.sync_rtp.push((field(4), field(16)));
            }
        }
    }
    let mut r = report.lock().unwrap();
    for w in packets.windows(2) {
        r.seq_gaps += w[1].0.wrapping_sub(w[0].0).wrapping_sub(1) as u32;
    }
    for (_, samples) in packets {
        r.left.extend(samples.iter().step_by(2));
    }
}
