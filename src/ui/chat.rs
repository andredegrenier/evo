//! The chat panel: ask the document a question, watch the answer arrive, and
//! click a citation to land on the page it came from.
//!
//! The panel owns no worker and no model config -- it renders what the engine's
//! status says and hands back what the user asked for. Everything it knows
//! about a conversation lives in [`ChatSessionState`], which sits in `DocState`
//! and so dies with the document it is about.

use eframe::egui;

use crate::chat::ChatEngine;
use crate::script::model::{ChatMessage, Role};
use crate::state::DocState;

/// One document's conversation.
#[derive(Default)]
pub struct ChatSessionState {
    pub open: bool,
    pub input: String,
    pub messages: Vec<ChatMessage>,
    /// The key the engine caches page text under; filled in on the first
    /// question, since it can mean hashing a few megabytes.
    pub doc_key: Option<String>,
    /// Why the last question failed, shown until the next one is asked.
    pub error: Option<String>,
    /// Pages the last answer was allowed to quote, 1-based.
    pub last_pages: Vec<usize>,
    /// Tools the last answer ran, in order.
    pub last_tools: Vec<String>,
    /// Whether this conversation may use the MCP servers in Preferences.
    ///
    /// Off by default, and per panel: letting a model reach other programs is
    /// a decision about this conversation, not a setting made once.
    pub allow_tools: bool,
    /// Ask the input box for the keyboard on the next frame.
    pub focus_pending: bool,
}

/// What the app shell should do after the panel is drawn.
pub enum ChatAction {
    Ask(String),
    Cancel,
    Clear,
    /// Scroll to a 1-based source page number, as written in a citation.
    GoToPage(usize),
}

/// A run of answer text, or a `[p.N]` citation lifted out of it.
#[derive(Debug, PartialEq)]
pub enum Segment<'a> {
    Text(&'a str),
    Cite(usize),
}

/// Split `[p.N]` citations out of an answer so they can be drawn as links.
///
/// Anything that isn't a well-formed citation stays plain text, including the
/// brackets: a model that writes "[p.four]" has written prose, not a link.
pub fn split_citations(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut plain_from = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // "[p." or "[P." then optional space, digits, "]".
        let mut j = i + 1;
        if bytes.get(j).is_none_or(|c| !c.eq_ignore_ascii_case(&b'p')) {
            i += 1;
            continue;
        }
        j += 1;
        if bytes.get(j) == Some(&b'.') {
            j += 1;
        }
        while bytes.get(j) == Some(&b' ') {
            j += 1;
        }
        let digits_from = j;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j == digits_from || bytes.get(j) != Some(&b']') {
            i += 1;
            continue;
        }
        let Ok(page) = text[digits_from..j].parse::<usize>() else {
            i += 1;
            continue;
        };
        if page == 0 {
            i += 1;
            continue;
        }
        if plain_from < i {
            out.push(Segment::Text(&text[plain_from..i]));
        }
        out.push(Segment::Cite(page));
        i = j + 1;
        plain_from = i;
    }
    if plain_from < text.len() {
        out.push(Segment::Text(&text[plain_from..]));
    }
    out
}

