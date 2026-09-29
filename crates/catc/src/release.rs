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

#[cfg(test)]
mod tests {
    use super::*;

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
