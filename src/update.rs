//! Self-update from the GitHub releases of `benpshore/pdftextract`, plus the
//! build's version string and the passive once-a-day update notice.
//!
//! A release carries one archive per target, `tpe-<target>.tar.gz`, each
//! holding a single file `tpe`, and a `SHA256SUMS` asset with one
//! `<sha256>  tpe-<target>.tar.gz` line per archive. `tpe update` reads
//! the latest release from the GitHub API, downloads the archive for the
//! running platform next to the current executable, verifies its SHA-256,
//! unpacks `tpe`, marks it executable and renames it over the running
//! binary (a rename over a running executable is fine on macOS and Linux).
//! A binary installed by mise lives under a `mise` directory and is left to
//! `mise upgrade`.
//!
//! The network functions are thin; everything decidable offline (asset
//! selection, checksum parsing, version comparison, the notice text) is a
//! pure function with unit tests.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Version baked in by `build.rs`: the release tag without its `v`, or
/// `0.0.0-dev`.
pub const VERSION: &str = env!("TPE_VERSION");
/// Short git commit baked in by `build.rs`, or `unknown`.
pub const GIT_SHA: &str = env!("TPE_GIT_SHA");

/// GitHub API endpoint for the latest release; `TPE_RELEASES_API` overrides it.
pub const DEFAULT_RELEASES_API: &str =
    "https://api.github.com/repos/benpshore/pdftextract/releases/latest";
/// Name of the checksum asset.
pub const SUMS_ASSET: &str = "SHA256SUMS";
/// Environment variable that disables the passive check when set (to
/// anything but an empty string or `0`).
pub const NO_CHECK_ENV: &str = "TPE_NO_UPDATE_CHECK";

/// Timeout of every request made by `tpe update`.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout of the passive background check.
const PASSIVE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the end of a run waits for the passive check's answer.
const PASSIVE_GRACE: Duration = Duration::from_millis(300);
/// Minimum interval between two passive checks.
const PASSIVE_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Largest archive accepted.
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Largest API or checksum response accepted.
const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;

/// `tpe <version> (<sha>)`, the `--version` text after the program name.
#[must_use]
pub fn version_string() -> String {
    format!("{VERSION} ({GIT_SHA})")
}

/// A `MAJOR.MINOR.PATCH` release version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// Parse `v?MAJOR.MINOR.PATCH`. Anything else (including a `-dev` suffix)
/// is not a release version.
#[must_use]
pub fn parse_version(text: &str) -> Option<Version> {
    let trimmed = text.trim();
    let core = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// Whether release `latest` is newer than `running`. A running build that is
/// not a release version (a dev build) is older than any release; a
/// `latest` that is not a release version is never newer.
#[must_use]
pub fn is_newer(latest: &str, running: &str) -> bool {
    match (parse_version(latest), parse_version(running)) {
        (Some(latest), Some(running)) => latest > running,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Release target triple of this build, when a release archive exists for it.
#[must_use]
pub fn target() -> Option<&'static str> {
    target_for(std::env::consts::ARCH, std::env::consts::OS)
}

/// Release target triple for an architecture and operating system.
#[must_use]
pub fn target_for(arch: &str, os: &str) -> Option<&'static str> {
    match (arch, os) {
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

/// Archive asset name for a target.
#[must_use]
pub fn asset_name(target: &str) -> String {
    format!("tpe-{target}.tar.gz")
}

/// One downloadable file of a release.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

/// The fields of a GitHub release this module uses.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Release {
    #[serde(rename = "tag_name")]
    pub tag: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// Parse the JSON body of the releases API.
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        serde_json::from_str(json).context("parsing the release JSON")
    }

    /// The asset named `name`.
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The archive for `target`.
    #[must_use]
    pub fn archive_for(&self, target: &str) -> Option<&Asset> {
        self.asset(&asset_name(target))
    }
}

/// The lower-case hex SHA-256 recorded for `name` in a `SHA256SUMS` text
/// (`<hex>  <name>`; a leading `*` on the name, as `sha256sum -b` writes it,
/// is accepted).
#[must_use]
pub fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hex = fields.next()?;
        let file = fields.next()?;
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

/// Lower-case hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The releases API URL: `TPE_RELEASES_API` or the default.
#[must_use]
pub fn api_url() -> String {
    std::env::var("TPE_RELEASES_API")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASES_API.to_string())
}

