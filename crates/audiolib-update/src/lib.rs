// crates/audiolib-update/src/lib.rs
//! GitHub release discovery, verified staging and transactional application updates.
//! Call blocking methods outside GUI/HTTP event loops. Libraries and preferences are
//! never update targets. Windows handoff waits for graceful process exit; Unix
//! servers can replace and exec themselves after draining their runtime.
mod deploy;
mod package;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path, time::Duration};

pub use deploy::{PreparedUpdate, handle_update_helper};

const REPOSITORY: &str = "Freedragonlancer/AudioLib-Explorer";
const RELEASES_API: &str =
    "https://api.github.com/repos/Freedragonlancer/AudioLib-Explorer/releases";
const MAX_JSON: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const EMPTY_LIST_RETRIES: u32 = 2;

/// Selects a product's exact release asset, independent of the active library.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Product {
    /// Windows x64 portable GUI and CLI.
    Desktop,
    /// Native Windows/Linux x64 or macOS arm64 server.
    Server,
}

/// Stable never receives prereleases; beta accepts beta/rc releases and stable.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Channel {
    /// Published stable releases only.
    Stable,
    /// Stable and supported beta/rc prereleases, ordered by semantic version.
    Beta,
}

impl Channel {
    /// Installation marker takes priority; a prerelease binary otherwise uses beta.
    pub fn for_install(directory: &Path, current_version: &str) -> Result<Self> {
        let path = directory.join("update-channel.txt");
        match std::fs::read_to_string(path) {
            Ok(text) => match text.trim() {
                "stable" => Ok(Self::Stable),
                "beta" => Ok(Self::Beta),
                _ => bail!("Invalid installed update channel"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(if Version::parse(current_version)?.pre.is_empty() {
                    Self::Stable
                } else {
                    Self::Beta
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn marker(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}

/// A supported release with the product archive and its published checksum.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseUpdate {
    /// Full GitHub tag including `v`.
    pub tag: String,
    /// Full semantic version including any prerelease suffix.
    pub version: String,
    /// Public GitHub release page.
    pub release_url: String,
    /// Release notes (bounded with the release listing).
    pub notes: String,
    /// Why a newer release was not offered, e.g. `v0.7.0` had no valid package for
    /// this platform and this older release was selected instead; `None` otherwise.
    #[serde(default)]
    pub skipped_newer: Option<String>,
    /// Every newer release (newest first) that was skipped in favour of this one.
    #[serde(default)]
    pub skipped_releases: Vec<SkippedRelease>,
    pub(crate) asset: Asset,
    pub(crate) checksum: Asset,
}

/// A newer release the channel would accept but that was not offered, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRelease {
    /// Full GitHub tag including `v`.
    pub tag: String,
    /// Human-readable reason the release was skipped.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

/// Blocking, bounded GitHub updater using the operating system's TLS trust roots.
pub struct UpdateClient {
    product: Product,
    channel: Channel,
    current: Version,
    agent: ureq::Agent,
    /// GitHub releases API endpoint (without query); defaults to this repository's.
    releases_api: String,
}

impl UpdateClient {
    /// Full installed version, including the marker's prerelease and build metadata.
    pub fn current_version(&self) -> String {
        self.current.to_string()
    }

    /// Whether the installed version uses the local-development prerelease `0`.
    /// This recognizes both legacy `X.Y.Z-0` markers and identified local builds
    /// such as `X.Y.Z-0+local.<exe_sha12>`. Stable and tagged beta/rc releases are
    /// not development installations. Callers choose their update policy; this
    /// predicate does not change release discovery, selection or preparation.
    pub fn is_development(&self) -> bool {
        self.current.pre.as_str() == "0"
    }

    /// Update channel selected by this installation.
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Replace the releases API endpoint (`?per_page=100&page=N` is appended). The
    /// production agent still only accepts HTTPS; intended for mirrors and tests.
    pub fn with_releases_api(mut self, url: impl Into<String>) -> Self {
        self.releases_api = url.into();
        self
    }

    /// The releases API endpoint in use.
    pub fn releases_api(&self) -> &str {
        &self.releases_api
    }

    /// Creates a release client without performing network I/O.
    pub fn new(product: Product, channel: Channel, current_version: &str) -> Result<Self> {
        Ok(Self {
            product,
            channel,
            current: Version::parse(current_version)?,
            releases_api: RELEASES_API.to_owned(),
            agent: ureq::Agent::config_builder()
                .https_only(true)
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .provider(ureq::tls::TlsProvider::Rustls)
                        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                        .build(),
                )
                .max_redirects(5)
                .timeout_connect(Some(Duration::from_secs(10)))
                .timeout_global(Some(Duration::from_secs(600)))
                .build()
                .into(),
        })
    }

    /// Reads package markers so prerelease tags remain distinguishable even when
    /// the executable's Cargo version contains only MAJOR.MINOR.PATCH.
    pub fn for_install(product: Product, current_version: &str, directory: &Path) -> Result<Self> {
        let binary_version = Version::parse(current_version)?;
        let installed = match std::fs::read_to_string(directory.join("update-version.txt")) {
            Ok(text) => {
                let version = Version::parse(text.trim())?;
                ensure!(
                    (version.major, version.minor, version.patch)
                        == (
                            binary_version.major,
                            binary_version.minor,
                            binary_version.patch
                        ),
                    "Installed update version does not match this executable"
                );
                version.to_string()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current_version.to_owned()
            }
            Err(error) => return Err(error.into()),
        };
        Self::new(
            product,
            Channel::for_install(directory, &installed)?,
            &installed,
        )
    }

    /// Queries published releases on every call; callers run this once at startup.
    /// Offline/rate-limit failures are ordinary errors and must not block startup.
    pub fn check(&self) -> Result<Option<ReleaseUpdate>> {
        let mut releases = Vec::new();
        // Paginate instead of relying on /latest, which excludes beta releases.
        // The finite bound prevents an untrusted/huge repository from stalling startup.
        for page in 1..=5 {
            let url = format!("{}?per_page=100&page={page}", self.releases_api);
            let mut batch = self.fetch_releases(&url)?;
            // GitHub (or an intermediate proxy) intermittently answers the first page
            // with 200 and `[]`; this repository always has releases, so an empty first
            // page is a failed lookup, never "up to date".
            for attempt in 1..=EMPTY_LIST_RETRIES {
                if page != 1 || !batch.is_empty() {
                    break;
                }
                tracing::warn!(
                    attempt,
                    "update check: GitHub returned an empty release list; retrying"
                );
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
                batch = self.fetch_releases(&url)?;
            }
            ensure!(
                page != 1 || !batch.is_empty(),
                "GitHub returned an empty release list; the update check could not complete"
            );
            let done = batch.len() < 100;
            releases.extend(batch);
            if done {
                break;
            }
        }
        self.select(releases)
    }

    fn fetch_releases(&self, url: &str) -> Result<Vec<Release>> {
        let bytes = self.fetch(url, MAX_JSON, Duration::from_secs(20))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn select(&self, releases: Vec<Release>) -> Result<Option<ReleaseUpdate>> {
        self.select_for_platform(releases, native_platform()?)
    }

    fn select_for_platform(
        &self,
        releases: Vec<Release>,
        platform: Platform,
    ) -> Result<Option<ReleaseUpdate>> {
        let total = releases.len();
        let mut selected: Option<(Version, ReleaseUpdate)> = None;
        // Newer releases this channel would accept but whose assets are missing or invalid.
        let mut unusable: Option<Version> = None;
        let mut skipped: Vec<(Version, String)> = Vec::new();
        for release in releases {
            if release.draft {
                continue;
            }
            let Some(text) = release.tag_name.strip_prefix('v') else {
                continue;
            };
            let Ok(version) = Version::parse(text) else {
                continue;
            };
            if version <= self.current
                || !version.build.is_empty()
                || (self.channel == Channel::Stable
                    && (release.prerelease || !version.pre.is_empty()))
                || !supported_prerelease(&version)
            {
                continue;
            }
            if selected.as_ref().is_some_and(|(best, _)| best >= &version) {
                continue;
            }
            let names = asset_names_for_platform(self.product, &release.tag_name, platform)?;
            let Some((asset, checksum)) = names.iter().find_map(|name| {
                let asset = unique_asset(&release.assets, name)?;
                let checksum = unique_asset(&release.assets, &format!("{name}.sha256"))?;
                (asset.size > 0
                    && asset.size <= MAX_ARCHIVE
                    && checksum.size <= 4096
                    && valid_asset_url(asset, &release.tag_name)
                    && valid_asset_url(checksum, &release.tag_name))
                .then_some((asset, checksum))
            }) else {
                tracing::warn!(
                    tag = %release.tag_name,
                    "update check: newer release has no valid package for this platform; skipped"
                );
                skipped.push((version.clone(), release.tag_name.clone()));
                if unusable.as_ref().is_none_or(|worst| &version > worst) {
                    unusable = Some(version);
                }
                continue;
            };
            selected = Some((
                version.clone(),
                ReleaseUpdate {
                    tag: release.tag_name.clone(),
                    version: version.to_string(),
                    release_url: format!(
                        "https://github.com/{REPOSITORY}/releases/tag/{}",
                        release.tag_name
                    ),
                    notes: release.body.unwrap_or_default(),
                    skipped_newer: None,
                    skipped_releases: Vec::new(),
                    asset: asset.clone(),
                    checksum: checksum.clone(),
                },
            ));
        }
        match (&selected, &unusable) {
            (None, Some(version)) => bail!(
                "Release v{version} is newer but has no valid package for this platform; the update cannot be installed yet"
            ),
            (Some((chosen, _)), Some(version)) if version > chosen => tracing::warn!(
                chosen = %chosen,
                skipped = %version,
                "update check: offering an older release because the newest has no valid package"
            ),
            _ => {}
        }
        if let (Some((chosen, update)), Some(version)) = (selected.as_mut(), unusable.as_ref())
            && *version > *chosen
        {
            update.skipped_newer = Some(format!(
                "Release v{version} is newer but has no valid package for this platform; offering v{chosen}"
            ));
        }
        if let Some((chosen, update)) = selected.as_mut() {
            skipped.retain(|(version, _)| version > chosen);
            skipped.sort_by(|a, b| b.0.cmp(&a.0));
            update.skipped_releases = skipped
                .into_iter()
                .map(|(_, tag)| SkippedRelease {
                    tag,
                    reason: "no valid package for this platform".into(),
                })
                .collect();
        }
        tracing::info!(
            releases = total,
            current = %self.current,
            channel = self.channel.marker(),
            selected = selected.as_ref().map(|(v, _)| v.to_string()).as_deref().unwrap_or("none"),
            "update check completed"
        );
        Ok(selected.map(|(_, update)| update))
    }

    /// Downloads and verifies a release into an owned directory inside the installation.
    /// Does not modify installed files; dropping the result deletes unused staging.
    pub fn prepare(&self, update: &ReleaseUpdate, directory: &Path) -> Result<PreparedUpdate> {
        let expected = &update.asset.name;
        ensure!(
            asset_names(self.product, &update.tag)?.contains(expected)
                && update.checksum.name == format!("{expected}.sha256")
                && valid_asset_url(&update.asset, &update.tag)
                && valid_asset_url(&update.checksum, &update.tag),
            "Invalid release asset identity"
        );
        let version = Version::parse(
            update
                .tag
                .strip_prefix('v')
                .context("Invalid release tag")?,
        )?;
        ensure!(
            version > self.current
                && version.to_string() == update.version
                && supported_prerelease(&version),
            "Invalid update version"
        );
        ensure!(
            self.channel == Channel::Beta || version.pre.is_empty(),
            "Prerelease on stable channel"
        );
        deploy::validate_install(directory)?;
        let stage = tempfile::Builder::new()
            .prefix(".audiolib-update-")
            .tempdir_in(directory)?;
        let prepared = PreparedUpdate::new(stage, directory, self.product)?;
        let stage = prepared.stage_root();
        let checksum = self.fetch(
            &update.checksum.browser_download_url,
            4096,
            Duration::from_secs(30),
        )?;
        let hash = package::parse_checksum(&checksum, expected)?;
        if let Some(digest) = &update.asset.digest {
            ensure!(
                digest.eq_ignore_ascii_case(&format!("sha256:{hash}")),
                "GitHub digest disagrees with release checksum"
            );
        }
        let archive = stage.join("download");
        self.download(&update.asset, &archive, &hash)?;
        let payload = stage.join("payload");
        std::fs::create_dir(&payload)?;
        package::extract(&archive, &payload, self.product, expected)?;
        std::fs::remove_file(&archive)?;
        if let Ok(text) = std::fs::read_to_string(payload.join("update-version.txt")) {
            ensure!(
                text.trim() == update.version,
                "Package update version does not match release"
            );
        }
        std::fs::write(
            payload.join("update-version.txt"),
            format!("{}\n", update.version),
        )?;
        std::fs::write(
            payload.join("update-channel.txt"),
            format!("{}\n", self.channel.marker()),
        )?;
        Ok(prepared)
    }

    fn fetch(&self, url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", "AudioLibExplorer-Updater")
            .header("Accept", "application/vnd.github+json")
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= limit,
            "Update response exceeds its size limit"
        );
        Ok(bytes)
    }

    fn download(&self, asset: &Asset, destination: &Path, expected: &str) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        ensure!(
            asset.size > 0 && asset.size <= MAX_ARCHIVE,
            "Invalid archive size"
        );
        let mut response = self
            .agent
            .get(&asset.browser_download_url)
            .header("User-Agent", "AudioLibExplorer-Updater")
            .call()?;
        let mut reader = response.body_mut().as_reader().take(asset.size + 1);
        let mut file = std::fs::File::create_new(destination)?;
        let mut digest = Sha256::new();
        let mut total = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(
                total <= asset.size,
                "Release archive exceeds its declared size"
            );
            file.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
        }
        file.sync_all()?;
        ensure!(total == asset.size, "Release archive is truncated");
        ensure!(
            format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected),
            "Release archive SHA256 mismatch"
        );
        Ok(())
    }
}

fn supported_prerelease(version: &Version) -> bool {
    if version.pre.is_empty() {
        return true;
    }
    let text = version.pre.as_str();
    text.strip_prefix("beta.")
        .or_else(|| text.strip_prefix("rc."))
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn unique_asset<'a>(assets: &'a [Asset], name: &str) -> Option<&'a Asset> {
    let mut found = assets.iter().filter(|asset| asset.name == name);
    let item = found.next()?;
    found.next().is_none().then_some(item)
}

fn valid_asset_url(asset: &Asset, tag: &str) -> bool {
    asset.browser_download_url
        == format!(
            "https://github.com/{REPOSITORY}/releases/download/{tag}/{}",
            asset.name
        )
}

#[derive(Clone, Copy)]
enum Platform {
    WindowsX64,
    LinuxX64,
    MacArm64,
}

fn native_platform() -> Result<Platform> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok(Platform::WindowsX64)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok(Platform::LinuxX64)
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(Platform::MacArm64)
    } else {
        bail!("Automatic updates are unavailable for this platform")
    }
}

