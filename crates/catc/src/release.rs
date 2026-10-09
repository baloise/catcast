//! Latest-release lookup.
//!
//! `GET {base}/releases/latest` on GitHub (and on anything mimicking it)
//! answers with a redirect to `{base}/releases/tag/vX.Y.Z`. Reading the
//! `Location` header yields the version without the REST API: no token, no
//! rate limit, no JSON. Only the tail of the header is parsed, so a `base`
//! that points at a catproxy URL (which rewrites `Location` to its own
//! origin) works too.

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use std::time::Duration;

/// Generous enough for a cold TLS handshake through a corporate proxy, short
/// enough that an unreachable release host does not make `--probe` crawl.
const TIMEOUT: Duration = Duration::from_secs(4);

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .user_agent(concat!("catc/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")
}

/// `None` when lookups are disabled (`update_url = ""`), otherwise the
/// outcome of one request to `{base}/releases/latest`.
pub async fn lookup(base: &str) -> Option<Result<Version>> {
    if base.trim().is_empty() {
        return None;
    }
    Some(async { latest_version(&client()?, base).await }.await)
}

pub async fn latest_version(client: &reqwest::Client, base: &str) -> Result<Version> {
    let url = format!("{}/releases/latest", base.trim_end_matches('/'));
    let res = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if status.as_u16() == 404 {
        bail!("no releases found at {base}");
    }
    if !status.is_redirection() {
        bail!("{url}: expected a redirect to the latest tag, got HTTP {status}");
    }
    let loc = res
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("{url}: redirect without a Location header"))?;
    version_from_location(loc)
        .ok_or_else(|| anyhow!("{url}: no version in redirect target {loc:?}"))
}

/// Parse `.../releases/tag/v1.2.3` (with or without the `v`, with or
/// without query/fragment) into a version.
pub fn version_from_location(loc: &str) -> Option<Version> {
    let (_, tail) = loc.rsplit_once("/releases/tag/")?;
    let tag = tail.split(['?', '#']).next()?.trim_end_matches('/');
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Outdated,
    Current,
    Ahead,
    /// The installed version string did not parse as semver.
    Unknown,
}

pub fn freshness(installed: &str, latest: &Version) -> Freshness {
    let installed = installed.trim();
    let Ok(v) = Version::parse(installed.strip_prefix('v').unwrap_or(installed)) else {
        return Freshness::Unknown;
    };
    match v.cmp(latest) {
        std::cmp::Ordering::Less => Freshness::Outdated,
        std::cmp::Ordering::Equal => Freshness::Current,
        std::cmp::Ordering::Greater => Freshness::Ahead,
    }
}

impl Freshness {
    /// Short status for a table column.
    pub fn label(self, latest: &Version) -> String {
        match self {
            Freshness::Outdated => format!("outdated → v{latest}"),
            Freshness::Current => "current".into(),
            Freshness::Ahead => "ahead of latest".into(),
            Freshness::Unknown => "unknown version".into(),
        }
    }
}

/// Which binary a release asset is for. Both share the same naming and
/// signing; only the prefix differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    Catstage,
    Catc,
}

impl Binary {
    fn prefix(self) -> &'static str {
        match self {
            Binary::Catstage => "catstage",
            Binary::Catc => "catc",
        }
    }
}

/// Release asset name for a binary + platform tag (`windows-x64` → `.exe`).
pub fn asset_name_of(which: Binary, platform: &str) -> String {
    let ext = if platform.starts_with("windows") {
        ".exe"
    } else {
        ""
    };
    format!("{}-{platform}{ext}", which.prefix())
}

pub fn asset_url_of(base: &str, which: Binary, version: &Version, platform: &str) -> String {
    format!(
        "{}/releases/download/v{version}/{}",
        base.trim_end_matches('/'),
        asset_name_of(which, platform)
    )
}