/// The running executable, canonicalized when possible.
pub fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Whether `exe` was installed by mise (a path component named `mise`).
#[must_use]
pub fn managed_by_mise(exe: &Path) -> bool {
    exe.components()
        .any(|component| component.as_os_str() == OsStr::new("mise"))
}

/// The command that installs a newer version.
fn upgrade_hint(managed: bool) -> &'static str {
    if managed {
        "run: mise upgrade"
    } else {
        "run: tpe update"
    }
}

/// The one-line notice printed when `latest` is newer than the running build.
#[must_use]
pub fn notice_line(latest: &str, running: &str, managed: bool) -> String {
    let latest = latest.trim().trim_start_matches('v');
    format!(
        "tpe {latest} is available (running {running}); {}",
        upgrade_hint(managed)
    )
}

/// An HTTP agent with `timeout` on the whole call and the `tpe/<version>`
/// user agent.
fn agent(timeout: Duration) -> ureq::Agent {
    let user_agent = format!("tpe/{VERSION}");
    let config = ureq::Agent::config_builder()
        .user_agent(user_agent.as_str())
        .http_status_as_error(true)
        .max_redirects(10)
        .timeout_global(Some(timeout))
        .build();
    ureq::Agent::new_with_config(config)
}

/// GET `url` as text (at most `limit` bytes).
fn get_text(agent: &ureq::Agent, url: &str, limit: u64) -> anyhow::Result<String> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("GET {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

/// GET `url` as bytes (at most `limit` bytes).
fn get_bytes(agent: &ureq::Agent, url: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .call()
        .with_context(|| format!("GET {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// Fetch the latest release with `timeout` per request.
pub fn fetch_latest(timeout: Duration) -> anyhow::Result<Release> {
    let url = api_url();
    let body = get_text(&agent(timeout), &url, MAX_TEXT_BYTES)?;
    Release::parse(&body)
}

/// Copy the single `tpe` file out of the gzip tar archive at `archive` to
/// `staged`.
fn unpack_tpe(archive: &Path, staged: &Path) -> anyhow::Result<()> {
    let file = fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading an archive entry")?;
        let is_tpe = {
            let path = entry.path().context("reading an archive entry name")?;
            path.file_name() == Some(OsStr::new("tpe"))
        };
        if !is_tpe || !entry.header().entry_type().is_file() {
            continue;
        }
        let mut out =
            fs::File::create(staged).with_context(|| format!("creating {}", staged.display()))?;
        io::copy(&mut entry, &mut out).with_context(|| format!("writing {}", staged.display()))?;
        out.flush()?;
        return Ok(());
    }
    bail!("the archive {} contains no `tpe` file", archive.display())
}

/// Mark `path` executable (`0o755`).
fn make_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Remove `path`, ignoring a failure (best-effort cleanup of temp files).
fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Replace the executable at `exe` with the `tpe` inside `release`'s
/// archive for `target`, verifying its checksum first. Temp files live
/// next to `exe` so the final rename stays on one file system.
fn install(release: &Release, target: &str, exe: &Path) -> anyhow::Result<()> {
    let name = asset_name(target);
    let archive = release
        .archive_for(target)
        .ok_or_else(|| anyhow!("release {} has no asset {name}", release.tag))?;
    let sums = release
        .asset(SUMS_ASSET)
        .ok_or_else(|| anyhow!("release {} has no {SUMS_ASSET} asset", release.tag))?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", exe.display()))?;
    let pid = std::process::id();
    let archive_path = dir.join(format!(".tpe-update-{pid}.tar.gz"));
    let staged = dir.join(format!(".tpe-update-{pid}.bin"));
    // Fail before any download when the directory is not writable.
    let unwritable = format!(
        "cannot write to {} (the directory holding {})",
        dir.display(),
        exe.display()
    );
    fs::File::create(&archive_path).context(unwritable)?;

    let staging = Staging {
        archive_path,
        staged,
        exe: exe.to_path_buf(),
    };
    let outcome = download_and_replace(archive, sums, &name, &staging);
    remove_quietly(&staging.archive_path);
    if outcome.is_err() {
        remove_quietly(&staging.staged);
    }
    outcome
}

/// Where an update is downloaded, unpacked and installed.
struct Staging {
    /// Temp file that receives the archive.
    archive_path: PathBuf,
    /// Temp file that receives the unpacked `tpe`.
    staged: PathBuf,
    /// The executable being replaced.
    exe: PathBuf,
}

/// Download `archive`, check it against `sums`, unpack it and rename the
/// result over the executable.
fn download_and_replace(
    archive: &Asset,
    sums: &Asset,
    name: &str,
    staging: &Staging,
) -> anyhow::Result<()> {
    let agent = agent(UPDATE_TIMEOUT);
    let sums_text = get_text(&agent, &sums.url, MAX_TEXT_BYTES)?;
    let expected = expected_sha256(&sums_text, name)
        .ok_or_else(|| anyhow!("{SUMS_ASSET} has no entry for {name}"))?;
    let bytes = get_bytes(&agent, &archive.url, MAX_ARCHIVE_BYTES)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("checksum mismatch for {name}: expected {expected}, got {actual}");
    }
    let archive_path = &staging.archive_path;
    let staged = &staging.staged;
    let exe = &staging.exe;
    fs::write(archive_path, &bytes)
        .with_context(|| format!("writing {}", archive_path.display()))?;
    unpack_tpe(archive_path, staged)?;
    make_executable(staged)?;
    fs::rename(staged, exe)
        .with_context(|| format!("replacing {} with {}", exe.display(), staged.display()))?;
    Ok(())
}

/// `tpe update [--check]`. Reports the latest release; without `--check`
/// installs it when it is newer, unless the binary is managed by mise.
pub fn run(check_only: bool) -> anyhow::Result<()> {
    let exe = current_exe()?;
    let managed = managed_by_mise(&exe);
    let release = fetch_latest(UPDATE_TIMEOUT)?;
    let latest = release.tag.trim().trim_start_matches('v').to_string();
    if !is_newer(&latest, VERSION) {
        println!("tpe {VERSION} is up to date (latest release {latest})");
        return Ok(());
    }
    if check_only {
        println!("{}", notice_line(&latest, VERSION, managed));
        return Ok(());
    }
    if managed {
        println!("tpe is managed by mise; run: mise upgrade");
        return Ok(());
    }
    let Some(target) = target() else {
        bail!(
            "no release archive for this platform ({}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    };
    install(&release, target, &exe)?;
    println!("updated tpe {VERSION} -> {latest} at {}", exe.display());
    Ok(())
}

/// Whether the passive check is disabled by `TPE_NO_UPDATE_CHECK`.
fn passive_disabled() -> bool {
    std::env::var(NO_CHECK_ENV).is_ok_and(|value| !value.is_empty() && value != "0")
}

/// `$XDG_CACHE_HOME`, else `$HOME/Library/Caches` on macOS, else
/// `$HOME/.cache`.
fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    let home = PathBuf::from(home);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        home.join(".cache")
    })
}

/// The passive check's state file.
fn state_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join("tpe/last-update-check"))
}

