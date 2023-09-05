use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::stream::FuturesUnordered;
use helix_core::chars::char_is_word;
use helix_core::syntax::LanguageServerFeature;
use helix_event::{
    canceable_future, cancelation, register_hook, send_blocking, CancelRx, CancelTx,
};
use helix_lsp::lsp;
use helix_lsp::util::pos_to_lsp_pos;
use helix_view::document::{Mode, SavePoint};
use helix_view::handlers::lsp::{CompletionEvent, CompletionTrigger};
use helix_view::Editor;
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
use tokio_stream::StreamExt;

use crate::commands;
use crate::compositor::Compositor;
use crate::config::Config;
use crate::events::{OnModeSwitch, PostCommand, PostInsertChar};
use crate::handlers::rope_ends_with;
use crate::job::{dispatch, dispatch_blocking};
use crate::keymap::MappableCommand;
use crate::ui::editor::InsertEvent;
use crate::ui::lsp::SignatureHelp;
use crate::ui::{self, CompletionItem, Popup};

use super::Handlers;

#[derive(Debug)]
pub(super) struct CompletionHandler {
    /// currently active trigger which will cause a
    /// completion request after the timeout
    trigger: Option<CompletionTrigger>,
    /// A handle for currently active completion request.
    /// This can be used to determine whether the current
    /// request is still active (and new triggers should be
    /// ignored) and can also be used to abort the current
    /// request (by dropping the handle)
    request: Option<CancelTx>,
    config: Arc<ArcSwap<Config>>,
}

impl CompletionHandler {
    pub fn new(config: Arc<ArcSwap<Config>>) -> CompletionHandler {
        Self {
            config,
            request: None,
            trigger: None,
        }
    }
}

impl helix_event::AsyncHook for CompletionHandler {
    type Event = CompletionEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _old_timeout: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            CompletionEvent::Trigger(trigger) => {
                // manual trigger chars should restart the completion request
                // as they essentially act as word boundaries (so we don't care
                // for the old request anymore)
                if !trigger.auto {
                    self.request = None;
                    self.trigger = Some(trigger);
                } else if self.trigger.is_none() {
                    self.trigger = Some(trigger);
                }
            }
            CompletionEvent::Cancel => {
                self.trigger = None;
                self.request = None;
            }
            CompletionEvent::DeleteText { pos } => {
                // if we deleted the original trigger, abort the completion
                if matches!(self.trigger, Some(CompletionTrigger{ trigger_pos,..}) if trigger_pos < pos)
                {
                    self.trigger = None;
                    self.request = None;
                }
            }
            CompletionEvent::Manual => {
                // immidietly request completions and drop all auto completion requests
                self.request = None;
                self.trigger = None;
                self.finish_debounce();
            }
        }
        if let Some(request) = &mut self.request {
            // if the current request was closed forget about it
            // otherwie don't start the completion timeout
            if request.is_closed() {
                self.request = None
            } else {
                return None;
            }
        }
        self.trigger.map(|trigger| {
            let timeout = if trigger.auto {
                self.config.load().editor.completion_timeout
            } else {
                // we want almost instant completions for trigger chars
                Duration::from_millis(5)
            };
            Instant::now() + timeout
        })
    }

    fn finish_debounce(&mut self) {
        let trigger = self.trigger.take();
        let (tx, rx) = cancelation();
        self.request = Some(tx);
        dispatch_blocking(move |editor, comositor| {
            request_completion(trigger, rx, editor, comositor)
        });
    }
}

