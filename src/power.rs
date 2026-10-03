// SPDX-License-Identifier: GPL-3.0-or-later
// --fps-battery: a slower wallpaper while the machine runs on battery.
//
// fps is the one knob that matters for cost (density is nearly free once the
// board settles), so unplugging drops to --fps-battery and plugging back in
// restores --fps.
//
// AC/battery comes straight off sysfs: no upower, no D-Bus, no helper daemon.
// `/sys/class/power_supply/*/type` == "Mains" identifies a charger; its
// `online` file reads 1 while it is supplying power.

use crate::board::Config;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often the mains state is re-read: two tiny sysfs reads a minute, and
/// nobody notices the pace changing a few seconds after the charger moves.
pub const POWER_POLL: Duration = Duration::from_secs(5);

/// A machine with no mains supply at all is a desktop, so it counts as always
/// plugged in and --fps-battery never applies. An unreadable `online` counts
/// as plugged in too: a misread should give the full-speed wallpaper the user
/// asked for, not a mysteriously slower one.
pub struct Power {
    mains: Vec<PathBuf>,
    pub on_battery: bool,
    next_check: Instant,
}

impl Power {
    pub fn new(now: Instant) -> Self {
        let mut mains = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") {
            for e in rd.flatten() {
                let p = e.path();
                if std::fs::read_to_string(p.join("type")).is_ok_and(|t| t.trim() == "Mains") {
                    mains.push(p.join("online"));
                }
            }
        }
        let mut me = Power { mains, on_battery: false, next_check: now };
        me.refresh(now);
        me
    }

    /// Re-read at most every POWER_POLL. True when the state flipped.
    pub fn refresh(&mut self, now: Instant) -> bool {
        if self.mains.is_empty() || now < self.next_check {
            return false;
        }
        self.next_check = now + POWER_POLL;
        let was = self.on_battery;
        // On battery only when every charger says offline.
        self.on_battery = self.mains.iter().all(|p| std::fs::read_to_string(p).is_ok_and(|s| s.trim() == "0"));
        was != self.on_battery
    }

    /// The frame interval now, re-reading the mains state when it is due.
    pub fn frame(&mut self, cfg: &Config) -> Duration {
        self.refresh(Instant::now());
        frame_interval(cfg, self.on_battery)
    }
}

/// --fps on mains, --fps-battery on battery (0 there: no throttling).
pub fn frame_interval(cfg: &Config, on_battery: bool) -> Duration {
    let fps = if on_battery && cfg.fps_battery > 0.0 { cfg.fps_battery } else { cfg.fps };
    Duration::from_secs_f64(1.0 / fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_fps_applies_only_on_battery() {
        let cfg = Config { fps: 15.0, fps_battery: 8.0, ..Config::default() };
        assert_eq!(frame_interval(&cfg, false), Duration::from_secs_f64(1.0 / 15.0));
        assert_eq!(frame_interval(&cfg, true), Duration::from_secs_f64(1.0 / 8.0));
    }

    #[test]
    fn zero_battery_fps_disables_throttling() {
        let cfg = Config { fps: 15.0, fps_battery: 0.0, ..Config::default() };
        assert_eq!(frame_interval(&cfg, true), frame_interval(&cfg, false));
    }

    #[test]
    fn a_machine_with_no_charger_is_never_on_battery() {
        // A desktop: no Mains supply in sysfs. refresh() must stay a no-op
        // rather than conclude "every charger is offline" from none.
        let now = Instant::now();
        let mut p = Power { mains: Vec::new(), on_battery: false, next_check: now };
        assert!(!p.refresh(now + POWER_POLL * 2));
        assert!(!p.on_battery);
    }

    #[test]
    fn reads_a_charger_going_offline() {
        let dir = std::env::temp_dir().join(format!("lifewall-power-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let online = dir.join("online");
        std::fs::write(&online, "1\n").unwrap();
        let now = Instant::now();
        let mut p = Power { mains: vec![online.clone()], on_battery: false, next_check: now };
        assert!(!p.refresh(now), "plugged in: no change");
        std::fs::write(&online, "0\n").unwrap();
        assert!(!p.refresh(now + POWER_POLL / 2), "not due yet");
        assert!(p.refresh(now + POWER_POLL) && p.on_battery);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
