use anyhow::{anyhow, Context as _, Result};
use assistant_slash_command::{
    ArgumentCompletion, SlashCommand, SlashCommandContent, SlashCommandEvent, SlashCommandOutput,
    SlashCommandOutputSection, SlashCommandResult,
};
use editor::{Editor, SemanticsProvider};
use futures::channel::mpsc::{self, UnboundedSender};
use futures::StreamExt as _;
use gpui::{AppContext, Task, WeakView};
use language::{BufferSnapshot, ContextItemType, LspAdapterDelegate};
use rope::Point;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{path::Path, sync::atomic::AtomicBool};
use text::ToPointUtf16 as _;
use ui::{Context, IconName, SharedString, ViewContext, WindowContext};
use workspace::Workspace;

pub(crate) struct LspDiscoveryCommand;

impl SlashCommand for LspDiscoveryCommand {
    fn name(&self) -> String {
        "lsp".into()
    }

    fn description(&self) -> String {
        "Insert context discovered by LSP".into()
    }

    fn icon(&self) -> IconName {
        IconName::FileSearch
    }

    fn menu_text(&self) -> String {
        self.description()
    }

    fn complete_argument(
        self: Arc<Self>,
        _arguments: &[String],
        _cancel: Arc<AtomicBool>,
        _workspace: Option<WeakView<Workspace>>,
        _cx: &mut WindowContext,
    ) -> Task<Result<Vec<ArgumentCompletion>>> {
        Task::ready(Err(anyhow!("this command does not require argument")))
    }

    fn requires_argument(&self) -> bool {
        false
    }

    fn run(
        self: Arc<Self>,
        _arguments: &[String],
        _context_slash_command_output_sections: &[SlashCommandOutputSection<language::Anchor>],
        _context_buffer: BufferSnapshot,
        workspace: WeakView<Workspace>,
        delegate: Option<Arc<dyn LspAdapterDelegate>>,
        cx: &mut WindowContext,
    ) -> Task<SlashCommandResult> {
        let (events_tx, events_rx) = mpsc::unbounded();
        match workspace.update(cx, |w, cx| selections_creases(w, cx, events_tx)) {
            Ok(v) => v,
            Err(v) => return Task::ready(Err(v)),
        }

        Task::ready(Ok(events_rx.boxed()))
    }
}

pub fn selections_creases(
    workspace: &mut workspace::Workspace,
    cx: &mut ViewContext<Workspace>,
    out: UnboundedSender<Result<SlashCommandEvent>>,
) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    let mut definitions = vec![];
    editor.update(cx, |editor, cx| {
        let selections = editor.selections.all_adjusted(cx);
        let buffer = editor.buffer().read(cx).snapshot(cx);
        for selection in selections {
            let items = buffer.contexts_contained_by(selection.range());
            let Some((snapshot, symbol_list)) = items else {
                continue;
            };

            let Some(buffer) = editor.buffer().read(cx).buffer(snapshot.remote_id()) else {
                continue;
            };

            for symbol in symbol_list {
                for (range, ty) in symbol.items {
                    if ty != ContextItemType::GotoDefinition {
                        continue;
                    }

                    let mut position = range.start.to_point_utf16(snapshot);
                    workspace.project().update(cx, |project, cx| {
                        let def = project.definition(&buffer, position, cx);
                        definitions.push(def);
                    });
                }
            }
        }
    });

    let cx: &mut AppContext = cx;
    cx.spawn(|cx: gpui::AsyncAppContext| async move {
        for def in definitions.into_iter() {
            match def.await {
                Ok(loc) => {
                    for loc in loc {
                        let Ok(Some(path)) = cx.read_model(&loc.target.buffer, |v, _| {
                            v.file().map(|p| p.path().clone())
                        }) else {
                            continue;
                        };
                        out.unbounded_send(Ok(SlashCommandEvent::Content(
                            SlashCommandContent::Text {
                                text: format!(
                                    "{:?}:\n   {:?}\n   {:?}\n\n",
                                    path, loc.target, loc.origin
                                ),
                                run_commands_in_text: false,
                            },
                        )))?;
                    }
                }
                Err(e) => {
                    out.unbounded_send(Ok(SlashCommandEvent::Content(
                        SlashCommandContent::Text {
                            text: format!("error: {}", e),
                            run_commands_in_text: false,
                        },
                    )))?;
                }
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
