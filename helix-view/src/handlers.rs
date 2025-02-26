use helix_event::send_blocking;
use tokio::sync::mpsc::Sender;

use crate::handlers::lsp::SignatureHelpInvoked;
use crate::Editor;

pub mod dap;
pub mod lsp;

pub struct Handlers {
    // only public because most of the actual implementation is in helix-term right now :/
    pub completions: Sender<lsp::CompletionEvent>,
    pub signature_hints: Sender<lsp::SignatureHelpEvent>,
}

impl Handlers {
    /// Manually trigger completion (c-x)
    pub fn trigger_completions(&self) {
        send_blocking(&self.completions, lsp::CompletionEvent::Manual);
    }

    pub fn trigger_signature_help(&self, invocation: SignatureHelpInvoked, editor: &Editor) {
        let event = match invocation {
            SignatureHelpInvoked::Automatic => {
                if !editor.config().lsp.auto_signature_help {
                    return;
                }
                lsp::SignatureHelpEvent::Trigger
            }
            SignatureHelpInvoked::Manual => lsp::SignatureHelpEvent::Invoked,
        };
        send_blocking(&self.signature_hints, event)
    }
}
