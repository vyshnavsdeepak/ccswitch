use anyhow::{Context, Result};
use colored::Colorize;
use std::process::Command;

const REPO: &str = "vyshnavsdeepak/ccswitch";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn update() -> Result<()> {
    println!("Checking for updates...");

    let latest_tag = fetch_latest_tag()?;
    let latest_version = latest_tag.trim_start_matches('v');

    if latest_version == CURRENT_VERSION {
        println!("Already on latest (v{})", CURRENT_VERSION);
        return Ok(());
    }

    println!(
        "Update available: {} → {}",
        format!("v{}", CURRENT_VERSION).dimmed(),
        format!("v{}", latest_version).green().bold()
    );

    if cargo_available() {
        run_cargo_install()
    } else {
        download_and_replace(&latest_tag, latest_version)
    }
}

fn fetch_latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let response: serde_json::Value = ureq::get(&url)
        .set("User-Agent", "ccswitch")
        .call()
        .context("Failed to reach GitHub API")?
        .into_json()
        .context("Failed to parse GitHub API response")?;

    response["tag_name"]
        .as_str()
        .map(|s| s.to_owned())
        .context("GitHub API response missing tag_name")
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_cargo_install() -> Result<()> {
    println!("Installing via cargo...");
    let status = Command::new("cargo")
        .args([
            "install",
            "--git",
            &format!("https://github.com/{}", REPO),
            "--force",
        ])
        .status()
        .context("Failed to run cargo install")?;

    if status.success() {
        println!("{}", "Update complete.".green().bold());
        Ok(())
    } else {
        anyhow::bail!("cargo install exited with {}", status);
    }
}

fn download_and_replace(tag: &str, new_version: &str) -> Result<()> {
    let asset = asset_name()?;
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        REPO, tag, asset
    );

    println!("Downloading {}...", url);

    let response = ureq::get(&url)
        .set("User-Agent", "ccswitch")
        .call()
        .context("Failed to download release")?;

    // Write tarball to a temp path
    let tmp_dir = std::env::temp_dir();
    let tarball_path = tmp_dir.join("ccswitch-update.tar.gz");
    let extract_dir = tmp_dir.join("ccswitch-update-extract");

    let _ = std::fs::remove_file(&tarball_path);
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir).context("Failed to create extract dir")?;

    {
        let mut file =
            std::fs::File::create(&tarball_path).context("Failed to create tarball file")?;
        let mut reader = response.into_reader();
        std::io::copy(&mut reader, &mut file).context("Failed to write downloaded tarball")?;
    }

    // Extract with system tar
    let tar_status = Command::new("tar")
        .args([
            "-xzf",
            tarball_path.to_str().unwrap(),
            "-C",
            extract_dir.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run tar")?;

    let _ = std::fs::remove_file(&tarball_path);

    if !tar_status.success() {
        anyhow::bail!("tar exited with {}", tar_status);
    }

    let extracted_bin = extract_dir.join("ccswitch");
    if !extracted_bin.exists() {
        anyhow::bail!("Extracted archive does not contain a 'ccswitch' binary");
    }

    // Atomic replace: copy to <exe>.tmp then rename over original
    let current_exe = std::env::current_exe().context("Failed to resolve current exe path")?;
    let tmp_dest = current_exe.with_extension("tmp");

    std::fs::copy(&extracted_bin, &tmp_dest).context("Failed to copy new binary to tmp path")?;

    let _ = std::fs::remove_dir_all(&extract_dir);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_dest, std::fs::Permissions::from_mode(0o755))
            .context("Failed to set permissions on new binary")?;
    }

    std::fs::rename(&tmp_dest, &current_exe).context("Failed to replace current binary")?;

    // Clear macOS quarantine attribute
    if matches!(crate::platform::detect(), crate::platform::Platform::MacOS) {
        let _ = Command::new("xattr")
            .args(["-c", current_exe.to_str().unwrap()])
            .status();
    }

    println!(
        "{} v{} → v{}",
        "Updated".green().bold(),
        CURRENT_VERSION,
        new_version
    );

    Ok(())
}

fn asset_name() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let name = match crate::platform::detect() {
        crate::platform::Platform::MacOS => match arch {
            "aarch64" => "ccswitch-aarch64-apple-darwin.tar.gz",
            "x86_64" => "ccswitch-x86_64-apple-darwin.tar.gz",
            other => anyhow::bail!("Unsupported macOS arch: {}", other),
        },
        crate::platform::Platform::Linux | crate::platform::Platform::Wsl => {
            "ccswitch-x86_64-unknown-linux-gnu.tar.gz"
        }
    };
    Ok(name.to_owned())
}
