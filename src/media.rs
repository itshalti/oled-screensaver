//! "Is a video playing?" via MPRIS. Browsers (Firefox/Zen, Chrome) block idle through DBus
//! org.freedesktop.ScreenSaver, which a desktop shell may own without telling the compositor,
//! so the idle timer fires during a film. Players do report playback over MPRIS, so we ask them.

use std::process::Command;

const PREFIX: &str = "org.mpris.MediaPlayer2.";

/// The first MPRIS player (bus name) that is playing and not in `ignore`, if any.
// shortcut: busctl processes instead of a DBus library; runs only every 2 s while idle with the
// screensaver hidden. Switch to zbus if it ever has to run often.
pub fn playing(ignore: &[String]) -> Option<String> {
    let busctl = |args: &[&str]| -> Option<String> {
        let out = Command::new("busctl").arg("--user").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let names = busctl(&["call", "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames"])?;
    players(&names).into_iter().filter(|n| !ignored(n, ignore)).find_map(|n| {
        let status = busctl(&["get-property", n, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", "PlaybackStatus"])?;
        status.contains("\"Playing\"").then(|| n.to_string())
    })
}

/// MPRIS bus names in `busctl call … ListNames` output: `as 3 "a" "b" "c"`.
fn players(list: &str) -> Vec<&str> {
    list.split('"').skip(1).step_by(2).filter(|n| n.starts_with(PREFIX)).collect()
}

/// Whether the player part of `name` (`spotify` in `org.mpris.MediaPlayer2.spotify`, `firefox` in
/// `…firefox.instance_1_209`) is in `ignore`, ignoring case.
fn ignored(name: &str, ignore: &[String]) -> bool {
    let player = name.trim_start_matches(PREFIX).split('.').next().unwrap_or("");
    ignore.iter().any(|i| i.eq_ignore_ascii_case(player))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mpris_players_in_busctl_output() {
        let out = r#"as 5 "org.freedesktop.DBus" ":1.7" "org.mpris.MediaPlayer2.firefox.instance_1_209" "org.mpris.MediaPlayer2.spotify" "org.mpris.MediaPlayerX.fake""#;
        assert_eq!(players(out), ["org.mpris.MediaPlayer2.firefox.instance_1_209", "org.mpris.MediaPlayer2.spotify"]);
        assert!(players("").is_empty());
    }

    #[test]
    fn ignore_list_matches_the_player_name_only() {
        let ignore = vec!["Spotify".to_string()];
        assert!(ignored("org.mpris.MediaPlayer2.spotify", &ignore));
        assert!(!ignored("org.mpris.MediaPlayer2.firefox.instance_1_209", &ignore));
        assert!(!ignored("org.mpris.MediaPlayer2.spotifyd", &ignore), "a prefix is not a match");
        assert!(ignored("org.mpris.MediaPlayer2.firefox.instance_1_209", &["firefox".to_string()]));
    }
}