pub fn show(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    engine: Option<&ChatEngine>,
    tools_configured: bool,
) -> Option<ChatAction> {
    let mut action = None;
    let running = match (engine, dc.chat.doc_key.as_deref()) {
        (Some(engine), Some(key)) => engine.is_running(key),
        _ => false,
    };

    egui::Panel::top("chat-header").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Chat");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✕")
                    .on_hover_text("Close the chat")
                    .clicked()
                {
                    dc.chat.open = false;
                }
                if ui
                    .add_enabled(
                        !dc.chat.messages.is_empty() && !running,
                        egui::Button::new("Clear").small(),
                    )
                    .clicked()
                {
                    action = Some(ChatAction::Clear);
                }
            });
        });
    });

    egui::Panel::bottom("chat-input").show(ui, |ui| {
        ui.add_space(4.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut dc.chat.input)
                .desired_width(f32::INFINITY)
                .hint_text("Ask about this document…"),
        );
        if std::mem::take(&mut dc.chat.focus_pending) {
            field.request_focus();
        }
        // A single-line field reports Enter by losing focus; asking for it back
        // next frame is what keeps the conversation typeable without a click.
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

        ui.horizontal(|ui| {
            if running {
                ui.spinner();
                if ui.button("Stop").clicked() {
                    action = Some(ChatAction::Cancel);
                }
            } else {
                let ready = !dc.chat.input.trim().is_empty();
                let asked = ui.add_enabled(ready, egui::Button::new("Ask")).clicked()
                    || (submitted && ready);
                if asked {
                    let question = dc.chat.input.trim().to_owned();
                    dc.chat.input.clear();
                    dc.chat.focus_pending = true;
                    action = Some(ChatAction::Ask(question));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if tools_configured {
                    ui.checkbox(
                        &mut dc.chat.allow_tools,
                        egui::RichText::new("Allow tools").small(),
                    )
                    .on_hover_text(
                        "Let the model use the MCP servers in Preferences ▸ MCP \
                             while answering. Off by default; what it runs is \
                             listed as it happens.",
                    );
                } else {
                    ui.label(
                        egui::RichText::new("Answers come from this document")
                            .weak()
                            .small(),
                    );
                }
            });
        });
        ui.add_space(2.0);
    });

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if dc.chat.messages.is_empty() && !running {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Ask a question and the pages that look relevant are sent to \
                         your local model, which is asked to cite them. Click a \
                         citation to go to the page.",
                    )
                    .weak(),
                );
            }
            for message in &dc.chat.messages {
                if let Some(page) = bubble(ui, message) {
                    action = Some(ChatAction::GoToPage(page));
                }
            }
            if !dc.chat.last_pages.is_empty() && !running {
                let mut line = format!("Read {}", page_list(&dc.chat.last_pages));
                if !dc.chat.last_tools.is_empty() {
                    line.push_str(&format!(" · ran {}", dc.chat.last_tools.join(", ")));
                }
                ui.label(egui::RichText::new(line).weak().small());
            }
            if running && let Some(engine) = engine {
                engine.with_status(|status| {
                    for line in &status.activity {
                        ui.label(egui::RichText::new(line).weak().small().monospace());
                    }
                    if let Some(stage) = status.stage {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(stage).weak());
                    }
                    if !status.streaming.is_empty() {
                        let pending = ChatMessage::new(Role::Assistant, status.streaming.clone());
                        // Citations only become links once the answer is whole;
                        // half of "[p.1" is not a page number yet.
                        bubble(ui, &pending);
                    }
                });
            }
            if let Some(error) = &dc.chat.error {
                ui.add_space(6.0);
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });

    action
}

/// One message. Returns the page a citation link was clicked for.
fn bubble(ui: &mut egui::Ui, message: &ChatMessage) -> Option<usize> {
    let mut clicked = None;
    let user = message.role == Role::User;
    ui.add_space(6.0);
    let frame = egui::Frame::new()
        .fill(if user {
            ui.visuals().faint_bg_color
        } else {
            egui::Color32::TRANSPARENT
        })
        .corner_radius(6)
        .inner_margin(8.0);
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(if user { "You" } else { "Document" })
                    .small()
                    .weak(),
            );
            for line in message.content.split('\n') {
                if line.trim().is_empty() {
                    ui.add_space(4.0);
                    continue;
                }
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for segment in split_citations(line) {
                        match segment {
                            Segment::Text(text) => {
                                ui.label(text);
                            }
                            Segment::Cite(page) => {
                                if ui
                                    .link(format!("[p.{page}]"))
                                    .on_hover_text(format!("Go to page {page}"))
                                    .clicked()
                                {
                                    clicked = Some(page);
                                }
                            }
                        }
                    }
                });
            }
        });
    });
    clicked
}

