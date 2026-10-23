//! Update checker: compares the running plugin version against the latest
//! GitHub release, fetched once per GUI open from the plugin's origin repo.

/// Parses a release tag into its leading numeric version, ignoring an optional
/// leading `v`/`V` and all suffix text (`v10-rc1 -> (10, 0, 0)`). `None` when
/// the tag has no numeric core at all.
pub fn parse_version_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    if tag.is_empty() {
        return None;
    }

    // Cut at the first non-`[0-9.]` character so suffixes like `-rc1` fall off,
    // then require every `.`-separated piece to be numeric.
    let numeric_core = tag
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|end| &tag[..end])
        .unwrap_or(tag);
    let mut components = numeric_core.split('.').map(|component| {
        if component.is_empty() {
            None
        } else {
            component.parse::<u64>().ok()
        }
    });

    let major = components.next()??;
    let minor = components.next().unwrap_or(Some(0))?;
    let patch = components.next().unwrap_or(Some(0))?;
    if components.next().is_some() {
        return None;
    }

    Some((major, minor, patch))
}

/// `true` only when the latest release's tag parses to a strictly greater
/// version than the running plugin's version.
pub fn version_is_newer(latest_tag: &str, running: &str) -> bool {
    let Some(latest) = parse_version_tag(latest_tag) else {
        return false;
    };
    let Some(running) = parse_version_tag(running) else {
        return false;
    };

    latest > running
}

/// The GitHub page the strip's button opens.
pub const RELEASES_LATEST_PAGE_URL: &str =
    "https://github.com/resname/resname-spectral-compressor-AUv3/releases/latest";

/// The latest release found in the response body of a `releases/latest`
/// lookup.
#[derive(Clone, Debug)]
pub struct LatestRelease {
    pub tag: String,
    pub page_url: String,
}

/// Extracts `tag_name`/`html_url` from a single `releases/latest` response
/// body and returns it only when its version is strictly newer than the
/// running version.
fn parse_release_json(json: &str, running_version: &str) -> Option<LatestRelease> {
    let body = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let tag = body.get("tag_name")?.as_str()?.to_owned();
    // Reject tags without a parseable numeric version, then only show
    // strictly newer releases.
    parse_version_tag(&tag)?;
    if !version_is_newer(&tag, running_version) {
        return None;
    }

    let page_url = body.get("html_url")?.as_str()?.to_owned();

    Some(LatestRelease { tag, page_url })
}

const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/resname/resname-spectral-compressor-AUv3/releases/latest";

/// Performs the update check: one GET against `releases/latest`, `None` on any
/// failure (offline, non-200, malformed, not newer, or 10s total timeout — the
/// default ureq config has no timeouts, and a stalled peer would leave the
/// fetch thread blocked for the GUI's lifetime). No retry, no pagination.
pub fn fetch_latest_release(running_version: &str) -> Option<LatestRelease> {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let mut response = agent
        .get(LATEST_RELEASE_API_URL)
        .header(
            "User-Agent",
            concat!("SpectralCompressor-", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .ok()?;
    let json = response.body_mut().read_to_string().ok()?;

    parse_release_json(&json, running_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_tag_basic() {
        assert_eq!(parse_version_tag("v0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version_tag("0.5.22"), Some((0, 5, 22)));
        assert_eq!(parse_version_tag("V1.2.3"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_version_tag_garbage() {
        assert_eq!(parse_version_tag("release-demo"), None);
        assert_eq!(parse_version_tag(""), None);
        assert_eq!(parse_version_tag("v"), None);
    }

    #[test]
    fn parse_version_tag_suffixes() {
        // Suffix text after the numeric core is ignored.
        assert_eq!(parse_version_tag("v10-rc1"), Some((10, 0, 0)));
        // Leading zeros parse numerically, never lexically.
        assert_eq!(parse_version_tag("v0.06.1"), Some((0, 6, 1)));
    }

    #[test]
    fn version_is_newer_strictly_greater() {
        assert!(version_is_newer("v0.7.0", "0.6.0"));
        assert!(!version_is_newer("v0.6.0", "0.6.0"));
        assert!(!version_is_newer("v0.5.9", "0.6.0"));
        assert!(!version_is_newer("notaversion", "0.6.0"));
        assert!(version_is_newer("v1", "0.99.99"));
    }

    #[test]
    fn parse_release_json_extracts_tag_and_url() {
        let json = r#"{"tag_name":"v0.7.0","html_url":"https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/tag/v0.7.0"}"#;
        let release = parse_release_json(json, "0.6.0").expect("should parse");
        assert_eq!(release.tag, "v0.7.0");
        assert_eq!(
            release.page_url,
            "https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/tag/v0.7.0"
        );
    }

    #[test]
    fn parse_release_json_older_is_none() {
        let json =
            r#"{"tag_name":"v0.6.0","html_url":"https://github.com/x/y/releases/tag/v0.6.0"}"#;
        assert!(parse_release_json(json, "0.6.0").is_none());
    }
}
