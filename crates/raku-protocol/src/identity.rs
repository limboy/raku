//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Raku Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Raku";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.raku.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.raku";

#[cfg(debug_assertions)]
pub const URL_SCHEME: &str = "raku-debug";
#[cfg(not(debug_assertions))]
pub const URL_SCHEME: &str = "raku";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Raku Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Raku";
