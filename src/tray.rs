//! Windows tray icon: pick an AirPlay device and a latency from the menu; streaming runs in a background thread.

use super::*;
use std::os::windows::io::IntoRawHandle;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};
use windows_sys::Win32::System::Console::{
    FreeConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, SetTimer, TranslateMessage,
};

const LATENCIES: [u32; 6] = [40, 60, 80, 120, 200, 400];

struct Session {
    addr: SocketAddr,
    ctl: Arc<Ctl>,
    thread: JoinHandle<()>,
}

impl Session {
    fn start(addr: SocketAddr, latency_ms: u32) -> Self {
        let ctl = Arc::<Ctl>::default();
        let c = ctl.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = play(addr, latency_ms, &c) {
                c.say(format!("error: {e}"));
            }
        });
        Session { addr, ctl, thread }
    }

    fn running(&self) -> bool {
        !self.thread.is_finished()
    }

    /// Stop and wait, so the HomePod gets its TEARDOWN before anything new starts.
    fn end(self) {
        self.ctl.stop.store(true, Relaxed);
        let _ = self.thread.join();
    }
}

/// A filled circle: grey when idle, green while streaming.
fn icon(streaming: bool) -> Icon {
    let (r, g, b) = if streaming {
        (52, 199, 89)
    } else {
        (160, 160, 160)
    };
    let px = (0..32 * 32).flat_map(|i| {
        let (x, y) = ((i % 32) as f32 - 15.5, (i / 32) as f32 - 15.5);
        let alpha = ((14.0 - (x * x + y * y).sqrt()).clamp(0.0, 1.0) * 255.0) as u8; // anti-aliased edge
        [r, g, b, alpha]
    });
    Icon::from_rgba(px.collect(), 32, 32).expect("32x32 RGBA icon")
}

fn menu(
    devices: &[(String, SocketAddr)],
    searching: bool,
    current: Option<SocketAddr>,
    latency_ms: u32,
    status: &str,
    has_log: bool,
) -> Res<Menu> {
    let m = Menu::new();
    m.append(&MenuItem::new(status, false, None))?;
    m.append(&PredefinedMenuItem::separator())?;
    for (i, (name, addr)) in devices.iter().enumerate() {
        m.append(&CheckMenuItem::with_id(
            format!("dev:{i}"),
            name,
            true,
            current == Some(*addr),
            None,
        ))?;
    }
    if devices.is_empty() {
        m.append(&MenuItem::new(
            if searching {
                "Searching…"
            } else {
                "No AirPlay devices found"
            },
            false,
            None,
        ))?;
    }
    m.append(&MenuItem::with_id(
        "search",
        "Search again",
        !searching,
        None,
    ))?;
    m.append(&PredefinedMenuItem::separator())?;
    let lat = Submenu::new(format!("Latency: {latency_ms} ms"), true);
    for ms in LATENCIES {
        lat.append(&CheckMenuItem::with_id(
            format!("lat:{ms}"),
            format!("{ms} ms"),
            true,
            ms == latency_ms,
            None,
        ))?;
    }
    m.append(&lat)?;
    m.append(&MenuItem::with_id("stop", "Stop", current.is_some(), None))?;
    m.append(&MenuItem::with_id("log", "Open log", has_log, None))?;
    m.append(&MenuItem::with_id("quit", "Quit", true, None))?;
    Ok(m)
}

/// Send everything podbeam prints (status, errors, panics) to `%LOCALAPPDATA%\podbeam\podbeam.log`.
/// A log over 1 MB is moved to `podbeam.log.1` first, so the previous run survives one restart.
fn log_to_file() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("podbeam");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("podbeam.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&path, dir.join("podbeam.log.1"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    // SAFETY: a valid handle that is never closed; std looks the std handles up on every write.
    unsafe {
        let handle = file.into_raw_handle();
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    Some(path)
}

pub fn run(mut latency_ms: u32) -> Res<()> {
    unsafe { FreeConsole() }; // the tray needs no console window
    let log_path = log_to_file();
    log(format!("podbeam {} started", env!("CARGO_PKG_VERSION")));
    let (found_tx, found) = mpsc::channel();
    let search = move || {
        let tx = found_tx.clone();
        std::thread::spawn(move || tx.send(discover().unwrap_or_default()));
    };
    search();

    let (mut devices, mut searching, mut session) = (Vec::new(), true, None::<Session>);
    let tray = TrayIconBuilder::new()
        .with_tooltip("podbeam")
        .with_icon(icon(false))
        .build()?;
    let (mut shown, mut dirty) = (None, true);
    // Wake the message loop twice a second so status changes from the streaming thread show up.
    unsafe { SetTimer(std::ptr::null_mut(), 0, 500, None) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(list) = found.try_recv() {
            (devices, searching, dirty) = (list, false, true);
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id.0.as_str();
            let restart = |s: Option<Session>, addr: SocketAddr, ms| {
                if let Some(s) = s {
                    s.end();
                }
                Some(Session::start(addr, ms))
            };
            if let Some(i) = id
                .strip_prefix("dev:")
                .and_then(|i| i.parse::<usize>().ok())
                .filter(|&i| i < devices.len())
            {
                session = restart(session.take(), devices[i].1, latency_ms);
            } else if let Some(ms) = id.strip_prefix("lat:").and_then(|m| m.parse().ok()) {
                latency_ms = ms;
                // The HomePod's latency is fixed per session, so a change means reconnecting.
                if let Some(addr) = session.as_ref().filter(|s| s.running()).map(|s| s.addr) {
                    session = restart(session.take(), addr, ms);
                }
            } else if id == "log" {
                // Explorer opens the file with whatever the user has set for .log files.
                if let Some(p) = &log_path {
                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                }
            } else if id == "stop" || id == "quit" {
                if let Some(s) = session.take() {
                    s.end();
                }
                if id == "quit" {
                    return Ok(());
                }
            } else if id == "search" {
                (searching, devices) = (true, Vec::new());
                search();
            }
            dirty = true;
        }

        let running = session.as_ref().is_some_and(Session::running);
        let status = match &session {
            Some(s) => s.ctl.status.lock().unwrap().clone(),
            None => "Not streaming".into(),
        };
        let current = session.as_ref().filter(|_| running).map(|s| s.addr);
        let state = (status.clone(), running, current, latency_ms);
        if dirty || shown.as_ref() != Some(&state) {
            tray.set_menu(Some(Box::new(menu(
                &devices,
                searching,
                current,
                latency_ms,
                &status,
                log_path.is_some(),
            )?)));
            tray.set_tooltip(Some(format!("podbeam: {status}")))?;
            tray.set_icon(Some(icon(running)))?;
            (shown, dirty) = (Some(state), false);
        }
    }
    Ok(())
}
