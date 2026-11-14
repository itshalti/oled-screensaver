# ss-for-oled

Animated screensaver for OLED monitors on Wayland. Pure black background, nothing stays still. Rendered on the GPU (~2% CPU).

Modes: topographic lines, clock, starfield, aurora, Lissajous, waves. A random one is picked and switched every 5 minutes. Any key or mouse move closes it.

Needs a compositor with `wlr-layer-shell` (Hyprland, Sway, niri…) and Vulkan.

## Install

```bash
cargo install --git https://github.com/itshalti/oled-screensaver --root ~/.local
```

This builds and puts `ss-for-oled` in `~/.local/bin`. If the command is not found, add that folder to your `PATH`:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshenv   # bash: ~/.bashrc
```

## Build from source

```bash
git clone https://github.com/itshalti/oled-screensaver
cd oled-screensaver
cargo build --release   # binary: target/release/ss-for-oled
```

## Use

```bash
ss-for-oled          # run now
ss-for-oled start    # start in the background
ss-for-oled stop     # close it
```

Idle daemon (e.g. Noctalia → Power → Idle → Custom behavior, timeout 600):

- Idle command: `~/.local/bin/ss-for-oled start`
- Resume command: `~/.local/bin/ss-for-oled stop`

Use the full path: idle daemons often don't have `~/.local/bin` in their `PATH`.
In Noctalia also set **Idle Dim to 0** — the screensaver fades in by itself, otherwise you get two fades in a row.

Options: `SCREENSAVER_OUTPUT=DP-1` (monitor), `SCREENSAVER_MODE=Topo` (start mode), `SCREENSAVER_FPS=60` (frame cap; default = monitor refresh).
