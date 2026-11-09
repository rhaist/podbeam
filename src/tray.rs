//! Tray app, shared by Windows and Linux: discovery, streaming sessions and what the menu shows.
//! The platform modules only draw the menu and icon and hand clicks back as an `Action`.

use super::*;
use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(windows)]
pub use windows::run;

const LATENCIES: [u32; 4] = LADDER_MS;

/// Identifies the tray to the system so that only one runs per user: a D-Bus name on Linux, a
/// mutex name on Windows (reverse DNS of the project's home, as both conventions ask).
const INSTANCE_ID: &str = "io.github.rhaist.podbeam";
const ALREADY_RUNNING: &str = "podbeam is already running in the tray"; // the buffer grows through these by itself too

/// A menu click.
#[derive(Clone, Copy)]
enum Action {
    Device(usize), // index into `View::devices`
    Latency(u32),
    Search,
    Stop,
    OpenLog,
    Quit,
}

/// Everything the menu, tooltip and icon show. Platforms redraw when it changes.
#[derive(Clone, PartialEq)]
struct View {
    status: String, // the menu header, also the tooltip
    devices: Vec<(String, SocketAddr)>,
    searching: bool,
    current: Option<SocketAddr>, // the speaker being streamed to
    latency_ms: u32,
    has_log: bool,
}

struct Session {
    addr: SocketAddr,
    name: String, // as discovered, for the menu header
    ctl: Arc<Ctl>,
    thread: JoinHandle<()>,
}

impl Session {
    /// The new session first waits for `previous` (a stopping session's thread) to send its
    /// TEARDOWN, so the HomePod never sees two sessions; waiting here keeps the menu responsive.
    fn start(
        addr: SocketAddr,
        name: String,
        latency_ms: u32,
        previous: Option<JoinHandle<()>>,
    ) -> Self {
        let ctl = Arc::<Ctl>::default();
        let c = ctl.clone();
        let thread = std::thread::spawn(move || {
            if let Some(p) = previous {
                let _ = p.join();
            }
            if let Err(e) = play(addr, latency_ms, &c) {
                c.say(format!("error: {e}"));
            }
        });
        Session {
            addr,
            name,
            ctl,
            thread,
        }
    }

    fn running(&self) -> bool {
        !self.thread.is_finished()
    }

    /// One short line for the menu header and tooltip. The full messages, statistics included, are
    /// in the log ("Open log").
    fn headline(&self) -> String {
        let status = self.ctl.status.lock().unwrap().clone();
        if let Some(e) = status.strip_prefix("error: ") {
            let short: String = e.chars().take(60).collect();
            let more = if short.len() < e.len() { "…" } else { "" };
            format!("Error: {short}{more}")
        } else if !self.running() {
            "Not streaming".into()
        } else if status.starts_with("streaming") {
            format!("Streaming to {}", self.name)
        } else {
            format!("Connecting to {}…", self.name)
        }
    }

    /// Ask the session to end without waiting for it. The returned thread finishes once TEARDOWN
    /// is sent (up to ~6 s if the HomePod stopped answering); join it before starting another
    /// session or exiting.
    fn stop(self) -> JoinHandle<()> {
        self.ctl.stop.store(true, Relaxed);
        self.thread
    }
}

/// The tray's state. Only ever used from one thread; every call returns quickly.
struct App {
    latency_ms: u32,
    devices: Vec<(String, SocketAddr)>,
    searching: bool,
    session: Option<Session>,
    stopping: Option<JoinHandle<()>>, // a stopped session still sending its TEARDOWN
    speakers_off: Option<VirtualSink>, // kept across a device or latency switch
    found_tx: mpsc::Sender<Vec<(String, SocketAddr)>>,
    found: mpsc::Receiver<Vec<(String, SocketAddr)>>,
    log_path: Option<PathBuf>,
}

impl App {
    fn new(latency_ms: u32, log_path: Option<PathBuf>) -> Self {
        let (found_tx, found) = mpsc::channel();
        let mut app = App {
            latency_ms,
            devices: Vec::new(),
            searching: false,
            session: None,
            stopping: None,
            speakers_off: None,
            found_tx,
            found,
            log_path,
        };
        app.search();
        app
    }

    fn search(&mut self) {
        (self.searching, self.devices) = (true, Vec::new());
        let tx = self.found_tx.clone();
        std::thread::spawn(move || tx.send(discover().unwrap_or_default()));
    }

