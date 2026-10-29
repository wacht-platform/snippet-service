//! `snippet doctor`: checks external tools and voice assets without the daemon or the database.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Pm {
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
    Brew,
    #[value(name = "none")]
    NoPm,
}

pub struct Host {
    pub linux: bool,
    pub arm64: bool,
    pub npm: bool,
}

pub struct Item {
    pub id: &'static str,
    pub required: bool,
    pub ok: bool,
    pub detail: String,
}

const CLOUDFLARED_URL: &str = "https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/";

pub fn purpose(id: &str, cmd: Option<&str>) -> String {
    match id {
        "git" => "version control for sessions, worktrees and checkpoints".into(),
        "cloudflared" if cmd.is_none() => format!("phone access outside the LAN, see {CLOUDFLARED_URL}"),
        "cloudflared" => "phone access outside the LAN".into(),
        "gh" => "GitHub pull requests".into(),
        "ffprobe" => "audio file transcription (ffmpeg)".into(),
        "rg" => "fast code search for agents (falls back to grep)".into(),
        "claude" => "Claude Code provider".into(),
        "libstdcxx" => format!("voice recognition runtime (libstdc++ with {})", voice_glibcxx()),
        "voice-assets" => "voice recognition runtime and model (about 35 MB download)".into(),
        _ => String::new(),
    }
}

fn voice_glibcxx() -> &'static str {
    #[cfg(target_os = "linux")]
    return crate::voiceprint::runtime::GLIBCXX;
    #[cfg(not(target_os = "linux"))]
    ""
}

fn pkg(pm: Pm, name: &str) -> Option<String> {
    Some(match pm {
        Pm::Apt => format!("sudo apt-get install -y {name}"),
        Pm::Dnf => format!("sudo dnf install -y {name}"),
        Pm::Pacman => format!("sudo pacman -S --noconfirm {name}"),
        Pm::Zypper => format!("sudo zypper --non-interactive install {name}"),
        Pm::Apk => format!("sudo apk add {name}"),
        Pm::Brew => format!("brew install {name}"),
        Pm::NoPm => return None,
    })
}

pub fn install_command(id: &str, pm: Pm, host: &Host) -> Option<String> {
    match id {
        "git" => pkg(pm, "git"),
        "gh" => pkg(pm, if matches!(pm, Pm::Pacman | Pm::Apk) { "github-cli" } else { "gh" }),
        "ffprobe" => pkg(pm, if pm == Pm::Dnf { "ffmpeg-free" } else { "ffmpeg" }),
        "rg" => pkg(pm, "ripgrep"),
        "cloudflared" => match pm {
            Pm::Brew | Pm::Pacman => pkg(pm, "cloudflared"),
            _ if host.linux => Some(format!(
                "mkdir -p \"$HOME/.snippet/bin\" && curl -fsSL -o \"$HOME/.snippet/bin/cloudflared\" https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-{} && chmod +x \"$HOME/.snippet/bin/cloudflared\"",
                if host.arm64 { "arm64" } else { "amd64" }
            )),
            _ => None,
        },
        "claude" => host.npm.then(|| "npm i -g @anthropic-ai/claude-code".to_string()),
        "libstdcxx" => match pm {
            Pm::Apt | Pm::Zypper => pkg(pm, "libstdc++6"),
            Pm::Dnf | Pm::Apk => pkg(pm, "libstdc++"),
            Pm::Pacman => pkg(pm, "gcc-libs"),
            Pm::Brew | Pm::NoPm => None,
        },
        "voice-assets" => Some("snippet doctor --prefetch".into()),
        _ => None,
    }
}

pub fn tsv_line(item: &Item, pm: Pm, host: &Host) -> String {
    let cmd = install_command(item.id, pm, host);
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    format!(
        "{}\t{}\t{}\t{}",
        item.id,
        if item.required { "required" } else { "optional" },
        clean(&purpose(item.id, cmd.as_deref())),
        cmd.map_or("-".into(), |c| clean(&c))
    )
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| is_exec(p))
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn found(id: &'static str, required: bool, path: Option<PathBuf>) -> Item {
    Item { id, required, ok: path.is_some(), detail: path.map_or("not found".into(), |p| p.display().to_string()) }
}

pub fn host() -> Host {
    Host { linux: cfg!(target_os = "linux"), arm64: cfg!(target_arch = "aarch64"), npm: on_path("npm").is_some() }
}

