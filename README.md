# ss-for-oled

Animated screensaver for OLED monitors on Wayland. Pure black background, nothing stays still. Rendered on the GPU (~2% CPU).

Modes: topographic lines, clock, starfield, aurora, Lissajous, waves.

Needs a compositor with `wlr-layer-shell` and `ext-idle-notify` (Hyprland, Sway, niri…), Vulkan and systemd.

## Install

```bash
cargo install --git https://github.com/itshalti/oled-screensaver --root ~/.local
~/.local/bin/ss-for-oled install
```

The first command builds `ss-for-oled` into `~/.local/bin`. The second registers a systemd user service, starts it, and creates the config. That's all: the screensaver now comes on after 10 minutes idle.

If `ss-for-oled` is "command not found", add that folder to your `PATH`:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshenv   # bash: ~/.bashrc
```

If your shell (Noctalia, Hyprland's hypridle, …) also runs a screensaver on idle, turn that off there.

## How it works

1. The service waits until the compositor reports that you have not touched the mouse or keyboard for `timeout` seconds.
2. Then it shows the screensaver on your monitor: the screen fades to black and an animation fades in.
3. Every few minutes it switches to another random mode.
4. Any key, click, scroll or mouse movement closes it at once. Popups appearing under a resting cursor do not count.
5. While something blocks idle (a playing video, a game, a call), the screensaver does not start.

The service starts with your graphical session and rereads the config whenever you save it.

## Config

Edit `~/.config/ss-for-oled/config.toml` in any text editor and save. Changes apply by themselves: `timeout` within 2 seconds, the rest the next time the screensaver starts. No restart needed.

```toml
timeout = 600        # seconds without input before it starts
output = "DP-1"      # monitor name: see `hyprctl monitors` (Sway: `swaymsg -t get_outputs`)
modes = []           # modes to show; empty = all: Topo, Starfield, Aurora, Lissajous, Waves, Clock
mode_minutes = 5     # minutes per mode before switching
fps = 0              # frame cap; 0 = monitor refresh rate (smoothest)
```

A missing key keeps its default. If the file has an error (a typo in a key, text instead of a number), the defaults are used and the reason is written to the log (see [Service](#service)).

### Examples

Start after 5 minutes:

```toml
timeout = 300
```

Only topographic lines and the clock:

```toml
modes = ["Topo", "Clock"]
```

Show it on another monitor:

```toml
output = "HDMI-A-1"
```

Use less power on a high refresh rate monitor:

```toml
fps = 60
```

Keep one mode all the time:

```toml
modes = ["Topo"]
```

## Use

```bash
ss-for-oled          # show it now (to try it out)
ss-for-oled start    # show it in the background
ss-for-oled stop     # close it
```

`SCREENSAVER_OUTPUT`, `SCREENSAVER_MODE`, `SCREENSAVER_FPS` override the config for one run, e.g. `SCREENSAVER_MODE=Topo ss-for-oled`.

## Service

```bash
systemctl --user status ss-for-oled       # is it running?
journalctl --user -u ss-for-oled -f       # live log: when it starts, why it closed, config errors
systemctl --user restart ss-for-oled      # restart (not needed after config changes)
```

## Uninstall

```bash
ss-for-oled uninstall                                # remove the service
cargo uninstall oled-screensaver --root ~/.local     # remove the program
rm -r ~/.config/ss-for-oled                          # remove the config
```

## Build from source

```bash
git clone https://github.com/itshalti/oled-screensaver
cd oled-screensaver
cargo build --release   # binary: target/release/ss-for-oled
```
