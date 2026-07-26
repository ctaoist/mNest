#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("enable at least one database backend feature: sqlite or postgres");

pub const VERSION: &str = match option_env!("MNEST_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod api;
pub mod artist_credit;
pub mod auth;
pub mod config;
pub mod db;
pub mod entities;
pub mod internet_radio;
pub mod jobs;
pub mod lastfm;
pub mod migrations;
pub mod models;
pub mod network;
pub mod providers;
pub mod remote_download;
pub mod scanner;
pub mod state;
pub mod tags;

pub use state::AppState;
