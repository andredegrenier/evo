// The shell: who is signed in, which view is showing, and the library.
//
// The router is the hash. `#library` is the list, `#doc/<id>/<page>` is one
// document open at one page -- so the back button, a bookmark and a link from
// a search result are all the same mechanism.

import { api, get, postJson, reason, isDocId, thumbUrl } from "./api.js";
import { openDocument, closeDocument } from "./viewer.js";

const views = {
  login: document.getElementById("view-login"),
  library: document.getElementById("view-library"),
  doc: document.getElementById("view-doc"),
};

/// Show one view and hide the rest. Nothing is destroyed: the viewer keeps
/// its pages so going back to the same document is instant.
function show(name) {
  for (const [key, section] of Object.entries(views)) {
    section.hidden = key !== name;
  }
}

function say(id, text) {
  document.getElementById(id).textContent = text || "";
}

// ---------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------

const loginForm = document.getElementById("login");
const enrolment = document.getElementById("enroll");
const qr = document.getElementById("qr");

loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  say("login-message", "");
  const password = document.getElementById("password").value;
  const code = document.getElementById("code").value.trim();
  const answer = await postJson("/api/login", {
    password,
    code: code === "" ? null : code,
  });

  // First leg of enrolment: the password is right, but there is no
  // authenticator app yet, so the server offers the QR code instead.
  if (answer.data.enroll) {
    enrolment.hidden = false;
    qr.src = "/api/setup-qr?t=" + encodeURIComponent(answer.data.setup);
    say("login-message", "Scan the code, then sign in again with a code.");
    return;
  }
  if (answer.ok) {
    enrolment.hidden = true;
    loginForm.reset();
    await enter();
    return;
  }
  say("login-message", reason(answer, "Sign in failed."));
});

document.getElementById("sign-out").addEventListener("click", async () => {
  await api("/api/logout", { method: "POST" });
  closeDocument();
  show("login");
});

/// Signed in: draw the library and go wherever the URL says.
async function enter() {
  show("library");
  await refresh();
  route();
}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

const grid = document.getElementById("grid");
const hits = document.getElementById("hits");
const search = document.getElementById("search");

let documents = [];

/// Fetch the library and draw the cards.
async function refresh() {
  const answer = await get("/api/docs");
  if (answer.status === 401) {
    show("login");
    return false;
  }
  if (!answer.ok) {
    say("library-message", reason(answer, "evo could not read the library."));
    return false;
  }
  documents = answer.data.documents || [];
  drawGrid();
  return true;
}

function drawGrid() {
  grid.replaceChildren();
  if (documents.length === 0) {
    say("library-message", "Nothing here yet. Add a PDF.");
    return;
  }
  say("library-message", "");
  for (const doc of documents) {
    grid.append(card(doc));
  }
}

/// One library card. Everything on it is `textContent`: a document's title is
/// whatever somebody uploaded, and it is never markup.
function card(doc) {
  const article = document.createElement("article");
  article.className = "card doc";
  article.dataset.id = doc.id;

  const thumb = document.createElement("img");
  thumb.className = "thumb";
  thumb.loading = "lazy";
  thumb.alt = "";
  thumb.src = thumbUrl(doc.id);
  article.append(thumb);

  const title = document.createElement("h2");
  title.textContent = doc.title;
  article.append(title);

  const pages = document.createElement("p");
  pages.className = "meta";
  pages.textContent = doc.pages === 1 ? "1 page" : `${doc.pages} pages`;
  article.append(pages);

  if (doc.tags && doc.tags.length > 0) {
    const tags = document.createElement("p");
    tags.className = "tags";
    for (const name of doc.tags) {
      const tag = document.createElement("span");
      tag.className = "tag";
      tag.textContent = name;
      tags.append(tag);
    }
    article.append(tags);
  }

  if (doc.summary) {
    const summary = document.createElement("p");
    summary.className = "summary";
    summary.textContent = doc.summary;
    article.append(summary);
  }

  article.addEventListener("click", () => {
    // A long press has already done something; the release must not also
    // open the document.
    if (article.dataset.pressed === "handled") {
      delete article.dataset.pressed;
      return;
    }
    location.hash = `#doc/${doc.id}/1`;
  });
  holdToDelete(article, doc);
  return article;
}

