//! Tool calling for a model that has no tool-calling API: the built-in one.
//!
//! A server implements tools by parsing the model's output against whatever
//! format its template uses and handing back structured calls. llama.cpp does
//! this internally but does not expose the parser through its C API, so evo
//! does the same thing in the open: the tools are described in the system
//! prompt, and the `<tool_call>` blocks the model writes are read back out of
//! the answer.
//!
//! The format is the Hermes one, which is what the Qwen3 models in the
//! catalogue are trained on:
//!
//! ```text
//! <tool_call>
//! {"name": "search_library", "arguments": {"query": "boiler"}}
//! </tool_call>
//! ```
//!
//! Everything here is a pure function over strings, and it is compiled whether
//! or not the engine is, so it is always under test.

use crate::script::model::{ToolCall, ToolDef};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// The system prompt the model is given when tools are on offer: whatever the
/// caller wanted to say, then the tools and how to ask for them.
pub fn system_with_tools(system: Option<&str>, tools: &[ToolDef]) -> String {
    let mut out = String::new();
    if let Some(system) = system {
        out.push_str(system.trim());
        out.push_str("\n\n");
    }
    out.push_str(
        "# Tools\n\nYou may call one or more functions to help answer the \
         question. The functions available to you are described inside \
         <tools></tools> tags:\n<tools>\n",
    );
    for tool in tools {
        out.push_str(&tool.wire().to_string());
        out.push('\n');
    }
    out.push_str(
        "</tools>\n\nTo call one, write a JSON object with the function's name \
         and its arguments inside <tool_call></tool_call> tags, and nothing \
         else:\n<tool_call>\n{\"name\": \"function-name\", \"arguments\": \
         {\"argument\": \"value\"}}\n</tool_call>\nCall a function only when \
         it is needed; otherwise answer directly.",
    );
    out
}

/// An assistant turn that asked for tools, written the way the model wrote it,
/// so that replaying the conversation looks like its own output.
pub fn render_tool_calls(text: &str, calls: &[ToolCall]) -> String {
    let mut out = text.trim().to_owned();
    for call in calls {
        if !out.is_empty() {
            out.push('\n');
        }
        let body = serde_json::json!({"name": call.name, "arguments": call.arguments});
        out.push_str(&format!("{OPEN}\n{body}\n{CLOSE}"));
    }
    out
}

/// What a tool answered, as a turn the model will recognize. There is no tool
/// role in this format: the result comes back as if the user pasted it.
pub fn render_tool_result(name: Option<&str>, content: &str) -> String {
    match name {
        Some(name) => format!("<tool_response>\n{name}: {content}\n</tool_response>"),
        None => format!("<tool_response>\n{content}\n</tool_response>"),
    }
}

/// Split an answer into the text meant for the reader and the calls the model
/// asked for. Malformed blocks are left in the text: showing the user
/// something odd beats silently swallowing what the model said.
pub fn parse_tool_calls(reply: &str) -> (String, Vec<ToolCall>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut rest = reply;

    while let Some(start) = rest.find(OPEN) {
        let Some(end_offset) = rest[start + OPEN.len()..].find(CLOSE) else {
            break;
        };
        let body_start = start + OPEN.len();
        let body = &rest[body_start..body_start + end_offset];
        match parse_call(body) {
            Some(call) => {
                text.push_str(&rest[..start]);
                calls.push(call);
            }
            // Not a call after all; keep it where it was.
            None => {
                text.push_str(&rest[..body_start + end_offset + CLOSE.len()]);
            }
        }
        rest = &rest[body_start + end_offset + CLOSE.len()..];
    }
    text.push_str(rest);
    (text.trim().to_owned(), calls)
}

/// What a reader should see of an answer that is still arriving: the text
/// with the `<tool_call>` blocks taken out, and an unfinished one held back.
///
/// It only ever grows, so a caller can stream whatever is new since last time.
/// A block that turns out not to parse is hidden here but kept by
/// [`parse_tool_calls`], so it reappears in the finished answer -- better than
/// showing half a call and taking it away again.
pub fn visible_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + OPEN.len()..].find(CLOSE) else {
            // Still being written: nothing of it is shown.
            return out;
        };
        rest = &rest[start + OPEN.len() + end + CLOSE.len()..];
    }
    out.push_str(visible_prefix(rest));
    out
}