/// Contents of the state file: when the last check ran and what it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckState {
    /// Unix seconds of the last check.
    pub checked_at: u64,
    /// Tag of the latest release seen by that check, if it answered.
    pub latest: Option<String>,
}

impl CheckState {
    /// Parse `key=value` lines.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut state = Self::default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("checked_at", value)) => {
                    state.checked_at = value.trim().parse().unwrap_or(0);
                }
                Some(("latest", value)) if !value.trim().is_empty() => {
                    state.latest = Some(value.trim().to_string());
                }
                _ => {}
            }
        }
        state
    }

    /// The `key=value` text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!("checked_at={}\n", self.checked_at);
        if let Some(latest) = &self.latest {
            text.push_str(&format!("latest={latest}\n"));
        }
        text
    }
}

/// Unix seconds now.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn read_state(path: &Path) -> Option<CheckState> {
    let text = fs::read_to_string(path).ok()?;
    Some(CheckState::parse(&text))
}

fn write_state(path: &Path, state: &CheckState) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, state.render())
}

/// The passive once-a-day check started at the beginning of a run and
/// finished at its end. Nothing here can fail the run: any error leaves the
/// notice silent.
#[derive(Debug)]
pub struct PassiveCheck {
    /// Answer of a background fetch started by this run.
    receiver: Option<mpsc::Receiver<String>>,
    /// Latest tag remembered from a check within the last day.
    cached: Option<String>,
    managed: bool,
}

