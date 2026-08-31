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

#[cfg(test)]
mod tests {
    use super::format_version;

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
}