fn request_completion(
    trigger: Option<CompletionTrigger>,
    cancel: CancelRx,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    let (view, doc) = current!(editor);

    if compositor
        .find::<ui::EditorView>()
        .unwrap()
        .completion
        .is_some()
        || editor.mode != Mode::Insert
    {
        return;
    }

    let text = doc.text();
    let cursor = doc.selection(view.id).primary().cursor(text.slice(..));
    if let Some(trigger) = trigger {
        if trigger.view != view.id || trigger.doc != doc.id() || cursor < trigger.trigger_pos {
            return;
        }
    }
    // this looks odd... Why are we not using the trigger position from
    // the `trigger` here? Won't that mean that the trigger char doesn't get
    // send to the LS if we type fast enougn? Yes that is true but it's
    // not actually a problem. The LSP will resolve the completion to the indetifier
    // anyway (in fact sending the later position is necessary to get the right results
    // from LSPs that provide incomplete completion list). We rely on trigger offset
    // and primary cursor matching for multi-cursor completions so this is definitly
    // necessary from our side too.
    let trigger_text = text.slice(..cursor);

    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesUnordered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::Completion)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .map(|ls| {
            let language_server_id = ls.id();
            let offset_encoding = ls.offset_encoding();
            let pos = pos_to_lsp_pos(text, cursor, offset_encoding);
            let doc_id = doc.identifier();
            let context = if trigger.is_some() {
                let trigger_char =
                    ls.capabilities()
                        .completion_provider
                        .as_ref()
                        .and_then(|provider| {
                            provider
                                .trigger_characters
                                .as_deref()?
                                .iter()
                                .find(|&trigger| rope_ends_with(trigger, trigger_text))
                        });
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::TRIGGER_CHARACTER,
                    trigger_character: trigger_char.cloned(),
                }
            } else {
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                }
            };

            let completion_request = ls.completion(doc_id, pos, None, context).unwrap();
            async move {
                let json = completion_request.await?;
                let response: Option<lsp::CompletionResponse> = serde_json::from_value(json)?;
                let items = match response {
                    Some(lsp::CompletionResponse::Array(items)) => items,
                    // TODO: do something with is_incomplete
                    Some(lsp::CompletionResponse::List(lsp::CompletionList {
                        is_incomplete: _is_incomplete,
                        items,
                    })) => items,
                    None => Vec::new(),
                }
                .into_iter()
                .map(|item| CompletionItem {
                    item,
                    language_server_id,
                    resolved: false,
                })
                .collect();
                anyhow::Ok(items)
            }
        })
        .collect();

    let future = async move {
        let mut items = Vec::new();
        while let Some(lsp_items) = futures.next().await {
            match lsp_items {
                Ok(mut lsp_items) => items.append(&mut lsp_items),
                Err(err) => {
                    log::debug!("completion request failed: {err:?}");
                }
            };
        }
        items
    };

    let savepoint = doc.savepoint(view);
    let trigger = CompletionTrigger {
        trigger_pos: cursor,
        doc: doc.id(),
        view: view.id,
        auto: false,
    };

    let ui = compositor.find::<ui::EditorView>().unwrap();
    ui.last_insert.1.push(InsertEvent::RequestCompletion);
    tokio::spawn(async move {
        let items = canceable_future(future, cancel).await.unwrap_or_default();
        if items.is_empty() {
            return;
        }
        dispatch(move |editor, compositor| {
            show_completion(editor, compositor, items, trigger, savepoint)
        })
        .await
    });
}

fn show_completion(
    editor: &mut Editor,
    compositor: &mut Compositor,
    items: Vec<CompletionItem>,
    trigger: CompletionTrigger,
    savepoint: Arc<SavePoint>,
) {
    let (view, doc) = current_ref!(editor);
    // check if the completion request is stale.
    //
    // Completions are completed asynchronously and therefore the user could
    //switch document/view or leave insert mode. In all of thoise cases the
    // completion should be discarded
    if editor.mode != Mode::Insert || view.id != trigger.view || doc.id() != trigger.doc {
        return;
    }

    let size = compositor.size();
    let ui = compositor.find::<ui::EditorView>().unwrap();
    if ui.completion.is_some() {
        return;
    }

    let completion_area = ui.set_completion(editor, savepoint, items, trigger.trigger_pos, size);
    let size = compositor.size();
    let signature_help_area = compositor
        .find_id::<Popup<SignatureHelp>>(SignatureHelp::ID)
        .map(|signature_help| signature_help.area(size, editor));
    // Delete the signature help popup if they intersect.
    if matches!((completion_area, signature_help_area),(Some(a), Some(b)) if a.intersects(b)) {
        compositor.remove(SignatureHelp::ID);
    }
}