/// Everything before a tail that could be the start of `<tool_call>`. The
/// opening tag arrives a few characters at a time like everything else.
fn visible_prefix(text: &str) -> &str {
    // A tail that could be the start of the tag waits for the next piece to
    // say whether it is one or just an angle bracket.
    for len in (1..OPEN.len()).rev() {
        let cut = text.len() - len.min(text.len());
        if text.is_char_boundary(cut) && text.as_bytes()[cut..] == OPEN.as_bytes()[..len] {
            return &text[..cut];
        }
    }
    text
}

fn parse_call(body: &str) -> Option<ToolCall> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    // Hermes says "name"/"arguments"; some models write the OpenAI shape.
    let function = if value.get("function").is_some() {
        &value["function"]
    } else {
        &value
    };
    let name = function["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let arguments = match &function["arguments"] {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        // Written as a string of JSON, which is what the OpenAI shape does.
        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(serde_json::json!({})),
        _ => serde_json::json!({}),
    };
    Some(ToolCall {
        id: value["id"].as_str().map(str::to_owned),
        ..ToolCall::new(name, arguments)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, parameters: serde_json::Value) -> ToolDef {
        ToolDef {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
        }
    }

    fn tools() -> Vec<ToolDef> {
        vec![
            tool(
                "search_library",
                "Search the library",
                serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            ),
            tool("list_library", "List documents", serde_json::json!({})),
        ]
    }

    #[test]
    fn the_system_prompt_describes_every_tool_and_the_format() {
        let prompt = system_with_tools(Some("Answer from the pages."), &tools());
        assert!(prompt.starts_with("Answer from the pages."));
        assert!(prompt.contains("<tools>") && prompt.contains("</tools>"));
        assert!(prompt.contains("search_library") && prompt.contains("list_library"));
        assert!(prompt.contains("Search the library"));
        assert!(prompt.contains("<tool_call>"));

        // Each tool is one line of JSON that a model could parse itself.
        let inside = prompt
            .split("<tools>\n")
            .nth(1)
            .and_then(|s| s.split("\n</tools>").next())
            .expect("a tools block");
        for line in inside.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect(line);
            assert_eq!(v["type"], "function");
        }

        // With no system prompt of its own it still stands on its own.
        let bare = system_with_tools(None, &tools());
        assert!(bare.starts_with("# Tools"));
    }

    #[test]
    fn a_call_is_read_out_of_the_answer_and_taken_off_the_text() {
        let reply = "Let me look that up.\n<tool_call>\n\
                     {\"name\": \"search_library\", \"arguments\": {\"query\": \"boiler\"}}\n\
                     </tool_call>";
        let (text, calls) = parse_tool_calls(reply);
        assert_eq!(text, "Let me look that up.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search_library");
        assert_eq!(calls[0].arguments["query"], "boiler");
    }

    #[test]
    fn several_calls_in_one_answer_all_come_out() {
        let reply = "<tool_call>{\"name\": \"a\", \"arguments\": {}}</tool_call>\
                     between\
                     <tool_call>{\"name\": \"b\", \"arguments\": {\"n\": 2}}</tool_call>after";
        let (text, calls) = parse_tool_calls(reply);
        assert_eq!(text, "betweenafter");
        assert_eq!(
            calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(calls[1].arguments["n"], 2);
    }

    #[test]
    fn an_answer_with_no_calls_comes_back_unchanged() {
        let (text, calls) = parse_tool_calls("The boiler is on page 3. [p.3]");
        assert_eq!(text, "The boiler is on page 3. [p.3]");
        assert!(calls.is_empty());
    }

    /// A block that is not a call must not vanish: the reader sees what the
    /// model actually said.
    #[test]
    fn malformed_blocks_are_left_in_the_text() {
        for reply in [
            "before <tool_call>not json</tool_call> after",
            "before <tool_call>{\"arguments\": {}}</tool_call> after",
            "before <tool_call>{\"name\": \"\"}</tool_call> after",
        ] {
            let (text, calls) = parse_tool_calls(reply);
            assert!(calls.is_empty(), "{reply}");
            assert!(text.contains("<tool_call>"), "{reply}");
            assert!(text.contains("after"), "{reply}");
        }

        // Never closed: nothing is parsed and nothing is lost.
        let (text, calls) = parse_tool_calls("before <tool_call>{\"name\": \"a\"}");
        assert!(calls.is_empty());
        assert!(text.contains("before") && text.contains("{\"name\": \"a\"}"));
    }

    /// Models trained on the OpenAI shape write it even under this prompt.
    #[test]
    fn the_openai_shape_is_read_too() {
        let reply = "<tool_call>{\"function\": {\"name\": \"a\", \
                     \"arguments\": \"{\\\"n\\\": 1}\"}}</tool_call>";
        let (_, calls) = parse_tool_calls(reply);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].arguments["n"], 1);
    }

    #[test]
    fn arguments_that_are_not_an_object_become_an_empty_one() {
        let reply = "<tool_call>{\"name\": \"a\", \"arguments\": 7}</tool_call>";
        let (_, calls) = parse_tool_calls(reply);
        assert_eq!(calls[0].arguments, serde_json::json!({}));

        let reply = "<tool_call>{\"name\": \"a\"}</tool_call>";
        let (_, calls) = parse_tool_calls(reply);
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    /// What was parsed can be written back, so replaying a conversation shows
    /// the model its own words.
    #[test]
    fn calls_round_trip_through_rendering() {
        let original = "<tool_call>\n{\"name\": \"a\", \"arguments\": {\"n\": 1}}\n</tool_call>";
        let (text, calls) = parse_tool_calls(original);
        let rendered = render_tool_calls(&text, &calls);
        let (text_again, calls_again) = parse_tool_calls(&rendered);
        assert_eq!(text_again, text);
        assert_eq!(calls_again, calls);
    }

    #[test]
    fn a_tool_result_is_a_response_block() {
        let with_name = render_tool_result(Some("search_library"), "3 documents");
        assert!(with_name.starts_with("<tool_response>"));
        assert!(with_name.contains("search_library: 3 documents"));
        assert!(with_name.ends_with("</tool_response>"));

        let without = render_tool_result(None, "3 documents");
        assert!(without.contains("3 documents"));
        assert!(!without.contains(':'));
    }

    /// Nothing of a call ever reaches the reader, and what does reach them
    /// only ever grows: an answer that streams a character at a time must not
    /// show something and then take it away.
    #[test]
    fn a_call_is_held_back_from_the_reader_as_it_is_written() {
        let whole = "Let me look.\n<tool_call>\n{\"name\": \"a\", \"arguments\": {}}\n\
                     </tool_call>\nTwo matches.";
        let mut last = String::new();
        for end in 1..=whole.len() {
            if !whole.is_char_boundary(end) {
                continue;
            }
            let visible = visible_text(&whole[..end]);
            assert!(!visible.contains('<'), "a call leaked: {visible:?}");
            assert!(
                visible.starts_with(&last) || last.starts_with(&visible),
                "{last:?} then {visible:?}"
            );
            assert!(visible.len() >= last.len(), "{last:?} then {visible:?}");
            last = visible;
        }
        assert_eq!(last, "Let me look.\n\nTwo matches.");
    }

    #[test]
    fn text_around_a_call_is_shown_and_an_angle_bracket_is_not_a_call() {
        assert_eq!(visible_text("Let me look."), "Let me look.");
        assert_eq!(visible_text("a < b"), "a < b");
        assert_eq!(visible_text("é<t"), "é");
        assert_eq!(
            visible_text("<tool_call>{}</tool_call> and <tool_call>{\"na"),
            " and "
        );
    }
}
