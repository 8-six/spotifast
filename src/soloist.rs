//! Spotify Soloist, downloaded and kept up to date at runtime.
//!
//! Soloist is Spotify's own headless Connect client. Spotify publishes it
//! only as an executable built for each platform and explicitly asks apps not
//! to redistribute it, so Spotifast downloads the official archive from
//! Spotify's CDN on first use and keeps the executable in its own state
//! folder. A build is only valid for 90 days after Spotify made it: Soloist
//! exits with code 10 once it has expired, which makes the supervisor fetch a
//! fresh build and start again.
//!
//! Nothing here talks to Soloist's WebSocket API. Playback control goes
//! through the Web API like any other Connect device.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::paths::AppDirs;

const ARCHIVE_BASE: &str = "https://soloist-builds.spotifycdn.com";
/// A build is downloaded again after this long, well inside Spotify's
/// 90-day validity window. A download date is only an estimate of the
/// build's age, so the margin leaves room for the archive to be older than
/// the moment it was fetched.
const REFRESH_AFTER: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// The largest official archive this will accept, the largest executable it
/// will write out of one, and the largest unpacked tar it will read.
const ARCHIVE_LIMIT: usize = 256 << 20;
const EXECUTABLE_LIMIT: usize = 128 << 20;
const TAR_LIMIT: usize = 512 << 20;
const PAIR_TIMEOUT: Duration = Duration::from_secs(300);

/// Installations in this process get their own temporary file, so two
/// supervisors racing at startup cannot write the same bytes at once.
static INSTALLS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archive {
    TarGz(&'static str),
    Zip(&'static str),
}

impl Archive {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::TarGz(name) | Self::Zip(name) => name,
        }
    }
}

/// The official archive for a platform, if Spotify builds Soloist for it.
pub fn archive_for(os: &str, arch: &str) -> Option<Archive> {
    match (os, arch) {
        ("linux", "x86_64") => Some(Archive::TarGz("soloist_release_x86_64.tar.gz")),
        ("linux", "aarch64") => Some(Archive::TarGz("soloist_release_arm64.tar.gz")),
        ("linux", "arm") => Some(Archive::TarGz("soloist_release_arm32.tar.gz")),
        ("macos", "aarch64") => Some(Archive::TarGz("soloist_release_osx_arm64.tar.gz")),
        ("windows", "x86_64") => Some(Archive::Zip("soloist_release_windows_x86_64.zip")),
        _ => None,
    }
}

pub fn current_archive() -> Option<Archive> {
    archive_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn archive_url(archive: Archive) -> String {
    format!("{ARCHIVE_BASE}/{}", archive.file_name())
}

pub fn executable_name() -> &'static str {
    if cfg!(windows) {
        "soloist.exe"
    } else {
        "soloist"
    }
}

