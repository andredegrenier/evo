//! The loop that lets a model use tools.
//!
//! Generate; if the model only spoke, that is the answer. If it asked for
//! tools, run them, append what it asked and what came back to the
//! conversation, and generate again. Every backend reports tool calls the same
//! way ([`GenerateOutcome`]) whether the dialect has an API for them or the
//! built-in engine parsed them out of the text, so this loop is written once.
//!
//! The iteration cap is the whole safety story: a model that keeps asking for
//! tools without ever answering is stopped with a sentence rather than left to
//! spend the afternoon.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::script::model::{
    ChatMessage, GenerateOutcome, GenerateRequest, ModelBackend, ModelError, ToolCall,
};

/// How many rounds of tool calls one question may take.
pub const MAX_ITERATIONS: usize = 5;

/// Run `request` to an answer, running whatever tools the model asks for along
/// the way.
///
/// `execute` runs one tool: `Ok` is its result and `Err` is a message the
/// *model* reads, since a tool that failed is something it can work around and
/// not something the user has to be shown. `on_token` receives the text of
/// every round, and returning [`ControlFlow::Break`] from it abandons the
/// request as usual.
pub fn run_agent(
    backend: &dyn ModelBackend,
    request: GenerateRequest,
    execute: &mut dyn FnMut(&ToolCall) -> Result<String, String>,
    max_iterations: usize,
    on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
    cancel: &AtomicBool,
) -> Result<GenerateOutcome, ModelError> {
    let mut request = request;
    for _ in 0..max_iterations.max(1) {
        if cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        let outcome = backend.generate(&request, on_token)?;
        if outcome.tool_calls.is_empty() {
            return Ok(outcome);
        }

        // What was asked this round becomes part of the conversation, so the
        // next round sees the question, the request, and the answers.
        if !request.prompt.is_empty() {
            let prompt = std::mem::take(&mut request.prompt);
            request
                .history
                .push(ChatMessage::new(crate::script::model::Role::User, prompt));
        }
        request.history.push(ChatMessage::calling(
            &outcome.text,
            outcome.tool_calls.clone(),
        ));
        for call in &outcome.tool_calls {
            if cancel.load(Ordering::Relaxed) {
                return Err(ModelError::Cancelled);
            }
            let result = execute(call).unwrap_or_else(|e| format!("The tool failed: {e}"));
            request.history.push(ChatMessage::tool_result(call, result));
        }
    }
    Err(ModelError::Read(format!(
        "the model kept asking for tools without answering ({} rounds); \
         nothing was returned",
        max_iterations.max(1)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::model::{Role, ToolDef};
    use std::sync::Mutex;

    /// A backend that answers from a script: one prepared outcome per call,
    /// recording the request it was given each time.
    struct ScriptedBackend {
        replies: Mutex<std::collections::VecDeque<GenerateOutcome>>,
        seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl ScriptedBackend {
        fn new(replies: Vec<GenerateOutcome>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl ModelBackend for ScriptedBackend {
        fn generate(
            &self,
            req: &GenerateRequest,
            on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> Result<GenerateOutcome, ModelError> {
            self.seen.lock().unwrap().push(req.history.clone());
            let outcome = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| GenerateOutcome::text("out of script"));
            if !outcome.text.is_empty() && on_token(&outcome.text).is_break() {
                return Err(ModelError::Cancelled);
            }
            Ok(outcome)
        }

        fn list_models(&self) -> Result<Vec<String>, ModelError> {
            Ok(vec!["scripted".to_owned()])
        }

        fn describe(&self) -> String {
            "scripted (test)".to_owned()
        }
    }

    fn request() -> GenerateRequest {
        GenerateRequest {
            model: "m".into(),
            prompt: "what is in the library about boilers?".into(),
            tools: vec![ToolDef {
                name: "search_library".into(),
                description: "Search the library".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..Default::default()
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: Some(format!("call_{name}")),
            name: name.to_owned(),
            arguments: serde_json::json!({"query": "boiler"}),
        }
    }

    #[test]
    fn an_answer_without_tool_calls_is_the_answer() {
        let backend = ScriptedBackend::new(vec![GenerateOutcome::text("Two documents. [p.1]")]);
        let mut ran = Vec::new();
        let outcome = run_agent(
            &backend,
            request(),
            &mut |c: &ToolCall| {
                ran.push(c.name.clone());
                Ok(String::new())
            },
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(false),
        )
        .expect("an answer");

        assert_eq!(outcome.text, "Two documents. [p.1]");
        assert_eq!(backend.calls(), 1, "one round, no tools");
        assert!(ran.is_empty());
    }

    #[test]
    fn a_tool_call_is_run_and_its_result_goes_back_to_the_model() {
        let backend = ScriptedBackend::new(vec![
            GenerateOutcome {
                text: "Let me look.".into(),
                tool_calls: vec![call("search_library")],
            },
            GenerateOutcome::text("Two: the boiler report and the survey."),
        ]);
        let mut ran = Vec::new();
        let mut streamed = Vec::new();
        let outcome = run_agent(
            &backend,
            request(),
            &mut |c: &ToolCall| {
                ran.push((
                    c.name.clone(),
                    c.arguments["query"].as_str().unwrap_or("").to_owned(),
                ));
                Ok("2 matches".to_owned())
            },
            MAX_ITERATIONS,
            &mut |chunk: &str| {
                streamed.push(chunk.to_owned());
                ControlFlow::Continue(())
            },
            &AtomicBool::new(false),
        )
        .expect("an answer");

        assert_eq!(outcome.text, "Two: the boiler report and the survey.");
        assert!(outcome.tool_calls.is_empty());
        assert_eq!(ran, [("search_library".to_owned(), "boiler".to_owned())]);
        assert_eq!(backend.calls(), 2);
        // Both rounds streamed: the reader sees the model thinking aloud.
        assert_eq!(
            streamed,
            ["Let me look.", "Two: the boiler report and the survey."]
        );

        // The second round saw the question, the request, and the result.
        let second = &backend.seen.lock().unwrap()[1];
        let roles: Vec<Role> = second.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool]);
        assert_eq!(second[0].content, "what is in the library about boilers?");
        assert_eq!(second[1].tool_calls.len(), 1);
        assert_eq!(second[2].content, "2 matches");
        assert_eq!(
            second[2].tool_call_id.as_deref(),
            Some("call_search_library")
        );
        assert_eq!(second[2].name.as_deref(), Some("search_library"));
    }

    /// Several calls in one round all run, in order, before the next round.
    #[test]
    fn every_call_of_one_round_is_answered() {
        let backend = ScriptedBackend::new(vec![
            GenerateOutcome {
                text: String::new(),
                tool_calls: vec![call("first"), call("second")],
            },
            GenerateOutcome::text("done"),
        ]);
        let mut ran = Vec::new();
        run_agent(
            &backend,
            request(),
            &mut |c: &ToolCall| {
                ran.push(c.name.clone());
                Ok(format!("{} result", c.name))
            },
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(false),
        )
        .expect("an answer");

        assert_eq!(ran, ["first", "second"]);
        let second = &backend.seen.lock().unwrap()[1];
        assert_eq!(second.len(), 4, "question, request, two results");
        assert_eq!(second[2].content, "first result");
        assert_eq!(second[3].content, "second result");
    }

    /// A tool that fails tells the model so; it is not the user's problem to
    /// solve, and the conversation carries on.
    #[test]
    fn a_failing_tool_reports_itself_to_the_model() {
        let backend = ScriptedBackend::new(vec![
            GenerateOutcome {
                text: String::new(),
                tool_calls: vec![call("search_library")],
            },
            GenerateOutcome::text("I could not search, but the index lists two."),
        ]);
        let outcome = run_agent(
            &backend,
            request(),
            &mut |_: &ToolCall| Err("the library is not open".to_owned()),
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(false),
        )
        .expect("an answer");
        assert!(outcome.text.starts_with("I could not search"));

        let second = &backend.seen.lock().unwrap()[1];
        assert_eq!(second[2].role, Role::Tool);
        assert!(second[2].content.contains("the library is not open"));
    }

    /// A model that only ever asks for tools is stopped, and says why.
    #[test]
    fn the_loop_gives_up_after_the_cap() {
        let asking = || GenerateOutcome {
            text: String::new(),
            tool_calls: vec![call("search_library")],
        };
        let backend = ScriptedBackend::new((0..10).map(|_| asking()).collect());
        let mut runs = 0;
        let err = run_agent(
            &backend,
            request(),
            &mut |_: &ToolCall| {
                runs += 1;
                Ok("nothing".to_owned())
            },
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(false),
        )
        .expect_err("the cap");

        assert_eq!(backend.calls(), MAX_ITERATIONS);
        assert_eq!(runs, MAX_ITERATIONS);
        assert!(err.to_string().contains("kept asking for tools"), "{err}");
    }

    #[test]
    fn cancelling_stops_the_loop_between_rounds() {
        let backend = ScriptedBackend::new(vec![
            GenerateOutcome {
                text: String::new(),
                tool_calls: vec![call("search_library")],
            },
            GenerateOutcome::text("never reached"),
        ]);
        let cancel = AtomicBool::new(false);
        let err = run_agent(
            &backend,
            request(),
            &mut |_: &ToolCall| {
                // The user pressed Stop while the tool was running.
                cancel.store(true, Ordering::Relaxed);
                Ok("half an answer".to_owned())
            },
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &cancel,
        )
        .expect_err("cancelled");
        assert!(matches!(err, ModelError::Cancelled), "{err}");
        assert_eq!(backend.calls(), 1);

        // Cancelled before it began: nothing is asked at all.
        let backend = ScriptedBackend::new(vec![GenerateOutcome::text("hello")]);
        let err = run_agent(
            &backend,
            request(),
            &mut |_: &ToolCall| Ok(String::new()),
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(true),
        )
        .expect_err("cancelled");
        assert!(matches!(err, ModelError::Cancelled));
        assert_eq!(backend.calls(), 0);
    }

    /// A model error is the caller's to report, not something to loop over.
    #[test]
    fn a_backend_failure_comes_straight_back() {
        struct Failing;
        impl ModelBackend for Failing {
            fn generate(
                &self,
                _req: &GenerateRequest,
                _on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
            ) -> Result<GenerateOutcome, ModelError> {
                Err(ModelError::Unavailable("no model".to_owned()))
            }
            fn list_models(&self) -> Result<Vec<String>, ModelError> {
                Ok(Vec::new())
            }
            fn describe(&self) -> String {
                "failing".to_owned()
            }
        }
        let err = run_agent(
            &Failing,
            request(),
            &mut |_: &ToolCall| Ok(String::new()),
            MAX_ITERATIONS,
            &mut |_: &str| ControlFlow::Continue(()),
            &AtomicBool::new(false),
        )
        .expect_err("no model");
        assert!(matches!(err, ModelError::Unavailable(_)), "{err}");
    }
}
