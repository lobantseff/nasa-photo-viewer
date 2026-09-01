//! Noticing that a newer release exists.
//!
//! The check is advisory: it runs in the background, fails silently, and never
//! delays startup. Being unable to reach GitHub is the normal case for someone
//! working offline, not an error worth reporting.

use serde::Deserialize;

/// Where the latest release is published.
pub const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/lobantseff/nasa-photo-viewer/releases/latest";

/// Where a user is sent to download it.
pub const RELEASES_PAGE: &str = "https://github.com/lobantseff/nasa-photo-viewer/releases/latest";

/// The parts of GitHub's release response this needs.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestRelease {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// A release newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// Tag of the newer release, as published.
    pub version: String,
    /// Page to send the user to.
    pub url: String,
}

/// Whether this build is a released version rather than one built from a
/// working tree.
///
/// A development build sits *after* some release, so offering it that release
/// as an upgrade would be wrong, and offering it a later one invites replacing
/// local work with a download. Neither is useful, so they do not check at all.
pub fn is_release_build(version: &str) -> bool {
    !version.contains(".dev") && !version.contains(".dirty")
}

/// Whether asking GitHub is worth doing at all.
///
/// Separated from the request so the rule can be checked directly, rather than
/// only through whichever kind of build the tests happen to run against.
pub fn should_check(enabled: bool, version: &str) -> bool {
    enabled && is_release_build(version)
}

/// Parse the release part of a version into a comparable triple.
///
/// Tolerates the leading `v` that git tags carry and any development suffix,
/// so the same function reads both a tag name and the running version.
pub fn release_triple(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    // Everything after the release part describes a build or a pre-release and
    // takes no part in the comparison.
    let core = version.split_once(".dev").map_or(version, |(core, _)| core);
    let core = core.split_once('-').map_or(core, |(core, _)| core);

    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    // A fourth component means this is not the shape being compared.
    parts.next().is_none().then_some((major, minor, patch))
}

/// Whether `latest` supersedes `current`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (release_triple(current), release_triple(latest)) {
        (Some(current), Some(latest)) => latest > current,
        // An unreadable version on either side is not grounds for telling
        // someone to upgrade.
        _ => false,
    }
}

/// Decide what to show, given the running version and what GitHub reported.
///
/// Returns `None` when there is nothing worth saying: not a release build, not
/// actually newer, or already dismissed.
pub fn evaluate(
    current: &str,
    latest: &LatestRelease,
    dismissed: Option<&str>,
) -> Option<Available> {
    if !is_release_build(current) {
        return None;
    }
    if !is_newer(current, &latest.tag_name) {
        return None;
    }
    // Dismissing one version should not silence a later one.
    if let Some(dismissed) = dismissed
        && !is_newer(dismissed, &latest.tag_name)
    {
        return None;
    }

    Some(Available {
        version: latest.tag_name.clone(),
        url: latest
            .html_url
            .clone()
            .unwrap_or_else(|| RELEASES_PAGE.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> LatestRelease {
        LatestRelease {
            tag_name: tag.to_string(),
            html_url: Some(format!("https://example.invalid/{tag}")),
        }
    }

    #[test]
    fn a_tagged_build_is_a_release_and_a_dev_build_is_not() {
        assert!(is_release_build("v0.5.2"));
        assert!(!is_release_build("v0.5.2.dev3+abc12345"));
        assert!(!is_release_build("v0.5.2.dev0+abc12345.dirty"));
    }

    #[test]
    fn a_check_is_made_only_for_an_enabled_release_build() {
        assert!(should_check(true, "v0.5.2"));

        assert!(!should_check(false, "v0.5.2"), "switched off");
        assert!(
            !should_check(true, "v0.5.2.dev3+abc12345"),
            "development build"
        );
        assert!(!should_check(false, "v0.5.2.dev3+abc12345"));
    }

    #[test]
    fn version_triples_ignore_the_prefix_and_any_suffix() {
        assert_eq!(release_triple("v0.5.2"), Some((0, 5, 2)));
        assert_eq!(release_triple("0.5.2"), Some((0, 5, 2)));
        assert_eq!(release_triple("v0.5.2.dev3+abc12345"), Some((0, 5, 2)));
        assert_eq!(release_triple("v1.2.3-rc1"), Some((1, 2, 3)));
    }

    #[test]
    fn unreadable_versions_have_no_triple() {
        for bad in ["", "v", "0.5", "0.5.2.1", "vx.y.z", "latest"] {
            assert_eq!(release_triple(bad), None, "parsed {bad:?}");
        }
    }

    #[test]
    fn only_a_higher_version_counts_as_newer() {
        assert!(is_newer("v0.5.2", "v0.5.3"));
        assert!(is_newer("v0.5.2", "v0.6.0"));
        assert!(is_newer("v0.5.2", "v1.0.0"));

        assert!(!is_newer("v0.5.2", "v0.5.2"));
        assert!(!is_newer("v0.5.2", "v0.5.1"));
        assert!(!is_newer("v1.0.0", "v0.9.9"));
    }

    #[test]
    fn an_unreadable_version_never_prompts_an_upgrade() {
        assert!(!is_newer("v0.5.2", "nightly"));
        assert!(!is_newer("unknown", "v9.9.9"));
    }

    #[test]
    fn a_newer_release_is_offered() {
        let got = evaluate("v0.5.2", &release("v0.6.0"), None).unwrap();
        assert_eq!(got.version, "v0.6.0");
        assert_eq!(got.url, "https://example.invalid/v0.6.0");
    }

    #[test]
    fn the_current_release_is_not_offered() {
        assert_eq!(evaluate("v0.5.2", &release("v0.5.2"), None), None);
    }

    #[test]
    fn a_development_build_is_never_offered_an_upgrade() {
        // It sits after v0.5.2 and may contain unreleased work; pointing it at
        // a download would be telling the user to discard that.
        assert_eq!(
            evaluate("v0.5.2.dev3+abc12345", &release("v0.6.0"), None),
            None
        );
    }

    #[test]
    fn a_dismissed_version_stays_dismissed() {
        assert_eq!(evaluate("v0.5.2", &release("v0.6.0"), Some("v0.6.0")), None);
    }

    #[test]
    fn dismissing_one_version_does_not_silence_a_later_one() {
        let got = evaluate("v0.5.2", &release("v0.7.0"), Some("v0.6.0")).unwrap();
        assert_eq!(got.version, "v0.7.0");
    }

    #[test]
    fn a_release_without_a_page_falls_back_to_the_releases_url() {
        let bare = LatestRelease {
            tag_name: "v0.6.0".to_string(),
            html_url: None,
        };
        assert_eq!(evaluate("v0.5.2", &bare, None).unwrap().url, RELEASES_PAGE);
    }

    #[test]
    fn the_github_response_shape_is_understood() {
        // Trimmed from a real response; unknown fields must not break parsing.
        let raw = r#"{
            "tag_name": "v0.5.2",
            "html_url": "https://github.com/lobantseff/nasa-photo-viewer/releases/tag/v0.5.2",
            "name": "v0.5.2",
            "draft": false,
            "prerelease": false,
            "assets": []
        }"#;
        let parsed: LatestRelease = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.tag_name, "v0.5.2");
        assert!(parsed.html_url.unwrap().ends_with("v0.5.2"));
    }
}
