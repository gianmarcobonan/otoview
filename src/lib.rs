pub mod app;
pub mod detect;
pub mod protocol;
pub mod record;
pub mod sim;
pub mod wifi;

use std::sync::OnceLock;

/// Process builder that never flashes a console window from the GUI app on Windows.
pub(crate) fn command(prog: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Italian if the system language is Italian, English otherwise.
/// `OTOVIEW_LANG=en|it` overrides the system language.
pub fn tr(it: &'static str, en: &'static str) -> &'static str {
    static ITALIAN: OnceLock<bool> = OnceLock::new();
    let italian = *ITALIAN.get_or_init(|| {
        let lang = std::env::var("OTOVIEW_LANG").ok().or_else(sys_locale::get_locale).unwrap_or_default();
        lang.to_lowercase().starts_with("it")
    });
    if italian { it } else { en }
}