fn asset_names(product: Product, tag: &str) -> Result<Vec<String>> {
    asset_names_for_platform(product, tag, native_platform()?)
}

fn asset_names_for_platform(
    product: Product,
    tag: &str,
    platform: Platform,
) -> Result<Vec<String>> {
    let version = Version::parse(
        tag.strip_prefix('v')
            .context("Release tag requires v prefix")?,
    )?;
    ensure!(
        supported_prerelease(&version) && version.build.is_empty(),
        "Unsupported release tag"
    );
    let canonical = match (product, platform) {
        (Product::Desktop, Platform::WindowsX64) => {
            format!("AudioLibExplorer-{tag}-win-x64-portable.zip")
        }
        (Product::Server, Platform::WindowsX64) => {
            format!("AudioLibServer-{tag}-win-x64.zip")
        }
        (Product::Server, Platform::LinuxX64) => {
            format!("AudioLibServer-{tag}-linux-x64.tar.gz")
        }
        (Product::Server, Platform::MacArm64) => {
            format!("AudioLibServer-{tag}-macos-arm64.tar.gz")
        }
        _ => bail!("Automatic updates are unavailable for this product/platform"),
    };
    let legacy = match product {
        Product::Desktop => canonical.replacen("AudioLibExplorer-v", "AudioLibExplorer-", 1),
        Product::Server => canonical.replacen("AudioLibServer-v", "audiolib-server-", 1),
    };
    let mut names = vec![canonical.clone()];
    // These published macOS archives contain a version-named directory with only
    // the executable (and sometimes AppleDouble files), not a public server payload.
    // Do not advertise a download the strict extractor will inevitably reject.
    let binary_only_legacy = product == Product::Server
        && matches!(platform, Platform::MacArm64)
        && matches!(tag, "v0.5.0" | "v0.5.1" | "v0.6.0" | "v0.6.1" | "v0.6.2");
    if !binary_only_legacy {
        names.push(legacy);
    }
    if product == Product::Server {
        names.push(canonical.replacen("AudioLibServer-v", "AudioLibServer-", 1));
    }
    Ok(names)
}