/// Press and hold to delete. A phone has no right-click and no Delete key, so
/// the gesture is the menu -- and it asks before it does anything.
function holdToDelete(article, doc) {
  let timer = null;
  let start = null;

  const cancel = () => {
    clearTimeout(timer);
    timer = null;
  };

  article.addEventListener("pointerdown", (event) => {
    start = { x: event.clientX, y: event.clientY };
    timer = setTimeout(async () => {
      article.dataset.pressed = "handled";
      if (navigator.vibrate) navigator.vibrate(15);
      if (!confirm(`Delete “${doc.title}”? This cannot be undone.`)) return;
      const answer = await api(`/api/docs/${doc.id}`, { method: "DELETE" });
      if (!answer.ok) {
        say("library-message", reason(answer, "evo could not delete that."));
        return;
      }
      await refresh();
    }, 600);
  });

  // A press that turns into a scroll is a scroll.
  article.addEventListener("pointermove", (event) => {
    if (!timer || !start) return;
    if (Math.hypot(event.clientX - start.x, event.clientY - start.y) > 10) cancel();
  });
  for (const done of ["pointerup", "pointercancel", "pointerleave"]) {
    article.addEventListener(done, cancel);
  }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

let searching = null;

search.addEventListener("input", () => {
  // Typing is faster than searching; only the pause at the end is a question.
  clearTimeout(searching);
  searching = setTimeout(runSearch, 250);
});

async function runSearch() {
  const query = search.value.trim();
  if (query === "") {
    hits.hidden = true;
    grid.hidden = false;
    return;
  }
  const answer = await get(`/api/docs?q=${encodeURIComponent(query)}`);
  if (!answer.ok) {
    say("library-message", reason(answer, "evo could not search the library."));
    return;
  }
  drawHits(answer.data.matches || []);
}

/// Search results are deep links: a hit knows its page, so tapping one opens
/// the document at that page rather than at the beginning.
function drawHits(matches) {
  hits.replaceChildren();
  grid.hidden = true;
  hits.hidden = false;
  if (matches.length === 0) {
    const empty = document.createElement("li");
    empty.className = "hit";
    empty.textContent = "Nothing in the library says that.";
    hits.append(empty);
    return;
  }
  for (const match of matches) {
    const item = document.createElement("li");
    item.className = "hit";

    const where = document.createElement("p");
    where.className = "hit-where";
    where.textContent = match.from_summary
      ? `${match.title} — summary`
      : `${match.title} — page ${match.page}`;
    item.append(where);

    const snippet = document.createElement("p");
    snippet.className = "hit-snippet";
    snippet.textContent = match.snippet;
    item.append(snippet);

    item.addEventListener("click", () => {
      location.hash = `#doc/${match.doc_id}/${Math.max(1, match.page)}`;
    });
    hits.append(item);
  }
}

// ---------------------------------------------------------------------------
// Uploading
// ---------------------------------------------------------------------------

document.getElementById("file").addEventListener("change", async (event) => {
  const files = [...event.target.files];
  event.target.value = "";
  for (const file of files) {
    say("library-message", `Adding ${file.name}…`);
    const answer = await api("/api/docs", {
      method: "POST",
      headers: {
        "content-type": "application/pdf",
        // The body is the PDF, so the name travels beside it.
        "X-Evo-Filename": asciiHeader(file.name),
        "X-Evo-Title": asciiHeader(file.name.replace(/\.pdf$/i, "")),
      },
      body: file,
    });
    if (!answer.ok) {
      say("library-message", reason(answer, `evo would not take ${file.name}.`));
      return;
    }
    if (answer.data.duplicate) {
      say("library-message", `${file.name} was already in the library.`);
    }
  }
  await refresh();
});

/// Header values are bytes, not text: anything that is not plain ASCII is
/// dropped rather than mangled, and the title survives as far as it can.
function asciiHeader(text) {
  return (
    [...text].filter((c) => c >= " " && c <= "~").join("").slice(0, 180) ||
    "document.pdf"
  );
}

// ---------------------------------------------------------------------------
// The router
// ---------------------------------------------------------------------------

document.getElementById("back").addEventListener("click", () => {
  location.hash = "#library";
});

async function route() {
  const parts = location.hash.replace(/^#/, "").split("/").filter(Boolean);
  if (parts[0] === "doc" && isDocId(parts[1])) {
    const page = Math.max(1, parseInt(parts[2], 10) || 1);
    show("doc");
    const opened = await openDocument(parts[1], page);
    if (!opened) location.hash = "#library";
    return;
  }
  closeDocument();
  show("library");
}

window.addEventListener("hashchange", route);

// ---------------------------------------------------------------------------
// Starting up
// ---------------------------------------------------------------------------

/// Is there a session? There is no endpoint that says so, and there does not
/// need to be: asking for something behind the guard answers the question.
async function boot() {
  const answer = await get("/api/status");
  if (answer.status === 401 || answer.status === 0) {
    show("login");
    if (answer.status === 0) say("login-message", "evo is not answering.");
    return;
  }
  await enter();
}

// The service worker makes the shell open without the network. It is an
// improvement, never a requirement: a browser that refuses it (plain HTTP on
// a LAN address, say) gets the same app, only slower to start.
if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  });
}

boot();