/// "page 3" / "pages 3, 4 and 7".
fn page_list(pages: &[usize]) -> String {
    let names: Vec<String> = pages.iter().map(|p| p.to_string()).collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => format!("page {last}"),
        Some((last, rest)) => format!("pages {} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_are_lifted_out_of_the_surrounding_text() {
        let segments = split_citations("The alarm is in the stairwell [p.3] as shown.");
        assert_eq!(
            segments,
            [
                Segment::Text("The alarm is in the stairwell "),
                Segment::Cite(3),
                Segment::Text(" as shown."),
            ]
        );
    }

    #[test]
    fn several_citations_and_the_spellings_models_actually_use() {
        assert_eq!(
            split_citations("[p.1][P. 12] and [p2]"),
            [
                Segment::Cite(1),
                Segment::Cite(12),
                Segment::Text(" and "),
                Segment::Cite(2),
            ]
        );
    }

    #[test]
    fn anything_that_is_not_a_citation_stays_text() {
        for text in [
            "no citations here",
            "[p.four] is not a number",
            "[page] alone",
            "an unclosed [p.3 bracket",
            "[p.0] is not a page",
            "[]",
            "[",
        ] {
            let segments = split_citations(text);
            assert_eq!(segments, [Segment::Text(text)], "{text:?}");
        }
    }

    #[test]
    fn splitting_never_loses_or_duplicates_text() {
        let text = "start [p.1] middle [p.22] end";
        let rebuilt: String = split_citations(text)
            .iter()
            .map(|s| match s {
                Segment::Text(t) => (*t).to_owned(),
                Segment::Cite(p) => format!("[p.{p}]"),
            })
            .collect();
        assert_eq!(rebuilt, text);
        assert!(split_citations("").is_empty());
    }

    /// The panel nests a header and an input panel around a scrolling
    /// transcript; drawing one real frame is what proves that layout holds
    /// together (and that no two of its ids collide).
    #[test]
    fn one_frame_of_the_panel_draws_without_upsetting_the_session() {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        let mut dc = DocState::new(doc, &ctx, crate::render::engine::EnginePref::Hayro);
        dc.chat.open = true;
        dc.chat.messages = vec![
            ChatMessage::new(Role::User, "what is on page 2?"),
            ChatMessage::new(Role::Assistant, "The second page.\n\nSee [p.2]."),
        ];
        dc.chat.last_pages = vec![2];
        dc.chat.error = Some("could not reach the model".to_owned());

        let mut action = None;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            action = egui::Panel::right("chat")
                .show(ui, |ui| show(ui, &mut dc, None, true))
                .inner;
        });

        // Nothing was clicked, so nothing was asked and the panel stays open
        // with the conversation intact.
        assert!(action.is_none());
        assert!(dc.chat.open);
        assert_eq!(dc.chat.messages.len(), 2);
    }

    /// The toggle only appears when there is something to allow, and drawing
    /// the panel must never turn it on by itself.
    #[test]
    fn tools_are_not_allowed_until_the_box_is_ticked() {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        let mut dc = DocState::new(doc, &ctx, crate::render::engine::EnginePref::Hayro);
        dc.chat.open = true;
        assert!(!dc.chat.allow_tools, "off by default");

        for configured in [false, true] {
            let ctx = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(900.0, 700.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                egui::Panel::right("chat").show(ui, |ui| show(ui, &mut dc, None, configured));
            });
            assert!(
                !dc.chat.allow_tools,
                "drawing the panel must not grant tools (configured: {configured})"
            );
        }
    }

    #[test]
    fn the_sources_line_reads_as_a_sentence() {
        assert_eq!(page_list(&[3]), "page 3");
        assert_eq!(page_list(&[3, 4]), "pages 3 and 4");
        assert_eq!(page_list(&[3, 4, 7]), "pages 3, 4 and 7");
        assert!(page_list(&[]).is_empty());
    }
}
