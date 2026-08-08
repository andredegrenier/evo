// Asking the open document questions.
//
// A sheet over the bottom of the viewer, because the answer is about the page
// behind it and a citation is a link back to that page. Nothing here knows how
// to draw a page; it asks the viewer to.
//
// The answer arrives as server-sent events over a POST, which rules out
// `EventSource` -- it only does GET -- so the stream is read by hand. That is
// the `frames` generator below, and it is the whole of the transport: read
// bytes, split on a blank line, take the `event:` and `data:` fields. The data
// of every frame is JSON, so a model that writes a paragraph break cannot
// accidentally end its own answer.
//
// Stopping is not a message to the server. Aborting the fetch closes the
// connection, which drops the receiver at the other end, which makes the next
// token fail to send, which ends the generation. One mechanism, no protocol.

import { api, get, reason } from "./api.js";
import { showPage } from "./viewer.js";

const sheet = document.getElementById("chat");
const transcript = document.getElementById("transcript");
const form = document.getElementById("chat-form");
const question = document.getElementById("chat-question");
const stopButton = document.getElementById("chat-stop");
const sendButton = document.getElementById("chat-send");

/// Which document is being discussed, and everything said about it so far.
let conversation = null;
/// The fetch in flight, if there is one. Aborting it is how Stop works.
let asking = null;

/// Open the chat about `id`, with whatever was said last time.
export async function openChat(id) {
  sheet.hidden = false;
  if (!conversation || conversation.id !== id) {
    conversation = { id, messages: [] };
    transcript.replaceChildren();
    say("");
    const answer = await get(`/api/docs/${id}/chatlog`);
    // A conversation that cannot be read is an empty one, not an error: the
    // question the reader is about to ask works either way.
    conversation.messages = (answer.ok && answer.data.messages) || [];
    draw();
  }
  question.focus();
}

export function closeChat() {
  stop();
  sheet.hidden = true;
}

export function isChatOpen() {
  return !sheet.hidden;
}

function say(text) {
  document.getElementById("chat-status").textContent = text || "";
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

function draw() {
  transcript.replaceChildren();
  for (const message of conversation.messages) {
    transcript.append(turn(message.role, message.content));
  }
  toBottom();
}

/// One thing somebody said. Citations become buttons; everything else is text,
/// because an answer is a model's words and never markup.
function turn(role, content) {
  const element = document.createElement("div");
  element.className = `turn ${role === "user" ? "user" : "assistant"}`;
  fill(element, content);
  return element;
}

function fill(element, content) {
  element.replaceChildren();
  const citation = /\[p\.(\d+)\]/g;
  let at = 0;
  let found;
  while ((found = citation.exec(content)) !== null) {
    if (found.index > at) element.append(content.slice(at, found.index));
    const page = Number(found[1]);
    const link = document.createElement("button");
    link.type = "button";
    link.className = "cite";
    link.textContent = found[0];
    link.addEventListener("click", () => showPage(page));
    element.append(link);
    at = found.index + found[0].length;
  }
  if (at < content.length) element.append(content.slice(at));
}

function toBottom() {
  transcript.scrollTop = transcript.scrollHeight;
}

/// Keep the conversation. Failing is not worth interrupting anyone over --
/// what is on screen is still right, it just will not be there tomorrow.
async function keep() {
  await api(`/api/docs/${conversation.id}/chatlog`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ messages: conversation.messages }),
  });
}

// ---------------------------------------------------------------------------
// Reading the stream
// ---------------------------------------------------------------------------

/// Server-sent events out of a `fetch` body: one object per frame.
///
/// A frame ends at a blank line. Lines beginning with a colon are comments --
/// the keep-alive is one -- and a frame with no data is nothing to report.
async function* frames(body) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffered += decoder.decode(value, { stream: true });
    let end;
    while ((end = buffered.indexOf("\n\n")) !== -1) {
      const block = buffered.slice(0, end);
      buffered = buffered.slice(end + 2);
      const frame = read(block);
      if (frame) yield frame;
    }
  }
}

function read(block) {
  let name = "message";
  const data = [];
  for (const raw of block.split("\n")) {
    const line = raw.replace(/\r$/, "");
    if (line.startsWith(":")) continue;
    if (line.startsWith("event:")) name = line.slice(6).trim();
    // Per the spec a space after the colon is part of the syntax, not the
    // data; several data lines in one frame are joined with newlines.
    else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
  }
  if (data.length === 0) return null;
  try {
    return { name, data: JSON.parse(data.join("\n")) };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = question.value.trim();
  if (text === "" || asking) return;
  question.value = "";
  await ask(text);
});

stopButton.addEventListener("click", stop);
document.getElementById("chat-close").addEventListener("click", closeChat);

function stop() {
  if (asking) asking.abort();
}

/// Ask one question and read the answer as it is written.
async function ask(text) {
  const id = conversation.id;
  const asked = turn("user", text);
  transcript.append(asked);
  const reply = turn("assistant", "");
  transcript.append(reply);
  toBottom();

  asking = new AbortController();
  busy(true);
  say("Thinking…");

  let streamed = "";
  let failure = null;
  try {
    const response = await fetch(`/api/docs/${id}/chat`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "text/event-stream",
        "X-Evo": "1",
      },
      // `tools` is the agent's, and arrives with it; the server reads the
      // field today and answers from the document either way.
      body: JSON.stringify({
        question: text,
        history: conversation.messages,
        tools: false,
      }),
      signal: asking.signal,
    });
    if (!response.ok || !response.body) {
      const answer = { status: response.status, ok: false, data: {}, headers: response.headers };
      try {
        answer.data = await response.json();
      } catch {
        // A refusal with no body still has a status.
      }
      failure = reason(answer, "evo could not answer that.");
    } else {
      for await (const frame of frames(response.body)) {
        if (frame.name === "stage") {
          say(frame.data.text || "");
        } else if (frame.name === "token") {
          streamed += frame.data.text || "";
          fill(reply, streamed);
          toBottom();
        } else if (frame.name === "done") {
          streamed = frame.data.text || streamed;
          fill(reply, streamed);
          say("");
        } else if (frame.name === "error") {
          failure = frame.data.error || "evo could not answer that.";
        }
      }
    }
  } catch (e) {
    // Aborting is the reader pressing Stop, which is not a failure.
    if (!(e && e.name === "AbortError")) {
      failure = "evo stopped answering. Check the connection.";
    }
  }

  asking = null;
  busy(false);

  // Whatever arrived before the stop is still an answer, and throwing it away
  // would be a worse response to "stop" than keeping it.
  if (streamed.trim() !== "") {
    conversation.messages.push({ role: "user", content: text });
    conversation.messages.push({ role: "assistant", content: streamed });
    say(failure || "");
    await keep();
    return;
  }

  // Nothing was said: take the question back off the screen and put it back in
  // the box, so asking again is one tap.
  asked.remove();
  reply.remove();
  question.value = text;
  say(failure || "");
}

function busy(running) {
  stopButton.hidden = !running;
  sendButton.hidden = running;
  question.disabled = running;
}