pub fn asset_url(base: &str, version: &Version, platform: &str) -> String {
    asset_url_of(base, Binary::Catstage, version, platform)
}

/// The same asset through catproxy's base64 route.
pub fn proxied_url(proxy: &str, raw: &str) -> String {
    format!("{}/b64/{raw}", proxy.trim_end_matches('/'))
}

/// First 64-hex token of a `sha256sum`-style line (or a bare hash).
pub fn parse_sha256(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|t| t.to_ascii_lowercase())
}

async fn get_text(url: &str) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("catc/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let res = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if res.status().as_u16() == 404 {
        return Ok(None);
    }
    let res = res
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    Ok(Some(
        res.text().await.with_context(|| format!("read {url}"))?,
    ))
}

/// Fetch `<asset>.sha256` published next to a binary's release asset.
pub async fn fetch_sha256_of(
    base: &str,
    which: Binary,
    version: &Version,
    platform: &str,
) -> Result<String> {
    let url = format!("{}.sha256", asset_url_of(base, which, version, platform));
    let body = get_text(&url).await?.ok_or_else(|| {
        anyhow!(
            "{url} not found — releases before the checksum sidecars cannot be pushed remotely;              download the binary yourself and pass --url/--sha256"
        )
    })?;
    parse_sha256(&body).ok_or_else(|| anyhow!("{url}: no sha256 in {body:?}"))
}

pub async fn fetch_sha256(base: &str, version: &Version, platform: &str) -> Result<String> {
    fetch_sha256_of(base, Binary::Catstage, version, platform).await
}

/// Fetch `<asset>.sig` (hex Ed25519) next to a binary's release asset.
/// `None` when the release predates signing (no sidecar), so callers can
/// explain rather than fail cryptically.
pub async fn fetch_sig_of(
    base: &str,
    which: Binary,
    version: &Version,
    platform: &str,
) -> Result<Option<String>> {
    let url = format!("{}.sig", asset_url_of(base, which, version, platform));
    let Some(body) = get_text(&url).await? else {
        return Ok(None);
    };
    let hex: String = body.split_whitespace().collect();
    if hex.len() == 128 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(Some(hex))
    } else {
        bail!("{url}: not a 64-byte hex signature ({:?})", body)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Update,
    Skip(String),
}

