//! Windows tray icon: a Win32 message loop around the shared `App`.

use super::*;
use std::os::windows::io::IntoRawHandle;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Console::{
    FreeConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, SetTimer, TranslateMessage,
};

fn icon(streaming: bool) -> Icon {
    Icon::from_rgba(icon_rgba(streaming, 32), 32, 32).expect("32x32 RGBA icon")
}

fn menu(v: &View) -> Res<Menu> {
    let m = Menu::new();
    m.append(&MenuItem::new(&v.status, false, None))?;
    m.append(&PredefinedMenuItem::separator())?;
    for (i, (name, addr)) in v.devices.iter().enumerate() {
        m.append(&CheckMenuItem::with_id(
            format!("dev:{i}"),
            name,
            true,
            v.current == Some(*addr),
            None,
        ))?;
    }
    if v.devices.is_empty() {
        m.append(&MenuItem::new(
            if v.searching {
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
        !v.searching,
        None,
    ))?;
    m.append(&PredefinedMenuItem::separator())?;
    let lat = Submenu::new(format!("Latency: {} ms", v.latency_ms), true);
    for ms in LATENCIES {
        lat.append(&CheckMenuItem::with_id(
            format!("lat:{ms}"),
            format!("{ms} ms"),
            true,
            ms == v.latency_ms,
            None,
        ))?;
    }
    m.append(&lat)?;
    m.append(&MenuItem::with_id(
        "stop",
        "Stop",
        v.current.is_some(),
        None,
    ))?;
    m.append(&MenuItem::with_id("log", "Open log", v.has_log, None))?;
    m.append(&MenuItem::with_id("quit", "Quit", true, None))?;
    Ok(m)
}

/// One tray per user session: a named mutex, which Windows releases when the process ends.
fn already_running() -> bool {
    let name: Vec<u16> = format!("Local\\{INSTANCE_ID}\0").encode_utf16().collect();
    // SAFETY: a NUL-terminated name; the handle is never closed, so the mutex lives until we exit.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    !handle.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS
}

fn action(id: &str) -> Option<Action> {
    Some(match id {
        "search" => Action::Search,
        "stop" => Action::Stop,
        "log" => Action::OpenLog,
        "quit" => Action::Quit,
        _ => match id.split_once(':')? {
            ("dev", i) => Action::Device(i.parse().ok()?),
            ("lat", ms) => Action::Latency(ms.parse().ok()?),
            _ => return None,
        },
    })
}

/// Send everything podbeam prints (status, errors, panics) to the log file.
fn log_to_file() -> Option<PathBuf> {
    let (path, file) = open_log()?;
    // SAFETY: a valid handle that is never closed; std looks the std handles up on every write.
    unsafe {
        let handle = file.into_raw_handle();
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    Some(path)
}

pub fn run(latency_ms: u32) -> Res<()> {
    unsafe { FreeConsole() }; // the tray needs no console window
    let log_path = log_to_file();
    if already_running() {
        log(ALREADY_RUNNING);
        return Ok(());
    }
    log(format!("podbeam {} started", env!("CARGO_PKG_VERSION")));
    let mut app = App::new(latency_ms, log_path);
    let tray = TrayIconBuilder::new()
        .with_tooltip("podbeam")
        .with_icon(icon(false))
        .build()?;
    let mut shown = None;
    // Wake the message loop twice a second so status changes from the streaming thread show up.
    unsafe { SetTimer(std::ptr::null_mut(), 0, 500, None) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        app.poll();
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match action(ev.id.0.as_str()) {
                Some(Action::Quit) => {
                    // Hide the icon at once, then wait for TEARDOWN so the HomePod isn't left hanging.
                    drop(tray);
                    app.shutdown();
                    return Ok(());
                }
                Some(a) => app.handle(a),
                None => {}
            }
        }
        let view = app.view();
        if shown.as_ref() != Some(&view) {
            tray.set_menu(Some(Box::new(menu(&view)?)));
            tray.set_tooltip(Some(format!("podbeam: {}", view.status)))?;
            tray.set_icon(Some(icon(view.current.is_some())))?;
            shown = Some(view);
        }
    }
    Ok(())
}
