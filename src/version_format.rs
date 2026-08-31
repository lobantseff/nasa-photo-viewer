// Included by `build.rs` as well as compiled into the library, so the rules
// and the tests that check them stay in one place: a copy in the build script
// alone could not be tested, because `cargo test` does not run build scripts.
//
// Inner doc comments are avoided here because this file is `include!`d.

/// Turn `git describe --tags --long --always --dirty=.dirty` output into a
/// version string.
///
/// ```text
/// v0.5.1-0-gabc12345         -> v0.5.1
/// v0.5.1-3-gabc12345         -> v0.5.1.dev3+abc12345
/// v0.5.1-3-gabc12345.dirty   -> v0.5.1.dev3+abc12345.dirty
/// abc12345                   -> v0.0.0.dev0+abc12345
/// ```
///
/// Only a clean checkout sitting exactly on a tag reports a bare version, so a
/// development build can never be mistaken for a release.
pub fn format_version(raw: &str) -> String {
    let (body, dirty) = match raw.strip_suffix(".dirty") {
        Some(body) => (body, ".dirty"),
        None => (raw, ""),
    };

    // `--long` always appends `-<count>-g<hash>`, so anything without that
    // shape is the bare hash `--always` falls back to when no tag exists.
    let Some((tag, hash)) = body.rsplit_once("-g") else {
        return format!("v0.0.0.dev0+{body}{dirty}");
    };
    let Some((tag, count)) = tag.rsplit_once('-') else {
        return format!("v0.0.0.dev0+{body}{dirty}");
    };
    let Ok(commits) = count.parse::<u32>() else {
        return format!("v0.0.0.dev0+{body}{dirty}");
    };

    if commits == 0 && dirty.is_empty() {
        return tag.to_string();
    }
    format!("{tag}.dev{commits}+{hash}{dirty}")
}

/// Whether a version string has the shape [`format_version`] produces.
///
/// A build whose git lookup failed reports a placeholder, which is a plausible
/// enough string to pass a cursory check but names no commit. This is strict
/// enough to reject it.
pub fn is_well_formed(version: &str) -> bool {
    let version = version.strip_suffix(".dirty").unwrap_or(version);

    let Some(rest) = version.strip_prefix('v') else {
        return false;
    };
    let (release, development) = match rest.split_once(".dev") {
        Some((release, development)) => (release, Some(development)),
        None => (rest, None),
    };

    // A release is three dot-separated numbers, optionally with a pre-release
    // suffix such as `1.2.3-rc1`.
    let core = release.split_once('-').map_or(release, |(core, _)| core);
    let mut parts = core.split('.');
    for _ in 0..3 {
        match parts.next() {
            Some(part) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {}
            _ => return false,
        }
    }
    if parts.next().is_some() {
        return false;
    }

    match development {
        // A development build must name both a distance and a commit.
        Some(development) => match development.split_once('+') {
            Some((count, hash)) => {
                !count.is_empty()
                    && count.bytes().all(|b| b.is_ascii_digit())
                    && !hash.is_empty()
                    && hash.bytes().all(|b| b.is_ascii_hexdigit())
            }
            None => false,
        },
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{format_version, is_well_formed};

    #[test]
    fn a_clean_tagged_build_is_just_the_tag() {
        assert_eq!(format_version("v0.5.1-0-gabc12345"), "v0.5.1");
    }

    #[test]
    fn commits_after_a_tag_become_a_development_version() {
        assert_eq!(format_version("v0.5.1-3-gabc12345"), "v0.5.1.dev3+abc12345");
    }

    #[test]
    fn a_dirty_tree_is_marked_even_when_sitting_on_a_tag() {
        // Otherwise uncommitted work would report itself as the release.
        assert_eq!(
            format_version("v0.5.1-0-gabc12345.dirty"),
            "v0.5.1.dev0+abc12345.dirty"
        );
        assert_eq!(
            format_version("v0.5.1-2-gabc12345.dirty"),
            "v0.5.1.dev2+abc12345.dirty"
        );
    }

    #[test]
    fn a_repository_without_tags_falls_back_to_zero() {
        assert_eq!(format_version("abc12345"), "v0.0.0.dev0+abc12345");
        assert_eq!(
            format_version("abc12345.dirty"),
            "v0.0.0.dev0+abc12345.dirty"
        );
    }

    #[test]
    fn a_tag_containing_dashes_keeps_its_name() {
        assert_eq!(
            format_version("v1.2.3-rc1-4-gabc12345"),
            "v1.2.3-rc1.dev4+abc12345"
        );
    }

    #[test]
    fn unrecognised_output_is_still_reported_as_a_development_build() {
        // Never silently claim to be a release.
        assert!(format_version("something-odd").starts_with("v0.0.0.dev0+"));
    }

    #[test]
    fn well_formed_accepts_what_the_formatter_produces() {
        for raw in [
            "v0.5.1-0-gabc12345",
            "v0.5.1-3-gabc12345",
            "v0.5.1-3-gabc12345.dirty",
            "abc12345",
            "v1.2.3-rc1-4-gabc12345",
        ] {
            let version = format_version(raw);
            assert!(
                is_well_formed(&version),
                "rejected {version:?} from {raw:?}"
            );
        }
    }

    #[test]
    fn well_formed_rejects_the_no_git_placeholder() {
        // The shape a build falls back to when `git describe` fails. It looks
        // like a version but names no commit, so it must not pass unnoticed.
        assert!(!is_well_formed("v0.0.0+unknown"));
    }

    #[test]
    fn well_formed_rejects_other_malformed_versions() {
        for bad in [
            "0.5.1",           // no leading v
            "v0.5",            // too few components
            "v0.5.1.2",        // too many
            "vx.y.z",          // not numbers
            "v0.5.1.dev3",     // distance without a commit
            "v0.5.1.dev+abc1", // commit without a distance
            "",
        ] {
            assert!(!is_well_formed(bad), "accepted {bad:?}");
        }
    }
}
