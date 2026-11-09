//! Linux tray icon over the StatusNotifierItem D-Bus protocol: Cinnamon, KDE, XFCE, waybar
//! (Hyprland, Sway) and GNOME with the AppIndicator extension. No GTK needed.

use super::*;
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip};
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Set in the detached copy that runs the tray: "terminal" or "launcher", where it was started.
const DETACHED: &str = "PODBEAM_TRAY_DETACHED";

struct Tray {
    app: Option<App>, // taken out on Quit
    quit: bool,
}

impl Tray {
    fn view(&self) -> Option<View> {
        self.app.as_ref().map(App::view)
    }

    fn click(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true, // the main loop shuts down; never block D-Bus here
            a => {
                if let Some(app) = &mut self.app {
                    app.handle(a)
                }
            }
        }
    }
}

fn item(label: impl Into<String>, enabled: bool, action: Option<Action>) -> MenuItem<Tray> {
    StandardItem {
        label: label.into(),
        enabled,
        activate: match action {
            Some(a) => Box::new(move |t: &mut Tray| t.click(a)),
            None => Box::new(|_| {}),
        },
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true; // there is no window: a left click opens the menu too

    fn id(&self) -> String {
        "podbeam".into()
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log(format!("waiting for a system tray ({reason:?})"));
        true // keep running; the icon appears once the panel's tray is up
    }

    fn title(&self) -> String {
        "podbeam".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let streaming = self.view().is_some_and(|v| v.current.is_some());
        [16, 22, 32, 48]
            .into_iter()
            .map(|size| {
                let mut data = icon_rgba(streaming, size);
                data.as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .for_each(|px| px.rotate_right(1)); // RGBA -> ARGB
                Icon {
                    width: size as i32,
                    height: size as i32,
                    data,
                }
            })
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!(
                "podbeam: {}",
                self.view().map_or_else(String::new, |v| v.status)
            ),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let Some(v) = self.view() else {
            return vec![];
        };
        let mut m = vec![item(v.status, false, None), MenuItem::Separator];
        for (i, (name, addr)) in v.devices.iter().enumerate() {
            m.push(
                CheckmarkItem {
                    label: name.clone(),
                    checked: v.current == Some(*addr),
                    activate: Box::new(move |t: &mut Tray| t.click(Action::Device(i))),
                    ..Default::default()
                }
                .into(),
            );
        }
        if v.devices.is_empty() {
            let note = if v.searching {
                "Searching…"
            } else {
                "No AirPlay devices found"
            };
            m.push(item(note, false, None));
        }
        m.push(item("Search again", !v.searching, Some(Action::Search)));
        m.push(MenuItem::Separator);
        m.push(
            SubMenu {
                label: format!("Latency: {} ms", v.latency_ms),
                submenu: vec![
                    RadioGroup {
                        selected: LATENCIES
                            .iter()
                            .position(|&ms| ms == v.latency_ms)
                            .unwrap_or(0),
                        select: Box::new(|t: &mut Tray, i| t.click(Action::Latency(LATENCIES[i]))),
                        options: LATENCIES
                            .iter()
                            .map(|ms| RadioItem {
                                label: format!("{ms} ms"),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        m.push(item("Stop", v.current.is_some(), Some(Action::Stop)));
        m.push(item("Open log", v.has_log, Some(Action::OpenLog)));
        m.push(item("Quit", true, Some(Action::Quit)));
        m
    }
}

/// Without a target, podbeam starts a copy of itself in the background that runs the tray, so the
/// terminal (or launcher) gets control back. Its output goes to the log file.
pub fn run(latency_ms: u32) -> Res<()> {
    if let Some(from) = std::env::var_os(DETACHED) {
        return run_tray(latency_ms, from == "terminal");
    }
    let (path, file) = open_log().ok_or("cannot create the log file")?;
    let mut child = Command::new(std::env::current_exe()?)
        .args(["", &latency_ms.to_string()])
        .env(
            DETACHED,
            if io::stdin().is_terminal() {
                "terminal"
            } else {
                "launcher"
            },
        )
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0) // Ctrl-C in this terminal must not reach it
        .spawn()?;
    // A tray that cannot show up (no tray on this desktop) exits right away: report that here,
    // not only in the log.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            // Its last line is usually "<time> error: <why>"; a panic has no such prefix.
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let last = text.lines().last().unwrap_or("");
            let why = last.split_once(" error: ").map_or(last, |(_, w)| w);
            if why == ALREADY_RUNNING {
                log(why); // not a failure: the tray the user wants is there
                return Ok(());
            }
            return Err(format!("the tray did not start: {why}").into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log(format!(
        "podbeam is running in the tray (log: {})",
        path.display()
    ));
    Ok(())
}

/// `from_terminal`: report a missing tray at once. Started from a launcher or autostart, wait for
/// it instead: at login podbeam can start before the panel's tray is up.
fn run_tray(latency_ms: u32, from_terminal: bool) -> Res<()> {
    // One tray per user, the freedesktop way (as GApplication and KDE apps do it): the session bus
    // gives a well-known name to a single process and frees it when that process ends, crash or not.
    let _instance = zbus::blocking::connection::Builder::session()?
        .name(INSTANCE_ID)?
        .replace_existing_names(false)
        .allow_name_replacements(false)
        .build()
        .map_err(|e| match e {
            zbus::Error::NameTaken => ALREADY_RUNNING.to_string(),
            e => format!("D-Bus session bus: {e}"),
        })?;
    log(format!("podbeam {} started", env!("CARGO_PKG_VERSION")));
    let tray = Tray {
        app: Some(App::new(latency_ms, log_path())),
        quit: false,
    };
    let handle = tray
        .assume_sni_available(!from_terminal)
        .spawn()
        .map_err(|e| {
            log(format!("tray: {e}"));
            "no system tray on this desktop. On GNOME, install the AppIndicator extension; \
         or stream without the tray: podbeam \"Living Room\""
        })?;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        // This also refreshes the menu: ksni re-reads it and signals only what changed.
        let quitting = handle.update(|t| {
            if let Some(app) = &mut t.app {
                app.poll();
            }
            if t.quit || INTERRUPTED.load(Relaxed) {
                t.app.take()
            } else {
                None
            }
        });
        match quitting {
            None => return Err("the tray service stopped".into()),
            Some(Some(app)) => {
                // Hide the icon at once, then wait for TEARDOWN so the HomePod isn't left hanging.
                handle.shutdown().wait();
                app.shutdown();
                log("quit");
                return Ok(());
            }
            Some(None) => {}
        }
    }
}