pub fn detect_pm() -> Pm {
    if cfg!(target_os = "macos") {
        return if on_path("brew").is_some() || Path::new("/opt/homebrew/bin/brew").exists() { Pm::Brew } else { Pm::NoPm };
    }
    [("apt-get", Pm::Apt), ("dnf", Pm::Dnf), ("pacman", Pm::Pacman), ("zypper", Pm::Zypper), ("apk", Pm::Apk), ("brew", Pm::Brew)]
        .into_iter()
        .find(|(bin, _)| on_path(bin).is_some())
        .map_or(Pm::NoPm, |(_, pm)| pm)
}

pub fn check() -> Vec<Item> {
    let mut items = vec![
        found("git", true, on_path("git")),
        found("cloudflared", false, crate::serve::find_cloudflared().map(|p| if p.is_absolute() { p } else { on_path("cloudflared").unwrap_or(p) })),
        found("gh", false, on_path("gh")),
        found("ffprobe", false, on_path("ffprobe")),
        found("rg", false, on_path("rg")),
        found("claude", false, crate::harness::claude_binary()),
    ];
    #[cfg(target_os = "linux")]
    items.push(libstdcxx());
    items.push(voice_assets());
    items
}

fn voice_assets() -> Item {
    let model = crate::voiceprint::neural::model_ok();
    #[cfg(target_os = "linux")]
    let runtime = crate::voiceprint::runtime::installed().is_some();
    #[cfg(not(target_os = "linux"))]
    let runtime = true;
    let detail = match (runtime, model) {
        (true, true) => format!("{} and model in ~/.snippet", if cfg!(target_os = "linux") { "runtime" } else { "built-in runtime" }),
        (false, true) => "runtime not downloaded yet".into(),
        (true, false) => "model not downloaded yet".into(),
        (false, false) => "runtime and model not downloaded yet".into(),
    };
    Item { id: "voice-assets", required: false, ok: runtime && model, detail }
}

#[cfg(target_os = "linux")]
fn libstdcxx() -> Item {
    use crate::voiceprint::runtime;
    let item = |ok, detail| Item { id: "libstdcxx", required: false, ok, detail };
    if let Some(lib) = runtime::installed() {
        match runtime::probe(&lib) {
            Ok(v) => return item(true, format!("onnxruntime {v} loads")),
            Err(e) if e.contains("GLIBC") || e.contains("libstdc++") => return item(false, e),
            Err(_) => {}
        }
    }
    let mut needle = runtime::GLIBCXX.as_bytes().to_vec();
    needle.push(0);
    let libs = libstdcxx_paths();
    for lib in &libs {
        if std::fs::read(lib).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle)) {
            return item(true, format!("{} has {}", lib.display(), runtime::GLIBCXX));
        }
    }
    match libs.first() {
        Some(lib) => item(false, format!("{} lacks {}", lib.display(), runtime::GLIBCXX)),
        None => item(false, "libstdc++.so.6 not found".into()),
    }
}

