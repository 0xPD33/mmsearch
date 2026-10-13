//! System load and AC power, for the backlog gate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SAMPLE: Duration = Duration::from_secs(2);
/// The gate stays closed this long after the last busy sample, so a game's loading
/// screen or a short compile does not flip it on and off.
const HOLD: Duration = Duration::from_secs(30);
/// CPU use of other processes, in percent of all cores.
const MAX_CPU: f32 = 50.0;
/// SM use of other processes on any NVIDIA GPU, in percent.
const MAX_GPU: u32 = 33;
/// Available memory, in percent of the total.
const MIN_MEM: u64 = 15;

/// Starts a sampler thread. The value is the reason the backlog must wait, or `None`.
pub fn watch() -> Arc<Mutex<Option<&'static str>>> {
    let busy = Arc::new(Mutex::new(None));
    let out = busy.clone();
    std::thread::Builder::new()
        .name("load".into())
        .spawn(move || {
            let mut cpu = CpuSample::now();
            let mut last: Option<(&'static str, Instant)> = None;
            loop {
                std::thread::sleep(SAMPLE);
                let next = CpuSample::now();
                let reason = if !on_ac_power() {
                    Some("battery")
                } else if mem_available_pct().is_some_and(|p| p < MIN_MEM) {
                    Some("low memory")
                } else if cpu.others_pct(&next).is_some_and(|p| p > MAX_CPU) {
                    Some("cpu busy")
                } else if gpu_others_pct().is_some_and(|p| p > MAX_GPU) {
                    Some("gpu busy")
                } else {
                    None
                };
                cpu = next;
                if let Some(r) = reason {
                    last = Some((r, Instant::now()));
                }
                *out.lock().unwrap() = last.filter(|(_, t)| t.elapsed() < HOLD).map(|(r, _)| r);
            }
        })
        .expect("spawn load thread");
    busy
}

/// Jiffies of the whole machine and of this process with its reaped children (ffmpeg).
struct CpuSample {
    total: u64,
    idle: u64,
    own: u64,
}

impl CpuSample {
    fn now() -> CpuSample {
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let f: Vec<u64> = stat
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse().ok())
            .collect();
        // user nice system idle iowait irq softirq steal
        let total = f.iter().take(8).sum();
        let idle = f.get(3).copied().unwrap_or(0) + f.get(4).copied().unwrap_or(0);
        // The comm field can hold spaces; the fields after it start past the last ')'.
        let own = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                let rest = s.rsplit_once(')')?.1.to_string();
                let v: Vec<u64> = rest
                    .split_whitespace()
                    .skip(11)
                    .take(4)
                    .filter_map(|x| x.parse().ok())
                    .collect();
                Some(v.iter().sum())
            })
            .unwrap_or(0);
        CpuSample { total, idle, own }
    }

    fn others_pct(&self, next: &CpuSample) -> Option<f32> {
        let total = next.total.checked_sub(self.total).filter(|t| *t > 0)?;
        let busy = total.saturating_sub(next.idle.saturating_sub(self.idle));
        let others = busy.saturating_sub(next.own.saturating_sub(self.own));
        Some(others as f32 * 100.0 / total as f32)
    }
}

fn mem_available_pct() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        info.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some(field("MemAvailable:")? * 100 / field("MemTotal:")?.max(1))
}

/// Compositors: their SM share rises while the daemon embeds, so counting them makes the
/// gate pause itself. A fullscreen game still counts through its own process.
const COMPOSITORS: &[&str] = &[
    "niri",
    "Hyprland",
    "sway",
    "kwin_wayland",
    "gnome-shell",
    "labwc",
    "river",
    "wayfire",
];

/// SM use of all processes but this one and the compositor, from `nvidia-smi pmon`.
/// `None` without NVIDIA.
// ponytail: NVIDIA only; amdgpu's gpu_busy_percent includes our own load, so it needs another approach.
fn gpu_others_pct() -> Option<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["pmon", "-c", "1", "-s", "u"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let me = std::process::id().to_string();
    let mut per_gpu: std::collections::HashMap<String, u32> = Default::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if line.starts_with('#')
            || cols.len() < 4
            || cols[1] == me
            || cols.last().is_some_and(|c| COMPOSITORS.contains(c))
        {
            continue;
        }
        *per_gpu.entry(cols[0].to_string()).or_default() += cols[3].parse().unwrap_or(0);
    }
    per_gpu.into_values().max().or(Some(0))
}

/// True if an external supply is online (mains, or USB-C on many laptops), or if the
/// machine has no system battery. Device batteries (a mouse, a game pad) do not count.
fn on_ac_power() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else {
        return true;
    };
    let mut battery = false;
    for supply in dir.flatten() {
        let read = |f: &str| {
            std::fs::read_to_string(supply.path().join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        if read("scope") == "Device" {
            continue;
        }
        match read("type").as_str() {
            "Mains" | "USB" | "Wireless" if read("online") == "1" => return true,
            "Battery" => battery = true,
            _ => {}
        }
    }
    !battery
}
