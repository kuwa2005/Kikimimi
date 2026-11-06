use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn open(url: &str) {
    match find_chrome() {
        Some(path) => {
            let mut cmd = Command::new(&path);
            cmd.arg(format!("--app={url}"))
                .arg("--new-window")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            match cmd.spawn() {
                Ok(_) => tracing::info!("Chrome を開きました: {}", path.display()),
                Err(err) => tracing::warn!(
                    "Chrome を起動できませんでした ({err})。ブラウザで {url} を開いてください"
                ),
            }
        }
        None => {
            tracing::info!("Chrome が見つかりません。ブラウザで {url} を開いてください");
        }
    }
}

fn find_chrome() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("KIKIMIMI_CHROME") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Google/Chrome/Application/chrome.exe"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        candidates.push(
            PathBuf::from(profile).join("AppData/Local/Google/Chrome/Application/chrome.exe"),
        );
    }
    candidates.push(PathBuf::from(
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    ));
    candidates.push(PathBuf::from(
        "/mnt/c/Program Files/Google/Chrome/Application/chrome.exe",
    ));
    candidates.push(PathBuf::from(
        "/mnt/c/Program Files (x86)/Google/Chrome/Application/chrome.exe",
    ));
    if let Ok(users) = std::fs::read_dir("/mnt/c/Users") {
        for entry in users.flatten() {
            candidates.push(
                entry
                    .path()
                    .join("AppData/Local/Google/Chrome/Application/chrome.exe"),
            );
        }
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "chromium",
    ] {
        if let Some(path) = which(name) {
            candidates.push(path);
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        let exe = Path::new(name).with_extension("exe");
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
