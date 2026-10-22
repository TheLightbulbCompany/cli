pub mod daemon;
pub mod doing;
pub mod mail;
pub mod os;
pub mod paths;
pub mod sessions;
pub mod store;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
