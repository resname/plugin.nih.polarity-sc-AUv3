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
    let numeric_core = tag.find(|c: char| !(c.is_ascii_digit() || c == '.'))
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
}