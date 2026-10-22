pub mod agents {
    pub mod caller;
    pub mod claude;
    pub mod codex;
    pub mod cursor_grok;
    pub mod source;

    /// Why a push did not reach the session; the message stays in the mailbox.
    #[derive(Debug)]
    pub struct PushError(pub &'static str);
}
pub mod daemon;
pub mod doing;
pub mod frame;
pub mod hook;
pub mod mail;
pub mod mcp;
pub mod os;
pub mod paths;
pub mod sessions;
pub mod store;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