    /// Pick up the result of a device search. Platforms call this a few times a second.
    fn poll(&mut self) {
        if let Ok(list) = self.found.try_recv() {
            (self.devices, self.searching) = (list, false);
        }
        // A session that ended on its own (HomePod lost): give the PC its speakers back.
        if !self.session.as_ref().is_some_and(Session::running) {
            self.speakers_off = None;
        }
        // The buffer grew on a stalling network: show it, report it to video players, and start
        // the next session there (in memory only).
        if let Some(s) = self.session.as_ref().filter(|s| s.running()) {
            let ms = s.ctl.latency_ms.load(Relaxed);
            if ms > self.latency_ms {
                self.latency_ms = ms;
                if let Some(sink) = &mut self.speakers_off {
                    sink.set_latency(ms);
                }
            }
        }
    }

    fn view(&self) -> View {
        let running = self.session.as_ref().filter(|s| s.running());
        View {
            status: self
                .session
                .as_ref()
                .map_or("Not streaming".into(), Session::headline),
            devices: self.devices.clone(),
            searching: self.searching,
            current: running.map(|s| s.addr),
            latency_ms: self.latency_ms,
            has_log: self.log_path.is_some(),
        }
    }

    /// Act on a click. Quit is up to the platform: it hides the icon, then calls `shutdown`.
    fn handle(&mut self, action: Action) {
        // At most one of the two is set: Stop moves the session to `stopping`, start takes it.
        let restart = |app: &mut Self, addr, name, ms| {
            let previous = app
                .session
                .take()
                .map(Session::stop)
                .or(app.stopping.take());
            match &mut app.speakers_off {
                Some(sink) => sink.set_latency(ms),
                None => app.speakers_off = VirtualSink::new(ms), // before the session picks its capture device
            }
            app.session = Some(Session::start(addr, name, ms, previous));
        };
        match action {
            Action::Device(i) if i < self.devices.len() => {
                let (name, addr) = self.devices[i].clone();
                restart(self, addr, name, self.latency_ms);
            }
            Action::Device(_) | Action::Quit => {}
            Action::Latency(ms) => {
                self.latency_ms = ms;
                // The HomePod's latency is fixed per session, so a change means reconnecting.
                if let Some((addr, name)) = self
                    .session
                    .as_ref()
                    .filter(|s| s.running())
                    .map(|s| (s.addr, s.name.clone()))
                {
                    restart(self, addr, name, ms);
                }
            }
            Action::Search => self.search(),
            Action::Stop => {
                if let Some(s) = self.session.take() {
                    self.stopping = Some(s.stop());
                }
                self.speakers_off = None;
            }
            Action::OpenLog => {
                // Opens the file with whatever the user has set for .log files.
                let opener = if cfg!(windows) {
                    "explorer"
                } else {
                    "xdg-open"
                };
                if let Some(p) = &self.log_path {
                    let _ = std::process::Command::new(opener).arg(p).spawn();
                }
            }
        }
    }

    /// Stop streaming and wait until the HomePod got its TEARDOWN.
    fn shutdown(mut self) {
        let threads = [self.session.take().map(Session::stop), self.stopping.take()];
        for t in threads.into_iter().flatten() {
            let _ = t.join();
        }
        self.speakers_off = None;
    }
}

/// A filled circle `size` pixels wide as RGBA: grey when idle, green while streaming.
fn icon_rgba(streaming: bool, size: u32) -> Vec<u8> {
    let (r, g, b) = if streaming {
        (52, 199, 89)
    } else {
        (160, 160, 160)
    };
    let (mid, radius) = (size as f32 / 2.0 - 0.5, size as f32 * 14.0 / 32.0);
    (0..size * size)
        .flat_map(|i| {
            let (x, y) = ((i % size) as f32 - mid, (i / size) as f32 - mid);
            let alpha = ((radius - (x * x + y * y).sqrt()).clamp(0.0, 1.0) * 255.0) as u8; // anti-aliased edge
            [r, g, b, alpha]
        })
        .collect()
}

/// Where the tray keeps its log: `%LOCALAPPDATA%\podbeam` on Windows, the XDG state directory
/// (`~/.local/state/podbeam`) on Linux.
fn log_path() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|d| d.is_absolute())
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?
    };
    Some(dir.join("podbeam").join("podbeam.log"))
}

/// Open the log for appending. A log over 1 MB is moved to `podbeam.log.1` first, so the previous
/// run survives one restart.
fn open_log() -> Option<(PathBuf, File)> {
    let path = log_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some((path, file))
}
