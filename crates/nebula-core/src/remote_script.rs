//! The shell that runs on the far end of every ssh nebula opens: `nebula
//! ssh`, `nebula tunnel`, and the REMOTE WORKSPACES link all start from
//! the same self-installing prelude, so it lives here where both the
//! binary and the TUI can reach it.
//!
//! Quoting: sshd hands the command string to the user's login shell, which
//! may be bash, zsh, or fish. Every script built on these pieces is a fixed
//! constant with no single quotes, backslashes, or newlines, wrapped once
//! in '...'; user input (install URL, start dir) is passed only as
//! positional parameters, each POSIX-single-quoted. csh/tcsh login shells
//! are the one unsupported case.

/// The opening half of every remote script: leave a usable `nebula` on the
/// remote PATH, installing it first when there is none. `$1` is the install
/// URL. A macro rather than a const because `concat!` only takes literals,
/// and each caller builds a different tail onto the same head.
#[macro_export]
macro_rules! install_prelude {
    () => {
        concat!(
            // sshd hands a remote command a bare PATH — no login shell runs,
            // so nothing the user configured applies. Prepend install.sh's
            // default NEBULA_INSTALL_DIR, and append both Homebrew prefixes:
            // on a macOS remote that is the only place ttyd (which
            // `nebula browser` needs) or a brew-installed nebula lives.
            "export PATH=\"$HOME/.local/bin:$PATH:/opt/homebrew/bin:/usr/local/bin\"; ",
            "if ! command -v nebula >/dev/null 2>&1; then ",
            "command -v curl >/dev/null 2>&1 || { ",
            "echo \"nebula: curl is required on the remote to install nebula\" >&2; exit 127; }; ",
            "echo \"nebula not found on remote; installing...\" >&2; ",
            "curl -fsSL \"$1\" | sh || exit 1; ",
            "fi; "
        )
    };
}

/// Hand the nebula the script starts the SETTINGS BUNDLE in positional
/// parameter `$n`, when the command carries one (see `nebula_tui::bundle`).
/// An environment variable rather than a `nebula config import` step: a
/// remote nebula too old to know the variable ignores it, where it would fail
/// on the unknown command — and the remote nebula works out its own data dir,
/// which a script writing the files would have to guess. A macro for the same
/// reason as [`install_prelude!`].
#[macro_export]
macro_rules! export_settings_bundle {
    ($n:literal) => {
        concat!(
            "[ -z \"$",
            $n,
            "\" ] || export NEBULA_IMPORT_BUNDLE=\"$",
            $n,
            "\"; "
        )
    };
}

/// Where `install.sh` is fetched from when a remote has no nebula:
/// `NEBULA_INSTALL_URL`, else the published script.
pub fn install_url() -> String {
    crate::env::non_empty(crate::env::INSTALL_URL)
        .unwrap_or_else(|| DEFAULT_INSTALL_URL.to_string())
}

pub const DEFAULT_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/AgentSystemLabs/nebula/main/install.sh";

/// POSIX-quote for a remote shell: `it's` -> `'it'\''s'`.
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_edge_cases() {
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("'''"), "''\\'''\\'''\\'''");
    }

    #[test]
    fn prelude_survives_single_quoting() {
        // The whole scheme rests on the pieces needing no escaping inside
        // '...' under any login shell.
        for piece in [install_prelude!(), export_settings_bundle!("3")] {
            assert!(!piece.contains('\''), "{piece}");
            assert!(!piece.contains('\\'), "{piece}");
            assert!(!piece.contains('\n'), "{piece}");
        }
    }

    /// The bundle macro spells the variable inside a string literal, so
    /// hold it to the constant the remote nebula reads.
    #[test]
    fn the_bundle_macro_exports_the_variable_the_remote_nebula_reads() {
        let export = format!("export {}=\"$3\"", crate::env::IMPORT_BUNDLE);
        assert!(export_settings_bundle!("3").contains(&export));
    }
}
