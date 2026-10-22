pub mod agents {
    pub mod caller;
    pub mod cursor_grok;
}
pub mod daemon;
pub mod doing;
pub mod frame;
pub mod hook;
pub mod mail;
pub mod os;
pub mod paths;
pub mod sessions;
pub mod store;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
