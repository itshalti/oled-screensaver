//! OLED-friendly screensaver: a wlr-layer-shell overlay on one output, rendered with wgpu.
//! Any key / pointer input (after a short grace period) exits.

mod render;
mod text;

use std::{
    error::Error,
    os::fd::AsFd,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
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
}

impl State {
    fn input(&mut self) {
        if Instant::now() >= self.armed_at {
            self.exit = true;
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
                use std::os::unix::process::CommandExt;
                use std::process::{Command, Stdio};
                // own process group: survives the launching shell/terminal
                Command::new(std::env::current_exe()?)
                    .arg("run")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()?;
            }
            Ok(())
        }
        Some("stop") => {
            if let Some(pid) = running_pid().and_then(rustix::process::Pid::from_raw) {
                rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
            }
            Ok(())
        }
        _ => {
            eprintln!("usage: {NAME} [start|stop|run]\n  start  launch in the background (idle command)\n  stop   close it (resume command)\n  run    run in the foreground (default)");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    std::fs::write(pidfile(), std::process::id().to_string())?;
    let want = std::env::var("SCREENSAVER_OUTPUT").unwrap_or_else(|_| "DP-1".into());
    // 0 (default) = one frame per monitor refresh (vsync-paced: smoothest, no judder)
    let fps: f64 = std::env::var("SCREENSAVER_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);

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

    let mut renderer = pollster::block_on(render::Renderer::new(&conn, &surface, w, h))?;

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
        if let Some(guard) = queue.prepare_read() {
            let timeout = next.saturating_duration_since(Instant::now());
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
        }
    }
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
            wl_pointer::Event::Enter { serial, .. } => pointer.set_cursor(serial, None, 0, 0), // hide cursor
            wl_pointer::Event::Motion { .. } | wl_pointer::Event::Button { .. } | wl_pointer::Event::Axis { .. } => state.input(),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { .. } = event {
            state.input();
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
    use super::pid_from;

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
