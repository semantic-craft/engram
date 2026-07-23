//! Path helpers shared by command renderers and hook capture.

use std::borrow::Cow;
use std::path::PathBuf;

/// Resolve the user home used for agent configuration paths.
///
/// Tests and scripted wrappers can set `ENGRAM_HOME` to exercise the
/// platform-specific config layout without touching the real user profile.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("ENGRAM_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

/// Strip only Windows verbatim path prefixes that are safe to render as plain
/// paths. Unknown verbatim forms are left unchanged.
pub(crate) fn strip_windows_verbatim_prefix(path: &str) -> Cow<'_, str> {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };

    if rest.len() >= 3
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
        && rest.as_bytes()[2] == b'\\'
    {
        return Cow::Borrowed(rest);
    }

    let bytes = rest.as_bytes();
    if bytes.len() >= 4
        && bytes[0].eq_ignore_ascii_case(&b'U')
        && bytes[1].eq_ignore_ascii_case(&b'N')
        && bytes[2].eq_ignore_ascii_case(&b'C')
        && bytes[3] == b'\\'
    {
        return Cow::Owned(format!(r"\\{}", &rest[4..]));
    }

    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_windows_verbatim_prefix_handles_drive_paths() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:\Users\me\AppData\Local\engram"),
            r"C:\Users\me\AppData\Local\engram"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\c:\Users\me\AppData\Local\engram"),
            r"c:\Users\me\AppData\Local\engram"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_unc_case_insensitively() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\UNC\server\share\m"),
            r"\\server\share\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\unc\Server\Share\m"),
            r"\\Server\Share\m"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_non_verbatim_paths_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"C:\Users\me\engram"),
            r"C:\Users\me\engram"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix("/home/alice/.local/share/engram"),
            "/home/alice/.local/share/engram"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_unknown_forms_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\.\PhysicalDrive0"),
            r"\\.\PhysicalDrive0"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:relative\m"),
            r"\\?\C:relative\m"
        );
    }
}