/// The name Soloist shows in Spotify Connect. The streaming session keeps
/// the listener's existing device name, so this one says which engine is
/// playing. A blank name still has to produce a name Spotify accepts.
pub fn connect_name(device_name: &str) -> String {
    let device_name = device_name.trim();
    if device_name.is_empty() {
        "Soloist".to_owned()
    } else {
        format!("{device_name} (Soloist)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Off,
    Unsupported(String),
    NeedsKey,
    Downloading,
    Starting,
    Running,
    Pairing,
    Failed(String),
}

/// A status change, or the end of a pairing attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Status(Status),
    Paired(Result<(), String>),
}

/// The files Soloist owns. The executable and its Connect session live in
/// durable state; only its audio cache is disposable.
pub struct Paths {
    pub executable: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    stamp: PathBuf,
}

pub fn paths(dirs: &AppDirs) -> Paths {
    let root = dirs.state.join("soloist");
    Paths {
        executable: root.join("bin").join(executable_name()),
        data: root.join("device"),
        cache: dirs.cache.join("soloist"),
        stamp: root.join("installed.json"),
    }
}

#[derive(Serialize, Deserialize)]
struct Stamp {
    version: u32,
    installed_at: u64,
    archive: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn fresh(stamp: &Stamp, archive: Archive, now: u64) -> bool {
    stamp.version == 1
        && stamp.archive == archive.file_name()
        && now.saturating_sub(stamp.installed_at) < REFRESH_AFTER.as_secs()
}

fn installed_stamp(path: &Path) -> Option<Stamp> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Download and install the current build, or reuse the installed one while
/// it is fresh. `force` ignores the stamp, for a build Soloist said expired.
pub async fn ensure(
    client: &reqwest::Client,
    dirs: &AppDirs,
    force: bool,
) -> Result<PathBuf, String> {
    let archive = current_archive()
        .ok_or_else(|| unsupported_reason(std::env::consts::OS, std::env::consts::ARCH))?;
    let paths = paths(dirs);
    if !force
        && paths.executable.is_file()
        && installed_stamp(&paths.stamp).is_some_and(|stamp| fresh(&stamp, archive, now_secs()))
    {
        return Ok(paths.executable);
    }

    let response = client
        .get(archive_url(archive))
        .send()
        .await
        .map_err(|error| format!("Couldn't download Spotify Soloist: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Couldn't download Spotify Soloist: the download server answered {}.",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Couldn't download Spotify Soloist: {error}"))?;
    if bytes.len() > ARCHIVE_LIMIT {
        return Err("The Spotify Soloist download was larger than expected.".into());
    }
    let data = match archive {
        Archive::TarGz(_) => extract_tar_gz(&bytes)?,
        Archive::Zip(_) => extract_zip(&bytes)?,
    };
    if data.len() > EXECUTABLE_LIMIT {
        return Err("The Spotify Soloist executable was larger than expected.".into());
    }

    let directory = paths
        .executable
        .parent()
        .ok_or_else(|| "The Spotify Soloist folder is unavailable.".to_string())?;
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| format!("Couldn't create the Spotify Soloist folder: {error}"))?;
    tokio::fs::create_dir_all(&paths.data)
        .await
        .map_err(|error| format!("Couldn't create the Spotify Soloist folder: {error}"))?;
    tokio::fs::create_dir_all(&paths.cache)
        .await
        .map_err(|error| format!("Couldn't create the Spotify Soloist cache: {error}"))?;
    let temporary = paths.executable.with_extension(format!(
        "download-{}-{}",
        std::process::id(),
        INSTALLS.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = install(&temporary, &paths.executable, &data).await {
        // A half-written download is no use on the next start.
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    let stamp = Stamp {
        version: 1,
        installed_at: now_secs(),
        archive: archive.file_name().to_owned(),
    };
    let bytes =
        serde_json::to_vec(&stamp).map_err(|_| "Couldn't record Spotify Soloist.".to_string())?;
    fastframe_file::write(&paths.stamp, &bytes)
        .map_err(|error| format!("Couldn't record Spotify Soloist: {error}"))?;
    Ok(paths.executable)
}

/// Write the executable beside its destination, then swap it in one step so
/// a failed download never leaves a partial build behind.
async fn install(temporary: &Path, executable: &Path, data: &[u8]) -> Result<(), String> {
    tokio::fs::write(temporary, data)
        .await
        .map_err(|error| format!("Couldn't save Spotify Soloist: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("Couldn't make Spotify Soloist executable: {error}"))?;
    }
    // Windows refuses to replace a running executable, which is why the
    // supervisor stops the old process before asking for a download.
    tokio::fs::rename(temporary, executable)
        .await
        .map_err(|error| format!("Couldn't install Spotify Soloist: {error}"))
}

fn unsupported_reason(os: &str, arch: &str) -> String {
    format!(
        "Spotify Soloist has no official build for {os} on {arch}. Local playback can still be enabled instead."
    )
}

/// The executable inside a `tar.gz` archive. Official builds hold one plain
/// `soloist` file; PAX and GNU long names are read so the lookup does not
/// depend on the archive writer.
fn extract_tar_gz(archive: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = Vec::new();
    decoder
        .take(TAR_LIMIT as u64 + 1)
        .read_to_end(&mut tar)
        .map_err(|error| format!("Couldn't open the Spotify Soloist archive: {error}"))?;
    if tar.len() > TAR_LIMIT {
        return Err("The Spotify Soloist archive was larger than expected.".into());
    }
    let wanted = executable_name();
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    let mut offset = 0usize;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let field = |start: usize, len: usize| -> String {
            let bytes = &header[start..start + len];
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        };
        let size = usize::from_str_radix(field(124, 12).trim(), 8)
            .map_err(|_| "The Spotify Soloist archive has an invalid entry.".to_string())?;
        let kind = header[156];
        let mut name = field(0, 100);
        let prefix = field(345, 155);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        if let Some(long) = long_name.take() {
            name = long;
        }
        if let Some(path) = pax_path.take() {
            name = path;
        }
        let start = offset + 512;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= tar.len())
            .ok_or_else(|| "The Spotify Soloist archive is truncated.".to_string())?;
        match kind {
            b'L' => {
                long_name = Some(
                    String::from_utf8_lossy(&tar[start..end])
                        .trim_end_matches('\0')
                        .to_owned(),
                );
            }
            b'x' => {
                pax_path = pax_record(&tar[start..end], "path");
            }
            b'0' | 0
                if Path::new(&name)
                    .file_name()
                    .is_some_and(|file| file == wanted) =>
            {
                return Ok(tar[start..end].to_vec());
            }
            _ => {}
        }
        offset = start + size.div_ceil(512) * 512;
    }
    Err(format!(
        "The Spotify Soloist archive does not contain {wanted}."
    ))
}

/// One `key=value` record from a PAX extended header, whose records are
/// `<length> <key>=<value>\n`.
fn pax_record(header: &[u8], key: &str) -> Option<String> {
    let text = std::str::from_utf8(header).ok()?;
    let mut rest = text;
    while !rest.is_empty() {
        let (length, _) = rest.split_once(' ')?;
        let length: usize = length.parse().ok()?;
        if length == 0 || length > rest.len() {
            return None;
        }
        let record = &rest[..length];
        if let Some(value) = record
            .strip_prefix(&format!("{length} {key}="))
            .map(|value| value.trim_end_matches('\n'))
        {
            return Some(value.to_owned());
        }
        rest = &rest[length..];
    }
    None
}

fn extract_zip(archive: &[u8]) -> Result<Vec<u8>, String> {
    let archive = crate::skin::zip::Archive::parse(archive)
        .map_err(|error| format!("Couldn't open the Spotify Soloist archive: {error}"))?;
    let wanted = executable_name();
    let entry = archive
        .entries()
        .iter()
        .find(|entry| entry.base_name() == wanted)
        .ok_or_else(|| format!("The Spotify Soloist archive does not contain {wanted}."))?;
    archive
        .read_limit(entry, EXECUTABLE_LIMIT)
        .map_err(|error| format!("Couldn't open the Spotify Soloist executable: {error}"))
}

/// Starts Soloist and keeps it running. A process that exits because its
/// build expired is replaced with a freshly downloaded one.
pub struct Runtime {
    dirs: AppDirs,
    updates: mpsc::UnboundedSender<Update>,
    status: Status,
    enabled: bool,
    device_name: String,
    key: Option<String>,
    stop: Option<watch::Sender<bool>>,
}

impl Runtime {
    pub fn new(dirs: AppDirs, updates: mpsc::UnboundedSender<Update>) -> Self {
        Self {
            dirs,
            updates,
            status: Status::Off,
            enabled: false,
            device_name: String::new(),
            key: None,
            stop: None,
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Report a failure found outside the download and process code, such as
    /// a network client that could not be built.
    pub fn fail(&mut self, message: String) {
        self.stop();
        self.set_status(Status::Failed(message));
    }

    /// Apply the settings and key the backend knows. Nothing restarts when
    /// none of them changed.
    pub fn configure(
        &mut self,
        enabled: bool,
        device_name: String,
        key: Option<String>,
        client: reqwest::Client,
    ) {
        if self.enabled == enabled && self.device_name == device_name && self.key == key {
            return;
        }
        self.stop();
        self.enabled = enabled;
        self.device_name = device_name;
        self.key = key;
        self.start(client);
    }

    /// Stop and start again, for a failed process or a finished pairing.
    pub fn restart(&mut self, client: reqwest::Client) {
        self.stop();
        self.start(client);
    }

    fn start(&mut self, client: reqwest::Client) {
        if !self.enabled {
            self.set_status(Status::Off);
            return;
        }
        if current_archive().is_none() {
            self.set_status(Status::Unsupported(unsupported_reason(
                std::env::consts::OS,
                std::env::consts::ARCH,
            )));
            return;
        }
        let Some(key) = self.key.clone().filter(|key| !key.trim().is_empty()) else {
            self.set_status(Status::NeedsKey);
            return;
        };
        let (stop, stopped) = watch::channel(false);
        self.stop = Some(stop);
        let dirs = self.dirs.clone();
        let updates = self.updates.clone();
        let device_name = connect_name(&self.device_name);
        tokio::spawn(async move {
            supervise(dirs, device_name, key, client, updates, stopped).await;
        });
        self.set_status(Status::Downloading);
    }

    /// Pair through the Spotify app, then let the caller start playback
    /// again. Pairing stops the daemon so both use the same session folder.
    pub fn pair(&mut self, client: reqwest::Client) {
        self.stop();
        if !self.enabled {
            self.set_status(Status::Off);
            return;
        }
        if current_archive().is_none() {
            self.set_status(Status::Unsupported(unsupported_reason(
                std::env::consts::OS,
                std::env::consts::ARCH,
            )));
            return;
        }
        let Some(key) = self.key.clone().filter(|key| !key.trim().is_empty()) else {
            self.set_status(Status::NeedsKey);
            return;
        };
        self.set_status(Status::Pairing);
        let dirs = self.dirs.clone();
        let updates = self.updates.clone();
        let device_name = connect_name(&self.device_name);
        tokio::spawn(async move {
            let result = pair_inner(&client, &dirs, &device_name, &key).await;
            let _ = updates.send(Update::Paired(result));
        });
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
    }

    fn set_status(&mut self, status: Status) {
        self.status = status.clone();
        let _ = self.updates.send(Update::Status(status));
    }
}

async fn supervise(
    dirs: AppDirs,
    device_name: String,
    key: String,
    client: reqwest::Client,
    updates: mpsc::UnboundedSender<Update>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut force = false;
    loop {
        send(&updates, Status::Downloading);
        let installed = paths(&dirs);
        let executable = match ensure(&client, &dirs, force).await {
            Ok(path) => path,
            // A scheduled refresh failed, but the build on disk is only
            // replaced after 60 days of a 90-day life. Play with it instead
            // of stopping because Spotify could not be reached.
            Err(_) if !force && installed.executable.is_file() => installed.executable.clone(),
            Err(error) => {
                send(&updates, Status::Failed(error));
                return;
            }
        };
        send(&updates, Status::Starting);
        let mut command = tokio::process::Command::new(&executable);
        command
            .arg("--device-name")
            .arg(&device_name)
            .arg("--api-key")
            .arg(&key)
            .arg("--data-dir")
            .arg(&installed.data)
            .arg("--cache-dir")
            .arg(&installed.cache)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Soloist's own log could repeat the API key it was given.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                send(
                    &updates,
                    Status::Failed(format!("Couldn't start Spotify Soloist: {error}")),
                );
                return;
            }
        };
        send(&updates, Status::Running);
        let exit = tokio::select! {
            changed = stopped.changed() => {
                let _ = child.kill().await;
                let _ = changed;
                return;
            }
            exit = child.wait() => exit,
        };
        match exit {
            // Spotify's documented sign that the downloaded build expired.
            Ok(status) if status.code() == Some(10) && !force => {
                force = true;
                continue;
            }
            Ok(status) => {
                let code = status.code().map_or_else(
                    || "without an exit code".to_owned(),
                    |code| format!("with exit code {code}"),
                );
                send(
                    &updates,
                    Status::Failed(format!(
                        "Spotify Soloist stopped {code}. Pair this device again, or check the API key."
                    )),
                );
                return;
            }
            Err(error) => {
                send(
                    &updates,
                    Status::Failed(format!("Couldn't watch Spotify Soloist: {error}")),
                );
                return;
            }
        }
    }
}

fn send(updates: &mpsc::UnboundedSender<Update>, status: Status) {
    let _ = updates.send(Update::Status(status));
}

async fn pair_inner(
    client: &reqwest::Client,
    dirs: &AppDirs,
    device_name: &str,
    key: &str,
) -> Result<(), String> {
    let executable = ensure(client, dirs, false).await?;
    let paths = paths(dirs);
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--pair")
        .arg("--device-name")
        .arg(device_name)
        .arg("--api-key")
        .arg(key)
        .arg("--data-dir")
        .arg(&paths.data)
        .arg("--cache-dir")
        .arg(&paths.cache)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(PAIR_TIMEOUT, command.status())
        .await
        .map_err(|_| {
            "Pairing timed out. Choose the Soloist device in the Spotify app, then try again."
                .to_string()
        })?
        .map_err(|error| format!("Couldn't start Spotify Soloist: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        let code = status.code().map_or_else(
            || "without an exit code".to_owned(),
            |code| format!("with exit code {code}"),
        );
        Err(format!(
            "Pairing ended {code}. Choose the Soloist device in the Spotify app."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut tar = Vec::new();
        for (name, data, kind) in entries {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            let size = format!("{:011o}\0", data.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[156] = *kind;
            header[257..263].copy_from_slice(b"ustar\0");
            let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
            let checksum = format!("{:06o}\0 ", sum);
            header[148..156].copy_from_slice(checksum.as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(data);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn every_official_build_maps_to_its_archive() {
        assert_eq!(
            archive_for("linux", "x86_64"),
            Some(Archive::TarGz("soloist_release_x86_64.tar.gz"))
        );
        assert_eq!(
            archive_for("linux", "aarch64"),
            Some(Archive::TarGz("soloist_release_arm64.tar.gz"))
        );
        assert_eq!(
            archive_for("linux", "arm"),
            Some(Archive::TarGz("soloist_release_arm32.tar.gz"))
        );
        assert_eq!(
            archive_for("macos", "aarch64"),
            Some(Archive::TarGz("soloist_release_osx_arm64.tar.gz"))
        );
        assert_eq!(
            archive_for("windows", "x86_64"),
            Some(Archive::Zip("soloist_release_windows_x86_64.zip"))
        );
        assert_eq!(archive_for("macos", "x86_64"), None);
        assert_eq!(archive_for("windows", "aarch64"), None);
        assert_eq!(archive_for("linux", "riscv64"), None);
    }

    #[test]
    fn the_archive_url_is_spotifys_own() {
        assert_eq!(
            archive_url(Archive::TarGz("soloist_release_arm64.tar.gz")),
            "https://soloist-builds.spotifycdn.com/soloist_release_arm64.tar.gz"
        );
    }

    #[test]
    fn a_tar_gz_archive_yields_its_executable() {
        let archive = tar(&[
            ("README.txt", b"notes", b'0'),
            (executable_name(), b"binary", b'0'),
        ]);
        assert_eq!(extract_tar_gz(&archive).unwrap(), b"binary");
    }

    #[test]
    fn a_tar_gz_long_name_still_finds_the_executable() {
        let long = format!("some/deep/folder/{}", executable_name());
        let archive = tar(&[
            ("././@LongLink", long.as_bytes(), b'L'),
            (&long, b"binary", b'0'),
        ]);
        assert_eq!(extract_tar_gz(&archive).unwrap(), b"binary");
    }

    #[test]
    fn a_pax_path_still_finds_the_executable() {
        let long = format!("some/deep/folder/{}", executable_name());
        let mut length = 0;
        let record = loop {
            let candidate = format!("{length} path={long}\n");
            if candidate.len() == length {
                break candidate;
            }
            length = candidate.len();
        };
        let archive = tar(&[
            ("PaxHeader", record.as_bytes(), b'x'),
            ("placeholder", b"binary", b'0'),
        ]);
        assert_eq!(extract_tar_gz(&archive).unwrap(), b"binary");
    }

    #[test]
    fn an_archive_without_the_executable_is_refused() {
        let archive = tar(&[("README.txt", b"notes", b'0')]);
        assert!(extract_tar_gz(&archive).is_err());
    }

    #[test]
    fn a_fresh_stamp_only_matches_the_same_platform() {
        let stamp = Stamp {
            version: 1,
            installed_at: 1_000,
            archive: "soloist_release_x86_64.tar.gz".into(),
        };
        let archive = Archive::TarGz("soloist_release_x86_64.tar.gz");
        assert!(fresh(&stamp, archive, 1_000 + REFRESH_AFTER.as_secs() - 1));
        assert!(!fresh(&stamp, archive, 1_000 + REFRESH_AFTER.as_secs()));
        assert!(!fresh(
            &stamp,
            Archive::TarGz("soloist_release_arm64.tar.gz"),
            1_000
        ));
    }

    #[test]
    fn the_soloist_device_says_which_engine_plays() {
        assert_eq!(connect_name("Spotifast"), "Spotifast (Soloist)");
        assert_eq!(connect_name("  Desk  "), "Desk (Soloist)");
        assert_eq!(connect_name("   "), "Soloist");
    }
}
