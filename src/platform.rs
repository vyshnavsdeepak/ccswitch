use std::{env, fs, path::Path};

#[derive(Debug, Clone, PartialEq)]
pub enum Platform {
    MacOS,
    Linux,
    Wsl,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::MacOS => write!(f, "macOS"),
            Platform::Linux => write!(f, "Linux"),
            Platform::Wsl => write!(f, "WSL"),
        }
    }
}

pub fn detect() -> Platform {
    match std::env::consts::OS {
        "macos" => Platform::MacOS,
        "linux" => {
            if env::var("WSL_DISTRO_NAME").is_ok() || env::var("WSL_INTEROP").is_ok() {
                Platform::Wsl
            } else {
                Platform::Linux
            }
        }
        _ => Platform::Linux,
    }
}

pub fn is_container() -> bool {
    if Path::new("/.dockerenv").exists() {
        return true;
    }
    for file in ["/proc/1/cgroup", "/proc/self/mountinfo"] {
        if let Ok(content) = fs::read_to_string(file) {
            if ["docker", "lxc", "containerd", "kubepods", "overlay"]
                .iter()
                .any(|k| content.contains(k))
            {
                return true;
            }
        }
    }
    env::var("CONTAINER").is_ok() || env::var("container").is_ok()
}

pub fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}
