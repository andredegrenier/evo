// Asking evo questions: about the document on screen, and about the library.
//
// One sheet, two conversations. The document tab quotes the pages behind it and
// a citation in an answer turns to that page. The agent tab has no document at
// all -- what it has is evo: it can search the library, open a document and
// mark a page, and it does those things while you watch, because the sheet
// floats over the app rather than replacing it.
//
// The answer arrives as server-sent events over a POST, which rules out
// `EventSource` -- it only does GET -- so the stream is read by hand. That is
// the `frames` generator below, and it is the whole of the transport: read
// bytes, split on a blank line, take the `event:` and `data:` fields. The data
// of every frame is JSON, so a model that writes a paragraph break cannot
// accidentally end its own answer.
//
// Five kinds of frame. `stage` is what the server is doing, `token` is the
// answer as it is written, `tool` is what the model is having evo do (a chip in
// the transcript, so a tool run is watched rather than discovered), `ui` is evo
// being driven -- open this, redraw that -- and `done` is the finished answer.
//
// Tools are off until they are switched on, per tab, remembered in
// localStorage. Handing a language model the controls of your own library is a
// thing to say yes to, not a default.
//
// Stopping is not a message to the server. Aborting the fetch closes the
// connection, which drops the receiver at the other end, which makes the next
// token fail to send, which ends the generation. One mechanism, no protocol.

import { api, get, reason, isDocId } from "./api.js";
import { showPage, refreshMarkup } from "./viewer.js";

const sheet = document.getElementById("chat");
const tabs = {
  doc: document.getElementById("tab-doc"),
  agent: document.getElementById("tab-agent"),
};
const bodies = {
  doc: document.getElementById("panel-doc"),
  agent: document.getElementById("panel-agent"),
};

/// Which conversation is showing.
let showing = "doc";

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
// evo, being driven
// ---------------------------------------------------------------------------

/// What a `ui` frame asks the app to do.
///
/// Opening goes through the hash, which is to say through the router: an agent
/// asking for a document is exactly a reader tapping one, and there is no
/// reason for it to be a second mechanism.
function drive(event) {
  if (!event || !isDocId(event.doc)) return;
  const page = Math.max(1, Number(event.page) || 1);
  if (event.action === "open") {
    location.hash = `#doc/${event.doc}/${page}`;
  } else if (event.action === "markup-changed") {
    // The version tag moved, so whatever overlay the viewer is holding for
    // this document is out of date.
    refreshMarkup(event.doc, page);
  }
}

// ---------------------------------------------------------------------------
// Whether the model may drive
// ---------------------------------------------------------------------------

/// A phone in private browsing has no storage, and a forgotten preference is
/// not worth failing over -- it just means the toggle starts off again.
function remember(key, on) {
  try {
    localStorage.setItem(key, on ? "yes" : "no");
  } catch {
    // Nothing to do about it, and nothing that needs saying.
  }
}