impl PassiveCheck {
    /// Begin the check: reuse the state file when it is younger than a day,
    /// otherwise record this attempt and query the API on a background
    /// thread. Disabled entirely by `TPE_NO_UPDATE_CHECK`.
    #[must_use]
    pub fn start() -> Self {
        let managed = current_exe().is_ok_and(|exe| managed_by_mise(&exe));
        let idle = Self {
            receiver: None,
            cached: None,
            managed,
        };
        if passive_disabled() {
            return idle;
        }
        let Some(path) = state_path() else {
            return idle;
        };
        let now = now_unix();
        if let Some(state) = read_state(&path)
            && state.checked_at <= now
            && now - state.checked_at < PASSIVE_INTERVAL_SECS
        {
            return Self {
                cached: state.latest,
                ..idle
            };
        }
        let state = CheckState {
            checked_at: now,
            latest: None,
        };
        if write_state(&path, &state).is_err() {
            return idle;
        }
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            if let Ok(release) = fetch_latest(PASSIVE_TIMEOUT) {
                let state = CheckState {
                    checked_at: now,
                    latest: Some(release.tag.clone()),
                };
                let _ = write_state(&path, &state);
                let _ = sender.send(release.tag);
            }
        });
        Self {
            receiver: Some(receiver),
            ..idle
        }
    }

    /// The notice to print, if a newer release is known. Waits briefly for
    /// a background fetch started by this run, then gives up on it.
    #[must_use]
    pub fn finish(self) -> Option<String> {
        let latest = match self.cached {
            Some(latest) => latest,
            None => self.receiver?.recv_timeout(PASSIVE_GRACE).ok()?,
        };
        is_newer(&latest, VERSION).then(|| notice_line(&latest, VERSION, self.managed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_and_without_v() {
        let expected = Some(Version {
            major: 0,
            minor: 7,
            patch: 12,
        });
        assert_eq!(parse_version("v0.7.12"), expected);
        assert_eq!(parse_version("0.7.12"), expected);
        assert_eq!(parse_version(" v0.7.12\n"), expected);
        assert_eq!(parse_version("0.0.0-dev"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("main"), None);
    }

    #[test]
    fn newer_compares_numerically_and_dev_is_oldest() {
        assert!(is_newer("v0.10.0", "0.9.3"));
        assert!(!is_newer("v0.9.3", "0.10.0"));
        assert!(!is_newer("v0.9.3", "0.9.3"));
        assert!(is_newer("v0.1.0", "0.0.0-dev"));
        assert!(!is_newer("main", "0.1.0"));
        assert!(!is_newer("nightly", "0.0.0-dev"));
    }

    #[test]
    fn targets_map_to_release_archives() {
        assert_eq!(target_for("aarch64", "macos"), Some("aarch64-apple-darwin"));
        assert_eq!(
            target_for("aarch64", "linux"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("x86_64", "linux"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(target_for("x86_64", "macos"), None);
        assert_eq!(target_for("x86_64", "windows"), None);
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "tpe-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn release_json_selects_the_archive_for_a_target() {
        let json = r#"{
  "tag_name": "v0.8.0",
  "assets": [
    {"name": "SHA256SUMS", "browser_download_url": "https://x/s", "size": 3},
    {"name": "tpe-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://x/m"},
    {"name": "tpe-x86_64-unknown-linux-gnu.tar.gz", "browser_download_url": "https://x/l"}
  ]
}"#;
        let release = Release::parse(json).unwrap();
        assert_eq!(release.tag, "v0.8.0");
        assert_eq!(
            release
                .archive_for("x86_64-unknown-linux-gnu")
                .map(|a| a.url.as_str()),
            Some("https://x/l")
        );
        assert_eq!(
            release.asset(SUMS_ASSET).map(|a| a.url.as_str()),
            Some("https://x/s")
        );
        assert!(release.archive_for("aarch64-unknown-linux-gnu").is_none());
        assert!(Release::parse("{}").is_err());
        let bare = Release::parse(r#"{"tag_name": "v1.0.0"}"#).unwrap();
        assert!(bare.assets.is_empty());
    }

    #[test]
    fn sha256sums_lines_are_parsed() {
        let hex = "ab".repeat(32);
        let upper = hex.to_ascii_uppercase();
        let sums = format!(
            "{hex}  tpe-aarch64-apple-darwin.tar.gz\n\
             {upper} *tpe-x86_64-unknown-linux-gnu.tar.gz\n\
             bad  tpe-aarch64-unknown-linux-gnu.tar.gz\n"
        );
        assert_eq!(
            expected_sha256(&sums, "tpe-aarch64-apple-darwin.tar.gz").as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            expected_sha256(&sums, "tpe-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            expected_sha256(&sums, "tpe-aarch64-unknown-linux-gnu.tar.gz"),
            None
        );
        assert_eq!(expected_sha256(&sums, "other"), None);
    }

    #[test]
    fn mise_installs_are_detected_by_path() {
        let exe = Path::new("/home/x/.local/share/mise/installs/tpe/0.7.0/bin/tpe");
        assert!(managed_by_mise(exe));
        assert!(!managed_by_mise(Path::new("/usr/local/bin/tpe")));
        assert!(!managed_by_mise(Path::new("/opt/misery/bin/tpe")));
    }

    #[test]
    fn notice_names_both_versions_and_the_installer() {
        assert_eq!(
            notice_line("v0.9.0", "0.8.0", false),
            "tpe 0.9.0 is available (running 0.8.0); run: tpe update"
        );
        assert_eq!(
            notice_line("0.9.0", "0.0.0-dev", true),
            "tpe 0.9.0 is available (running 0.0.0-dev); run: mise upgrade"
        );
    }

    #[test]
    fn state_round_trips() {
        let state = CheckState {
            checked_at: 1_700_000_000,
            latest: Some("v0.9.0".to_string()),
        };
        assert_eq!(CheckState::parse(&state.render()), state);
        let bare = CheckState {
            checked_at: 5,
            latest: None,
        };
        assert_eq!(CheckState::parse(&bare.render()), bare);
        assert_eq!(CheckState::parse("garbage"), CheckState::default());
    }

    #[test]
    fn version_string_has_the_expected_shape() {
        let text = version_string();
        assert!(text.starts_with(VERSION), "{text}");
        assert!(text.ends_with(&format!("({GIT_SHA})")), "{text}");
    }
}
