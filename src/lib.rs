pub mod daemon;
pub mod paths;
pub mod store;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