/// Whether a stage should receive `target`, given what its last `State`
/// said. Policy lives here, on the operator side: the stage itself accepts
/// whatever an authenticated command names.
pub fn plan_target(state: Option<&catcast_core::State>, target: &Version, force: bool) -> Decision {
    let Some(st) = state else {
        return Decision::Skip("offline (no reply)".into());
    };
    if st.platform.is_empty() {
        return Decision::Skip(format!(
            "v{} predates remote update — swap the binary by hand once",
            st.version
        ));
    }
    match freshness(&st.version, target) {
        Freshness::Current if !force => {
            Decision::Skip(format!("already v{target} (--force to reinstall)"))
        }
        Freshness::Ahead if !force => Decision::Skip(format!(
            "would downgrade v{} → v{target} (--force to allow)",
            st.version
        )),
        Freshness::Unknown if !force => Decision::Skip(format!(
            "unrecognised version {:?} (--force to override)",
            st.version
        )),
        _ => Decision::Update,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(version: &str, platform: &str) -> catcast_core::State {
        let mut s = catcast_core::State::fresh("k", version);
        s.platform = platform.into();
        s
    }

    #[test]
    fn asset_urls() {
        let v = v("0.2.0");
        assert_eq!(
            asset_name_of(Binary::Catstage, "windows-x64"),
            "catstage-windows-x64.exe"
        );
        assert_eq!(
            asset_name_of(Binary::Catstage, "linux-x64"),
            "catstage-linux-x64"
        );
        assert_eq!(
            asset_name_of(Binary::Catc, "windows-x64"),
            "catc-windows-x64.exe"
        );
        assert_eq!(asset_name_of(Binary::Catc, "linux-x64"), "catc-linux-x64");
        assert_eq!(
            asset_url("https://github.com/baloise/catcast/", &v, "windows-x64"),
            "https://github.com/baloise/catcast/releases/download/v0.2.0/catstage-windows-x64.exe"
        );
        assert_eq!(
            proxied_url("https://p.workers.dev/", "https://github.com/x/y/f.exe"),
            "https://p.workers.dev/b64/https://github.com/x/y/f.exe"
        );
    }

    #[test]
    fn sha256_lines() {
        let h = "ab".repeat(32);
        assert_eq!(
            parse_sha256(&format!("{h}  catstage-linux-x64\n")),
            Some(h.clone())
        );
        assert_eq!(parse_sha256(&h.to_uppercase()), Some(h.clone()));
        assert_eq!(parse_sha256("not a hash"), None);
        assert_eq!(parse_sha256(&"zz".repeat(32)), None);
    }

    #[test]
    fn decision_table() {
        let target = v("0.2.0");
        assert!(matches!(
            plan_target(None, &target, false),
            Decision::Skip(_)
        ));
        assert!(matches!(
            plan_target(Some(&state("0.1.0", "")), &target, true),
            Decision::Skip(m) if m.contains("by hand")
        ));
        assert_eq!(
            plan_target(Some(&state("0.1.0", "linux-x64")), &target, false),
            Decision::Update
        );
        assert!(matches!(
            plan_target(Some(&state("0.2.0", "linux-x64")), &target, false),
            Decision::Skip(m) if m.contains("already")
        ));
        assert_eq!(
            plan_target(Some(&state("0.2.0", "linux-x64")), &target, true),
            Decision::Update
        );
        assert!(matches!(
            plan_target(Some(&state("0.3.0", "linux-x64")), &target, false),
            Decision::Skip(m) if m.contains("downgrade")
        ));
        assert_eq!(
            plan_target(Some(&state("0.3.0", "linux-x64")), &target, true),
            Decision::Update
        );
        assert!(matches!(
            plan_target(Some(&state("dev", "linux-x64")), &target, false),
            Decision::Skip(_)
        ));
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_github_and_proxied_locations() {
        assert_eq!(
            version_from_location("https://github.com/baloise/catcast/releases/tag/v0.1.1"),
            Some(v("0.1.1"))
        );
        // catproxy rewrites Location onto its own origin; the tail survives.
        assert_eq!(
            version_from_location(
                "https://catproxy.example.workers.dev/https://github.com/baloise/catcast/releases/tag/v0.2.0"
            ),
            Some(v("0.2.0"))
        );
        assert_eq!(
            version_from_location("/baloise/catcast/releases/tag/1.0.0?x=1#frag"),
            Some(v("1.0.0"))
        );
        assert_eq!(
            version_from_location("https://github.com/x/y/releases/tag/v0.2.0-rc.1"),
            Some(v("0.2.0-rc.1"))
        );
    }

    #[test]
    fn rejects_garbage_locations() {
        assert_eq!(
            version_from_location("https://github.com/x/y/releases"),
            None
        );
        assert_eq!(
            version_from_location("https://github.com/x/y/releases/tag/latest"),
            None
        );
        assert_eq!(version_from_location(""), None);
    }

    #[test]
    fn freshness_table() {
        let latest = v("0.1.1");
        assert_eq!(freshness("0.1.0", &latest), Freshness::Outdated);
        assert_eq!(freshness("v0.1.1", &latest), Freshness::Current);
        assert_eq!(freshness("0.2.0", &latest), Freshness::Ahead);
        // Pre-releases sort below their final version.
        assert_eq!(freshness("0.1.1-rc.1", &latest), Freshness::Outdated);
        assert_eq!(freshness("", &latest), Freshness::Unknown);
        assert_eq!(freshness("dev", &latest), Freshness::Unknown);
    }
}
