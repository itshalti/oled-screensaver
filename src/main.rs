//! OLED-friendly screensaver: a wlr-layer-shell overlay on one output, rendered with wgpu.
//! Any key / pointer input (after a short grace period) exits. `daemon` starts it when the
//! compositor reports the user idle (ext-idle-notify); `install` registers that as a systemd user service.

mod config;
mod pipes;
mod render;
mod text;

use std::{
    error::Error,
    os::fd::AsFd,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region::WlRegion,
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// Input right after the surface appears is ignored (pointer jitter, key release).
const GRACE: Duration = Duration::from_secs(1);

struct State {
    outputs: Vec<(WlOutput, Option<String>)>,
    configured: bool,
    new_size: Option<(u32, u32)>,
    exit: bool,
    armed_at: Instant,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    /// Pointer position when it entered our surface; motion is measured from here.
    anchor: Option<(f64, f64)>,
}

impl State {
    fn input(&mut self, what: &str) {
        if Instant::now() >= self.armed_at && !self.exit {
            eprintln!("closing on input: {what}");
            self.exit = true;
        }
    }

    /// Pointer at (x, y): `moved` = motion event, otherwise enter. Hyprland sends motion at the
    /// same spot when surfaces under a resting cursor change (e.g. a "now playing" popup), so only
    /// a real move of more than 2 px from where the pointer entered counts as input.
    fn pointer_at(&mut self, x: f64, y: f64, moved: bool) {
        match self.anchor {
            Some((ax, ay)) if moved => {
                if (x - ax).hypot(y - ay) > 2.0 {
                    self.input(&format!("pointer moved to {x:.0},{y:.0}"));
                }
            }
            _ => self.anchor = Some((x, y)),
        }
    }
}

const NAME: &str = "ss-for-oled";

fn pidfile() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(Into::into).unwrap_or_else(std::env::temp_dir).join(format!("{NAME}.pid"))
}

/// PID of a running screensaver, if the pidfile points at a live `ss-for-oled` process.
fn running_pid() -> Option<i32> {
    pid_from(&pidfile())
}

fn pid_from(file: &std::path::Path) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(file).ok()?.trim().parse().ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (comm.trim() == NAME && pid != std::process::id() as i32).then_some(pid)
}

fn main() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        None | Some("run") => run(),
        Some("start") => {
            if running_pid().is_none() {
                spawn_saver(false)?;
            }
            Ok(())
        }
        Some("stop") => stop(),
        Some("daemon") => daemon(),
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        _ => {
            eprintln!(
                "usage: {NAME} [run|start|stop|install|uninstall|daemon]
  run        show the screensaver now (default)
  start      show it in the background
  stop       close it
  install    start it automatically when idle (systemd user service) and create the config
  uninstall  remove the service (the config is kept)
  daemon     the idle watcher the service runs
config: {}",
                config::path().display()
            );
            std::process::exit(2);
        }
    }
}

/// Launches `ss-for-oled run` in its own process group, so it outlives the launching shell.
fn spawn_saver(log: bool) -> Result<std::process::Child, Box<dyn Error>> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let err = if log { Stdio::inherit() } else { Stdio::null() };
    Ok(Command::new(std::env::current_exe()?).arg("run").stdin(Stdio::null()).stdout(Stdio::null()).stderr(err).process_group(0).spawn()?)
}

fn stop() -> Result<(), Box<dyn Error>> {
    if let Some(pid) = running_pid().and_then(rustix::process::Pid::from_raw) {
        rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    }
    Ok(())
}

