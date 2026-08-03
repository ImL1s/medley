//! User-facing rendering of the state directory.
//!
//! Kept next to the resolution it describes rather than in a UI crate: every
//! layer that prints a path to a user needs it, including `xai-grok-shell`,
//! which cannot reach the pager. Splitting the two meant the lower layers had
//! no way to say "the directory this install actually uses" and wrote
//! `~/.grok` literally instead — which stopped being true once state moved to
//! `~/.medley`, so the program named a directory it was not writing to.

use crate::paths::grok_home;
use std::path::Path;

/// User-facing label for the user state directory: `~/.medley`, `~/.grok`
/// while a legacy install is still live, or the override environment variable.
///
/// Derived from the resolved [`grok_home()`] against
/// [`crate::default_grok_home()`], not from whether an override happens to be
/// exported.
pub fn display_grok_home_prefix() -> String {
    display_grok_home_prefix_for(&grok_home())
}

pub(crate) fn display_grok_home_prefix_for(home: &Path) -> String {
    let default = crate::default_grok_home();
    if home == default {
        // Name the directory that actually holds state — which of ~/.medley
        // and ~/.grok that is depends on whether the migration has run.
        return match default.file_name() {
            Some(name) => format!("~/{}", name.to_string_lossy()),
            None => "~".to_string(),
        };
    }
    // Named from the source that actually won, not from which variable is set:
    // an exported-but-empty `MEDLEY_HOME` is ignored by resolution, so
    // labelling paths `$MEDLEY_HOME/...` would send the user to an empty
    // location. When `home` is an explicit path rather than the resolved one,
    // no override is in play and the compatibility variable is the right thing
    // to name.
    let env = crate::state_dir::resolve()
        .source
        .env_var()
        .unwrap_or(crate::state_dir::LEGACY_STATE_HOME_ENV);
    format!("${env}")
}

/// User-facing path under [`grok_home()`], e.g. `~/.medley/config.toml`.
///
/// Use this instead of writing a literal. A literal is right until someone
/// sets `MEDLEY_HOME`, or keeps an existing `~/.grok`, and then it is a
/// confident lie in a message whose whole job is to tell the user where to
/// look.
pub fn display_user_grok_path(relative: impl AsRef<Path>) -> String {
    display_user_grok_path_for(&grok_home(), relative)
}

pub(crate) fn display_user_grok_path_for(home: &Path, relative: impl AsRef<Path>) -> String {
    let rel = relative.as_ref();
    let prefix = display_grok_home_prefix_for(home);
    if rel.as_os_str().is_empty() {
        return prefix;
    }
    format!("{prefix}/{}", rel.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label of whichever env override is live, matching
    /// `display_grok_home_prefix_for`'s non-default branch.
    fn expected_env_label() -> String {
        let env = crate::state_dir::resolve()
            .source
            .env_var()
            .unwrap_or(crate::state_dir::LEGACY_STATE_HOME_ENV);
        format!("${env}")
    }

    #[test]
    fn display_grok_home_prefix_default_install() {
        if std::env::var_os(crate::state_dir::STATE_HOME_ENV).is_some()
            || std::env::var_os(crate::state_dir::LEGACY_STATE_HOME_ENV).is_some()
        {
            return;
        }
        let default = crate::default_grok_home();
        let expected = format!("~/{}", default.file_name().unwrap().to_string_lossy());
        assert_eq!(display_grok_home_prefix(), expected);
    }

    #[test]
    fn display_user_grok_path_joins_relative() {
        let path = display_user_grok_path("config.toml");
        assert!(path.ends_with("/config.toml") || path.ends_with("\\config.toml"));
        assert!(
            path.contains(crate::state_dir::STATE_DIR_NAME)
                || path.contains(crate::state_dir::LEGACY_STATE_DIR_NAME)
                || path.contains(&expected_env_label()),
            "got {path}"
        );
    }

    #[test]
    fn display_user_grok_path_for_custom_home_uses_override_label() {
        let custom = std::env::temp_dir().join("grok-home-display-regression");
        let label = expected_env_label();
        assert_eq!(
            display_user_grok_path_for(&custom, "config.toml"),
            format!("{label}/config.toml")
        );
        assert_eq!(
            display_user_grok_path_for(&custom, "sandbox.toml"),
            format!("{label}/sandbox.toml")
        );
    }

    /// The property every caller depends on and a literal cannot have: two
    /// different state directories must render differently. Swapping the
    /// helper back for a constant satisfies every assertion above and fails
    /// this one.
    #[test]
    fn two_homes_never_render_the_same() {
        let a = display_user_grok_path_for(Path::new("/srv/one"), "config.toml");
        let b = display_user_grok_path_for(Path::new("/srv/two"), "config.toml");
        let default = display_user_grok_path_for(&crate::default_grok_home(), "config.toml");

        // /srv/one and /srv/two are both non-default, so both render with the
        // override label — equal to each other, but never equal to the default
        // install's rendering, which is the distinction messages rely on.
        assert_ne!(a, default);
        assert_ne!(b, default);
        assert!(!default.starts_with('$'), "got {default}");
    }
}