#[cfg(target_os = "linux")]
fn libstdcxx_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for bin in ["ldconfig", "/sbin/ldconfig", "/usr/sbin/ldconfig"] {
        if let Ok(o) = std::process::Command::new(bin).arg("-p").output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if line.trim_start().starts_with("libstdc++.so.6 ") && (line.contains("x86-64") || line.contains("AArch64")) {
                    if let Some((_, p)) = line.rsplit_once("=> ") {
                        out.push(PathBuf::from(p.trim()));
                    }
                }
            }
            break;
        }
    }
    for p in ["/usr/lib/x86_64-linux-gnu/libstdc++.so.6", "/usr/lib64/libstdc++.so.6", "/usr/lib/libstdc++.so.6", "/lib/x86_64-linux-gnu/libstdc++.so.6"] {
        let p = PathBuf::from(p);
        if p.exists() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

pub fn report(items: &[Item], pm: Pm, host: &Host) -> String {
    let mut out = format!("Snippet doctor ({} {})\n\n", std::env::consts::OS, std::env::consts::ARCH);
    for it in items {
        let cmd = install_command(it.id, pm, host);
        let status = if it.ok { "ok" } else if it.required { "MISSING" } else { "optional" };
        let kind = if it.required { "required" } else { "optional" };
        out.push_str(&format!("  {status:<9} {:<13} {} ({kind})\n", it.id, purpose(it.id, cmd.as_deref())));
        out.push_str(&format!("  {:<9} {:<13} {}\n", "", "", it.detail));
        if !it.ok {
            out.push_str(&format!("  {:<9} {:<13} install: {}\n", "", "", cmd.as_deref().unwrap_or("no package for this system")));
        }
    }
    let missing = |req: bool| items.iter().filter(|i| !i.ok && i.required == req).count();
    out.push_str(&format!("\n{} required missing, {} optional missing\n", missing(true), missing(false)));
    out
}

pub fn prefetch() {
    #[cfg(target_os = "linux")]
    {
        use crate::voiceprint::runtime;
        let fresh = runtime::installed().is_none();
        if fresh {
            println!("voice runtime: downloading onnxruntime {}...", runtime::VERSION);
        }
        match runtime::install_checked() {
            Ok((p, v)) => println!("voice runtime: ok, onnxruntime {v} at {}", p.display()),
            Err(e) => println!("warning: voice runtime unavailable: {e}"),
        }
    }
    if !crate::voiceprint::neural::model_ok() {
        println!("voice model: downloading {}...", crate::voiceprint::NEURAL_MODEL);
    }
    match crate::voiceprint::neural::fetch_model() {
        Ok(p) => println!("voice model: ok, {}", p.display()),
        Err(e) => println!("warning: voice model unavailable: {e}"),
    }
}

pub fn run(tsv: bool, pm: Option<Pm>, prefetch_assets: bool) {
    if prefetch_assets {
        prefetch();
        return;
    }
    let pm = pm.unwrap_or_else(detect_pm);
    let host = host();
    let items = check();
    if tsv {
        for it in items.iter().filter(|i| !i.ok) {
            println!("{}", tsv_line(it, pm, &host));
        }
    } else {
        print!("{}", report(&items, pm, &host));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: Host = Host { linux: true, arm64: false, npm: true };
    const MAC_NO_NPM: Host = Host { linux: false, arm64: true, npm: false };

    fn item(id: &'static str, required: bool) -> Item {
        Item { id, required, ok: false, detail: String::new() }
    }

    #[test]
    fn tsv_has_four_tab_separated_fields() {
        let line = tsv_line(&item("git", true), Pm::Apt, &LINUX);
        assert_eq!(line, "git\trequired\tversion control for sessions, worktrees and checkpoints\tsudo apt-get install -y git");
        let line = tsv_line(&item("claude", false), Pm::Brew, &MAC_NO_NPM);
        assert_eq!(line, "claude\toptional\tClaude Code provider\t-");
        for id in ["git", "cloudflared", "gh", "ffprobe", "rg", "claude", "libstdcxx", "voice-assets"] {
            for pm in [Pm::Apt, Pm::Dnf, Pm::Pacman, Pm::Zypper, Pm::Apk, Pm::Brew, Pm::NoPm] {
                let line = tsv_line(&item(id, false), pm, &LINUX);
                let fields: Vec<_> = line.split('\t').collect();
                assert_eq!(fields.len(), 4, "{line}");
                assert!(fields.iter().all(|f| !f.is_empty() && !f.contains('\n')), "{line}");
            }
        }
    }

    #[test]
    fn package_names_per_manager() {
        let cmd = |id, pm| install_command(id, pm, &LINUX);
        assert_eq!(cmd("gh", Pm::Pacman).unwrap(), "sudo pacman -S --noconfirm github-cli");
        assert_eq!(cmd("gh", Pm::Dnf).unwrap(), "sudo dnf install -y gh");
        assert_eq!(cmd("ffprobe", Pm::Apt).unwrap(), "sudo apt-get install -y ffmpeg");
        assert_eq!(cmd("ffprobe", Pm::Dnf).unwrap(), "sudo dnf install -y ffmpeg-free");
        assert_eq!(cmd("rg", Pm::Zypper).unwrap(), "sudo zypper --non-interactive install ripgrep");
        assert_eq!(cmd("libstdcxx", Pm::Apt).unwrap(), "sudo apt-get install -y libstdc++6");
        assert_eq!(cmd("libstdcxx", Pm::Dnf).unwrap(), "sudo dnf install -y libstdc++");
        assert_eq!(cmd("libstdcxx", Pm::Brew), None);
        assert_eq!(cmd("voice-assets", Pm::NoPm).unwrap(), "snippet doctor --prefetch");
        assert_eq!(cmd("git", Pm::NoPm), None);
        assert_eq!(install_command("cloudflared", Pm::Brew, &MAC_NO_NPM).unwrap(), "brew install cloudflared");
        assert!(cmd("cloudflared", Pm::Apt).unwrap().contains("cloudflared-linux-amd64"));
        assert_eq!(install_command("cloudflared", Pm::NoPm, &MAC_NO_NPM), None);
        assert!(purpose("cloudflared", None).contains("https://"));
        assert_eq!(cmd("claude", Pm::Apt).unwrap(), "npm i -g @anthropic-ai/claude-code");
    }
}
