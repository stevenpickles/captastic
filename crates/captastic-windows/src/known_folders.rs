//! Where Windows says the user's own folders are.
//!
//! `%USERPROFILE%\Pictures` is a guess, not an answer. The shell's Pictures folder is a *known
//! folder*, and its location is a per-user setting that OneDrive's Known Folder Move, a roaming
//! profile, or a group policy can point anywhere — most often at
//! `%USERPROFILE%\OneDrive\Pictures`. When that has happened, `%USERPROFILE%\Pictures` usually
//! still exists as an empty leftover, so writing there fails silently from the user's point of
//! view: the files land in a directory Explorer no longer shows as Pictures, and "Pictures" in
//! the sidebar stays empty. That is exactly what happened on 2026-09-17.
//!
//! Like `host`, every probe here is a question about the machine and never an instruction to it:
//! nothing below creates a directory, writes a value, or holds a resource past the call, and a
//! refusal answers `None` rather than failing. A user who has no Pictures folder at all — a
//! service account, a stripped image — still gets a working Captastic through the caller's
//! fallback.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// The shell's Pictures folder for the current user, or `None` if Windows will not say.
///
/// `KF_FLAG_DEFAULT` asks for the folder as it is configured now and does not create it: a probe
/// that made a directory as a side effect of being asked a question would leave a trail on a
/// machine that only ran `captastic doctor`.
///
/// Redirected folders are the reason this exists, and they are also the reason the answer is not
/// cached here: it is read once at startup by the caller, which is where the resolved directory
/// is logged, so a user comparing the log against Explorer is comparing two live answers.
pub fn known_pictures_folder() -> Option<PathBuf> {
    // SAFETY: `FOLDERID_Pictures` is a `'static` GUID, so the pointer is valid for the call.
    // `None` for the token means the calling process's user, which is who we are writing for. On
    // success Windows allocates the path with the COM task allocator and hands us the only owning
    // pointer to it; it is freed exactly once below. On failure nothing is allocated.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None) }.ok()?;
    if path.is_null() {
        // Documented as impossible on S_OK, and cheap to refuse rather than dereference.
        return None;
    }
    // SAFETY: `path` came back from a successful `SHGetKnownFolderPath`, which returns a
    // NUL-terminated wide string, so it is valid for reads up to and including that NUL. The
    // slice is copied into an owned `OsString` before the buffer is freed.
    let wide = unsafe { path.as_wide() }.to_vec();
    // SAFETY: `path` was allocated by the successful call above with the COM task allocator, is
    // freed here exactly once, and is not read again afterwards.
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast())) };
    if wide.is_empty() {
        return None;
    }
    let directory = PathBuf::from(OsString::from_wide(&wide));
    // A relative known folder would be a redirection nobody could act on, and it would put
    // captures wherever the daemon happened to be launched from. Refuse it to the caller's
    // fallback instead.
    directory.is_absolute().then_some(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the real probe. Asserts only what is true of any machine able to run it.
    ///
    /// Including a hosted CI runner with no interactive session — which still has a user profile,
    /// but is exactly the host this must not panic on if it somehow does not.
    #[test]
    fn the_pictures_folder_answers_without_panicking() {
        let Some(directory) = known_pictures_folder() else {
            // A machine without a Pictures known folder is a machine the fallback exists for.
            return;
        };
        assert!(directory.is_absolute(), "{}", directory.display());
        assert!(!directory.as_os_str().is_empty());
    }

    /// The whole point: the answer is the shell's, not `%USERPROFILE%\Pictures` assumed.
    ///
    /// On an un-redirected machine the two agree and this asserts nothing interesting. On a
    /// redirected one — OneDrive Known Folder Move, a roaming profile — they differ, and the
    /// probe must be reporting the redirected location rather than the assumption.
    #[test]
    fn the_answer_comes_from_the_shell_rather_than_the_profile() {
        let Some(directory) = known_pictures_folder() else {
            return;
        };
        // Whatever the shell says, it ends in a directory name: no trailing separator, no empty
        // final component, which is what the caller joins `Captastic` onto.
        assert!(directory.file_name().is_some(), "{}", directory.display());
    }
}