function recall(key) {
  try {
    return localStorage.getItem(key) === "yes";
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// One conversation
// ---------------------------------------------------------------------------

class Panel {
  constructor(name, elements) {
    this.name = name;
    for (const [part, id] of Object.entries(elements)) {
      this[part] = document.getElementById(id);
    }
    /// The document being discussed, for the tab that has one.
    this.id = null;
    this.messages = [];
    /// The fetch in flight, if there is one. Aborting it is how Stop works.
    this.asking = null;

    this.tools.checked = recall(this.key);
    this.tools.addEventListener("change", () =>
      remember(this.key, this.tools.checked),
    );
    this.form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const text = this.question.value.trim();
      if (text === "" || this.asking) return;
      this.question.value = "";
      await this.ask(text);
    });
    this.stop.addEventListener("click", () => this.abort());
  }

  get key() {
    return `evo.tools.${this.name}`;
  }

  /// Where this conversation's questions go.
  get url() {
    return this.id ? `/api/docs/${this.id}/chat` : "/api/agent/chat";
  }

  say(text) {
    this.status.textContent = text || "";
  }

  draw() {
    this.transcript.replaceChildren();
    for (const message of this.messages) {
      this.transcript.append(turn(message.role, message.content));
    }
    this.toBottom();
  }

  toBottom() {
    this.transcript.scrollTop = this.transcript.scrollHeight;
  }

  abort() {
    if (this.asking) this.asking.abort();
  }

  busy(running) {
    this.stop.hidden = !running;
    this.send.hidden = running;
    this.question.disabled = running;
  }

  /// Ask one question and read the answer as it is written.
  async ask(text) {
    const asked = turn("user", text);
    this.transcript.append(asked);
    const reply = turn("assistant", "");
    this.transcript.append(reply);
    this.toBottom();

    this.asking = new AbortController();
    this.busy(true);
    this.say("Thinking…");

    let streamed = "";
    let failure = null;
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "text/event-stream",
          "X-Evo": "1",
        },
        body: JSON.stringify({
          question: text,
          history: this.messages,
          tools: this.tools.checked,
        }),
        signal: this.asking.signal,
      });
      if (!response.ok || !response.body) {
        const answer = {
          status: response.status,
          ok: false,
          data: {},
          headers: response.headers,
        };
        try {
          answer.data = await response.json();
        } catch {
          // A refusal with no body still has a status.
        }
        failure = reason(answer, "evo could not answer that.");
      } else {
        for await (const frame of frames(response.body)) {
          if (frame.name === "stage") {
            this.say(frame.data.text || "");
          } else if (frame.name === "token") {
            streamed += frame.data.text || "";
            fill(reply, streamed);
            this.toBottom();
          } else if (frame.name === "tool") {
            // A chip goes *before* the reply so the transcript reads in the
            // order things happened: the model spoke, then it did this.
            this.transcript.insertBefore(chip(frame.data.text || ""), reply);
            this.toBottom();
          } else if (frame.name === "ui") {
            drive(frame.data);
          } else if (frame.name === "done") {
            streamed = frame.data.text || streamed;
            fill(reply, streamed);
            this.say("");
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

    this.asking = null;
    this.busy(false);

    // Whatever arrived before the stop is still an answer, and throwing it away
    // would be a worse response to "stop" than keeping it.
    if (streamed.trim() !== "") {
      this.messages.push({ role: "user", content: text });
      this.messages.push({ role: "assistant", content: streamed });
      this.say(failure || "");
      await this.keep();
      return;
    }

    // Nothing was said: take the question back off the screen and put it back
    // in the box, so asking again is one tap.
    asked.remove();
    reply.remove();
    this.question.value = text;
    this.say(failure || "");
  }

  /// Keep the conversation, if it belongs to a document. Failing is not worth
  /// interrupting anyone over -- what is on screen is still right, it just
  /// will not be there tomorrow. The agent's conversation is about the library
  /// rather than about any one document, and there is nowhere to file it.
  async keep() {
    if (!this.id) return;
    await api(`/api/docs/${this.id}/chatlog`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ messages: this.messages }),
    });
  }
}

const panels = {
  doc: new Panel("doc", {
    transcript: "transcript",
    status: "chat-status",
    form: "chat-form",
    question: "chat-question",
    send: "chat-send",
    stop: "chat-stop",
    tools: "chat-tools",
  }),
  agent: new Panel("agent", {
    transcript: "agent-transcript",
    status: "agent-status",
    form: "agent-form",
    question: "agent-question",
    send: "agent-send",
    stop: "agent-stop",
    tools: "agent-tools",
  }),
};

// ---------------------------------------------------------------------------
// What one turn looks like
// ---------------------------------------------------------------------------

/// One thing somebody said. Citations become buttons; everything else is text,
/// because an answer is a model's words and never markup.
function turn(role, content) {
  const element = document.createElement("div");
  element.className = `turn ${role === "user" ? "user" : "assistant"}`;
  fill(element, content);
  return element;
}

/// One thing the model had evo do. Not a turn: nobody said it.
function chip(text) {
  const element = document.createElement("div");
  element.className = "chip";
  element.textContent = text;
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

// ---------------------------------------------------------------------------
// The sheet
// ---------------------------------------------------------------------------

function select(which) {
  showing = which;
  for (const [name, tab] of Object.entries(tabs)) {
    tab.setAttribute("aria-selected", String(name === which));
    bodies[name].hidden = name !== which;
  }
  panels[which].question.focus();
}

for (const [name, tab] of Object.entries(tabs)) {
  tab.addEventListener("click", () => select(name));
}
document.getElementById("chat-close").addEventListener("click", closeChat);

/// Open the sheet on `id`'s conversation, with whatever was said last time.
export async function openChat(id) {
  sheet.hidden = false;
  tabs.doc.disabled = false;
  select("doc");
  const panel = panels.doc;
  if (panel.id !== id) {
    panel.id = id;
    panel.messages = [];
    panel.transcript.replaceChildren();
    panel.say("");
    const answer = await get(`/api/docs/${id}/chatlog`);
    // A conversation that cannot be read is an empty one, not an error: the
    // question the reader is about to ask works either way.
    panel.messages = (answer.ok && answer.data.messages) || [];
    panel.draw();
  }
}

/// Open the sheet on the agent, which needs no document.
export function openAgent() {
  sheet.hidden = false;
  tabs.doc.disabled = panels.doc.id === null;
  select("agent");
}

export function closeChat() {
  for (const panel of Object.values(panels)) panel.abort();
  sheet.hidden = true;
}

export function isChatOpen() {
  return !sheet.hidden;
}

/// The route changed. The agent's conversation is about the library, so it
/// stays open wherever the reader goes -- that is how they watch it work. A
/// conversation about a document they have left is not, and closes with it.
export function onRoute(id) {
  if (sheet.hidden || showing === "agent") return;
  if (panels.doc.id !== id) closeChat();
}
