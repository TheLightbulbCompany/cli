pub mod caller;
pub mod claude;
pub mod codex;
pub mod cursor_grok;
pub mod source;

/// Why a push did not reach the session; the message stays in the mailbox.
#[derive(Debug)]
pub struct PushError(pub &'static str);