pub fn trigger_auto_completion(
    tx: &Sender<CompletionEvent>,
    editor: &Editor,
    trigger_char_only: bool,
) {
    let config = editor.config.load();
    if config.auto_completion {
        let (view, doc): (&helix_view::View, &helix_view::Document) = current_ref!(editor);
        let mut text = doc.text().slice(..);
        let primary_cursor = doc.selection(view.id).primary().cursor(text);
        text = doc.text().slice(..primary_cursor);

        let is_trigger_char = doc
            .language_servers_with_feature(LanguageServerFeature::Completion)
            .any(|ls| {
                matches!(&ls.capabilities().completion_provider, Some(lsp::CompletionOptions {
                        trigger_characters: Some(triggers),
                        ..
                    }) if triggers.iter().any(|trigger| rope_ends_with(trigger, text)))
            });

        let is_auto_trigger = !trigger_char_only
            && doc
                .text()
                .chars_at(primary_cursor)
                .reversed()
                .take(config.completion_trigger_len as usize)
                .all(char_is_word);

        if is_trigger_char || is_auto_trigger {
            send_blocking(
                tx,
                CompletionEvent::Trigger(CompletionTrigger {
                    trigger_pos: primary_cursor,
                    doc: doc.id(),
                    view: view.id,
                    auto: !is_trigger_char,
                }),
            );
        }
    }
}

fn update_completions(cx: &mut commands::Context, c: Option<char>) {
    cx.callback.push(Box::new(move |compositor, cx| {
        let editor_view = compositor.find::<ui::EditorView>().unwrap();
        if let Some(completion) = &mut editor_view.completion {
            completion.update_filter(c);
            if completion.is_empty() {
                editor_view.clear_completion(cx.editor);
                // clearing completions might mean we want to immidietly rerequest them (usually
                // this occurs if typing a trigger char)
                if c.is_some() {
                    trigger_auto_completion(&cx.editor.handlers.completions, cx.editor, false);
                }
            }
        }
    }))
}

fn clear_completions(cx: &mut commands::Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let editor_view = compositor.find::<ui::EditorView>().unwrap();
        editor_view.clear_completion(cx.editor);
    }))
}

fn completion_post_command_hook(
    tx: &Sender<CompletionEvent>,
    PostCommand { command, cx }: &mut PostCommand<'_, '_>,
) -> anyhow::Result<()> {
    if cx.editor.mode == Mode::Insert {
        if cx.editor.last_completion.is_some() {
            match command {
                MappableCommand::Static {
                    name: "delete_word_forward" | "delete_char_forward" | "completion",
                    ..
                } => (),
                MappableCommand::Static {
                    name: "delete_char_backward",
                    ..
                } => update_completions(cx, None),
                _ => clear_completions(cx),
            }
        } else {
            let event = match command {
                MappableCommand::Static {
                    name:
                        "delete_word_forward"
                        | "delete_char_backward"
                        | "delete_char_forward"
                        | "completion",
                    ..
                } => {
                    let (view, doc) = current!(cx.editor);
                    let primary_cursor = doc
                        .selection(view.id)
                        .primary()
                        .cursor(doc.text().slice(..));
                    CompletionEvent::DeleteText {
                        pos: primary_cursor,
                    }
                }
                _ => CompletionEvent::Cancel,
            };
            send_blocking(tx, event);
        }
    }
    Ok(())
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.completions.clone();
    register_hook!(move |event: &mut PostCommand<'_, '_>| completion_post_command_hook(&tx, event));

    let tx = handlers.completions.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, CompletionEvent::Cancel);
            clear_completions(event.cx);
        } else if event.new_mode == Mode::Insert {
            trigger_auto_completion(&tx, event.cx.editor, false)
        }
        Ok(())
    });

    let tx = handlers.completions.clone();
    register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
        if event.cx.editor.last_completion.is_some() {
            update_completions(event.cx, Some(event.c))
        } else {
            trigger_auto_completion(&tx, event.cx.editor, false);
        }
        Ok(())
    });
}