/// Sleeps until a Wayland event arrives or `timeout` passes, then reads the events.
fn wait<S>(queue: &mut EventQueue<S>, timeout: Duration) -> Result<(), Box<dyn Error>> {
    let Some(guard) = queue.prepare_read() else { return Ok(()) };
    let ts = rustix::event::Timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
    let fd = guard.connection_fd();
    let fd = fd.as_fd();
    let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
    if rustix::event::poll(&mut fds, Some(&ts)).unwrap_or(0) > 0 {
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    std::fs::write(pidfile(), std::process::id().to_string())?;
    let cfg = config::load();
    // env vars override the config, handy for trying things out
    let want = std::env::var("SCREENSAVER_OUTPUT").unwrap_or(cfg.output);
    // 0 (default) = one frame per monitor refresh (vsync-paced: smoothest, no judder)
    let fps: f64 = std::env::var("SCREENSAVER_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(cfg.fps);
    let modes = cfg.modes.iter().filter_map(|m| render::mode_index(m)).collect();

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State {
        outputs: Vec::new(),
        configured: false,
        new_size: None,
        exit: false,
        armed_at: Instant::now() + GRACE,
        pointer: None,
        keyboard: None,
        anchor: None,
    };

    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
    let _seat: WlSeat = globals.bind(&qh, 1..=9, ())?;
    for g in globals.contents().clone_list() {
        if g.interface == WlOutput::interface().name && g.version >= 4 {
            let idx = state.outputs.len();
            let out = globals.registry().bind::<WlOutput, _, _>(g.name, 4, &qh, idx);
            state.outputs.push((out, None));
        }
    }
    queue.roundtrip(&mut state)?; // receive output names + seat capabilities

    let output = state.outputs.iter().find(|(_, n)| n.as_deref() == Some(want.as_str())).map(|(o, _)| o.clone());
    if output.is_none() {
        eprintln!("output {want} not found, letting the compositor choose");
    }

    let surface = compositor.create_surface(&qh, ());
    let layer = shell.get_layer_surface(
        &surface,
        output.as_ref(),
        zwlr_layer_shell_v1::Layer::Overlay,
        "oled-screensaver".into(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();
    while !state.configured {
        queue.blocking_dispatch(&mut state)?;
    }
    let (w, h) = state.new_size.take().unwrap_or((1920, 1080));

    let mut renderer = pollster::block_on(render::Renderer::new(&conn, &surface, w, h, modes, cfg.mode_minutes * 60.0))?;

    let frame = if fps > 0.0 { Duration::from_secs_f64(1.0 / fps) } else { Duration::ZERO };
    let mut next = Instant::now();
    let mut opaque = false;
    loop {
        queue.dispatch_pending(&mut state)?;
        queue.flush()?;
        if state.exit {
            break;
        }
        if let Some((w, h)) = state.new_size.take() {
            renderer.resize(w, h);
        }

        let now = Instant::now();
        if now >= next {
            if renderer.render() && !opaque {
                // Fully black now: tell the compositor, so it stops drawing the desktop underneath.
                // Applies with the next frame's commit.
                let region = compositor.create_region(&qh, ());
                region.add(0, 0, i32::MAX, i32::MAX);
                surface.set_opaque_region(Some(&region));
                region.destroy();
                opaque = true;
            }
            next += frame;
            if next < now {
                next = now + frame; // fell behind: don't try to catch up
            }
        }

        // Sleep until the next frame or a Wayland event, whichever comes first.
        wait(&mut queue, next.saturating_duration_since(Instant::now()))?;
    }
    Ok(())
}

// ---------- idle daemon ----------

/// Last idle event from the compositor: Some(true) = idle, Some(false) = active again.
struct Idle(Option<bool>);

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn daemon() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Idle>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=9, ())?;
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| format!("compositor lacks ext-idle-notify-v1: {e}"))?;

    let path = config::path();
    let mut stamp = mtime(&path);
    let mut cfg = config::load();
    // get_idle_notification (v1) respects idle inhibitors: video players, games, calls
    let mut note = notifier.get_idle_notification(cfg.timeout * 1000, &seat, &qh, ());
    eprintln!("starting after {} s idle (config: {})", cfg.timeout, path.display());
    let mut child: Option<std::process::Child> = None;
    let mut idle = Idle(None);
    loop {
        queue.dispatch_pending(&mut idle)?;
        queue.flush()?;
        match idle.0.take() {
            Some(true) if running_pid().is_none() => {
                eprintln!("idle: starting screensaver");
                child = Some(spawn_saver(true)?);
            }
            Some(false) => {
                if running_pid().is_some() {
                    eprintln!("compositor says active again: stopping screensaver");
                }
                stop()?;
            }
            _ => {}
        }
        if child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some()) {
            child = None; // reaped
        }
        if mtime(&path) != stamp {
            stamp = mtime(&path);
            let new = config::load();
            if new.timeout != cfg.timeout {
                note.destroy();
                note = notifier.get_idle_notification(new.timeout * 1000, &seat, &qh, ());
                eprintln!("config changed: starting after {} s idle", new.timeout);
            }
            cfg = new;
        }
        wait(&mut queue, Duration::from_secs(2))?;
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Idle {
    fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtIdleNotificationV1, ()> for Idle {
    fn event(state: &mut Self, _: &ExtIdleNotificationV1, event: ext_idle_notification_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.0 = Some(true),
            ext_idle_notification_v1::Event::Resumed => state.0 = Some(false),
            _ => {}
        }
    }
}

delegate_noop!(Idle: ignore WlSeat);
delegate_noop!(Idle: ignore ExtIdleNotifierV1);

// ---------- systemd user service ----------

const UNIT: &str = "[Unit]
Description=OLED screensaver: starts ss-for-oled when the session is idle
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=\"{exe}\" daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=graphical-session.target
";

fn unit_path() -> std::path::PathBuf {
    config::config_home().join("systemd/user").join(format!("{NAME}.service"))
}

fn systemctl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let ok = std::process::Command::new("systemctl").arg("--user").args(args).status()?.success();
    if ok { Ok(()) } else { Err(format!("systemctl --user {} failed", args.join(" ")).into()) }
}

fn install() -> Result<(), Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    let unit = unit_path();
    std::fs::create_dir_all(unit.parent().unwrap())?;
    std::fs::write(&unit, UNIT.replace("{exe}", &exe.display().to_string()))?;
    let cfg = config::path();
    if !cfg.exists() {
        std::fs::create_dir_all(cfg.parent().unwrap())?;
        std::fs::write(&cfg, config::TEMPLATE)?;
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", NAME])?;
    println!("installed: {}\nruns: {} daemon\nsettings: {}", unit.display(), exe.display(), cfg.display());
    Ok(())
}

fn uninstall() -> Result<(), Box<dyn Error>> {
    let _ = systemctl(&["disable", "--now", NAME]); // fine if it was never enabled
    match std::fs::remove_file(unit_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    systemctl(&["daemon-reload"])?;
    println!("service removed; settings kept in {}", config::path().display());
    Ok(())
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlOutput, usize> for State {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*idx].1 = Some(name);
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(state: &mut Self, pointer: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                pointer.set_cursor(serial, None, 0, 0); // hide cursor
                state.pointer_at(surface_x, surface_y, false);
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => state.pointer_at(surface_x, surface_y, true),
            wl_pointer::Event::Button { .. } => state.input("mouse button"),
            wl_pointer::Event::Axis { .. } => state.input("scroll"),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { key, .. } = event {
            state.input(&format!("key {key}"));
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(state: &mut Self, surf: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surf.ack_configure(serial);
                state.configured = true;
                state.new_size = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => state.exit = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn armed_state() -> State {
        State {
            outputs: Vec::new(),
            configured: false,
            new_size: None,
            exit: false,
            armed_at: Instant::now(),
            pointer: None,
            keyboard: None,
            anchor: None,
        }
    }

    #[test]
    fn pointer_events_without_real_movement_do_not_close() {
        let mut s = armed_state();
        s.pointer_at(1632.0, 1393.0, false); // Enter after a popup appeared and vanished under the cursor
        s.pointer_at(1632.0, 1393.0, true); // the compositor's synthetic motion at the same spot
        s.pointer_at(1633.0, 1393.5, true); // sensor jitter
        assert!(!s.exit);
        s.pointer_at(1634.5, 1393.0, true); // small steps add up: a real slow move
        assert!(s.exit);
    }

    #[test]
    fn re_entering_elsewhere_is_not_movement() {
        let mut s = armed_state();
        s.pointer_at(10.0, 10.0, false);
        s.pointer_at(500.0, 500.0, false); // Enter carries the new spot, it is not a move by itself
        s.pointer_at(500.0, 500.0, true);
        assert!(!s.exit);
        s.pointer_at(540.0, 500.0, true);
        assert!(s.exit);
    }

    #[test]
    fn stale_or_bad_pidfile_is_not_running() {
        let dir = std::env::temp_dir().join(format!("ss-for-oled-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pid");
        assert_eq!(pid_from(&f), None, "missing file");
        std::fs::write(&f, "garbage").unwrap();
        assert_eq!(pid_from(&f), None, "unparsable");
        std::fs::write(&f, "1").unwrap();
        assert_eq!(pid_from(&f), None, "pid 1 is not ss-for-oled");
        std::fs::write(&f, std::process::id().to_string()).unwrap();
        assert_eq!(pid_from(&f), None, "own pid never counts");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
