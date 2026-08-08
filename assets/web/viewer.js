// The viewer: one document, one page per screen, swipe sideways.
//
// A page is two things laid over each other in the same box -- a PNG of the
// page and an SVG of the markup on it. The SVG has the page's own viewBox, so
// the browser scales it with the picture and a highlight stays where it was
// drawn at any zoom, with nothing re-fetched.
//
// Pinch is ours rather than the browser's: the pages are `touch-action: pan-x`
// so a sideways swipe is a native scroll (and scroll-snap does the paging for
// free) while two fingers come through as pointer events. What a pinch changes
// is a CSS transform, which is instant; when it settles, a sharper PNG is
// asked for at the scale the reader ended up at.
//
// Markup is the same gesture with a tool switched on. A drag becomes a
// rectangle in CSS pixels, which becomes a rectangle in PDF points -- the
// coordinates evo has used since the desktop app's first highlight, counted up
// from the bottom of the page -- and that is what is saved. Nothing about the
// zoom, the scale bucket or the screen ends up in the document.

import { api, get, reason, pageUrl, overlayUrl, scaleFor } from "./api.js";

const container = document.getElementById("pages");
const titleEl = document.getElementById("doc-title");
const indicator = document.getElementById("page-indicator");

/// Zooming past this stops being reading and starts being pixels.
const MAX_ZOOM = 6;
/// How many pages either side of the one on screen are fetched ahead.
const PREFETCH = 1;
/// And how far away a page has to be before its picture is let go of again.
const FORGET = 4;

/// A drag shorter than this, in PDF points, was a tap: nobody means to draw a
/// two-point highlight, and picking something already on the page is the more
/// useful reading of it.
const TAP = 5;
/// The smallest note worth drawing text into, in points. A note dragged out
/// smaller is grown rather than refused -- the box is a container, not the
/// thing the reader was aiming at.
const MIN_NOTE = { width: 96, height: 32 };
/// Note text, in points. About the size of the body text on a letter page.
const NOTE_FONT = 11;

/// The colours markup is made in. Straight RGBA, 0-255, exactly as
/// `doc::annotation::Color` is serialized -- these travel into the sidecar and
/// come back out in the desktop app, so they are the app's colours and not the
/// stylesheet's.
const CLEAR = { r: 0, g: 0, b: 0, a: 0 };
const HIGHLIGHTER = { r: 250, g: 220, b: 50, a: 255 };
const NOTE_PAPER = { r: 255, g: 245, b: 180, a: 255 };
const NOTE_INK = { r: 30, g: 30, b: 46, a: 255 };

let open = null;
/// Which tool is on: `null`, `"highlight"` or `"note"`. Module-level because
/// the pinch handlers have to stand aside while one is.
let tool = null;
/// The annotation the reader has picked, if any: `{ page, id }`.
let selected = null;

/// Open `id` at `page`. Returns false if the document could not be opened, so
/// the router can go back to the library.
export async function openDocument(id, page) {
  if (open && open.id === id) {
    goTo(page);
    return true;
  }

  const answer = await get(`/api/docs/${id}/manifest`);
  if (!answer.ok) {
    message(reason(answer, "evo could not open that document."));
    return false;
  }
  const manifest = answer.data;
  open = {
    id,
    pages: manifest.pages || [],
    sections: [],
    current: 1,
    observer: null,
    // What is drawn on the document, as the server has it. Kept so a tap can
    // be answered without a round trip; every *write* re-reads it first.
    markup: { version: 1, annotations: [] },
  };
  titleEl.textContent = manifest.title || "";
  message("");
  build();
  goTo(page);
  await readMarkup();
  return true;
}

/// Leave the document. The pages go with it: a phone should not be holding
/// twenty page images of something nobody is looking at.
export function closeDocument() {
  if (open && open.observer) open.observer.disconnect();
  open = null;
  setTool(null);
  container.replaceChildren();
  titleEl.textContent = "";
  indicator.textContent = "";
}

/// Turn to a page. What a citation in an answer does: chat has a page number
/// and no idea how a page is drawn, which is the right way round.
export function showPage(number) {
  if (open) goTo(number);
}

function message(text) {
  document.getElementById("doc-message").textContent = text || "";
}

// ---------------------------------------------------------------------------
// Laying the pages out
// ---------------------------------------------------------------------------

function build() {
  container.classList.remove("locked");
  container.replaceChildren();
  open.sections = open.pages.map((size, index) => section(size, index + 1));
  container.append(...open.sections);

  // Which page is on screen decides what is worth fetching. The scroller is
  // the root, so this asks the browser rather than watching every scroll.
  open.observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        const number = Number(entry.target.dataset.page);
        if (number !== open.current) settleOn(number);
      }
    },
    { root: container, threshold: 0.55 },
  );
  for (const element of open.sections) open.observer.observe(element);
}

/// One page: a box of the page's own proportions, with the picture and the
/// markup inside it.
function section(size, number) {
  const element = document.createElement("section");
  element.className = "page";
  element.dataset.page = String(number);

  const stage = document.createElement("div");
  stage.className = "stage";
  stage.style.aspectRatio = `${size.width} / ${size.height}`;

  const raster = document.createElement("img");
  raster.className = "raster";
  raster.alt = `Page ${number}`;
  raster.decoding = "async";
  stage.append(raster);

  const overlay = document.createElement("div");
  overlay.className = "overlay";
  stage.append(overlay);

  element.append(stage);
  element.zoom = { scale: 1, x: 0, y: 0 };
  pinchable(element, stage);
  drawable(element, stage, number);
  return element;
}

/// Fetch the picture and the markup for one page, once.
async function load(number) {
  const element = open.sections[number - 1];
  if (!element || element.dataset.loaded === "yes") return;
  element.dataset.loaded = "yes";

  const raster = element.querySelector(".raster");
  raster.src = pageUrl(open.id, number, scaleFor(element.zoom.scale));
  raster.dataset.scale = String(scaleFor(element.zoom.scale));
  await drawOverlay(number);
}

/// The markup of one page, as an SVG laid over the picture.
///
/// Injected as an element rather than left in an `<img>` so it scales with the
/// page; the server sends it `no-cache` with a version tag, so asking again
/// after a change is a revalidation and gets the new one.
async function drawOverlay(number) {
  const element = open && open.sections[number - 1];
  if (!element) return;
  const overlay = element.querySelector(".overlay");
  try {
    const response = await fetch(overlayUrl(open.id, number), {
      headers: { "X-Evo": "1" },
    });
    if (response.ok) overlay.innerHTML = await response.text();
  } catch {
    // No overlay is a page without markup, which is a page.
  }
}

/// Let go of a picture that is far away. The element stays, so its size and
/// its place in the scroll never change.
function forget(number) {
  const element = open.sections[number - 1];
  if (!element || element.dataset.loaded !== "yes") return;
  delete element.dataset.loaded;
  element.querySelector(".raster").removeAttribute("src");
  element.querySelector(".overlay").replaceChildren();
}

/// The page the reader is on: say so, fetch its neighbours, and put it in the
/// URL so back and bookmarks both work.
function settleOn(number) {
  open.current = number;
  indicator.textContent = `${number} / ${open.sections.length}`;
  history.replaceState(null, "", `#doc/${open.id}/${number}`);

  for (let n = number - PREFETCH; n <= number + PREFETCH; n += 1) {
    if (n >= 1 && n <= open.sections.length) load(n);
  }
  for (let n = 1; n <= open.sections.length; n += 1) {
    if (Math.abs(n - number) > FORGET) forget(n);
  }
}

function goTo(number) {
  const page = Math.min(Math.max(1, number), open.sections.length);
  const element = open.sections[page - 1];
  if (!element) return;
  settleOn(page);
  // `auto` rather than `smooth`: this is arriving at a page, not travelling
  // to one, and a deep link should not scroll through the whole document.
  element.scrollIntoView({ behavior: "auto", inline: "center" });
}

// ---------------------------------------------------------------------------
// Pinch and pan
// ---------------------------------------------------------------------------

/// Two fingers zoom, one finger pans what is zoomed, two taps put it back.
function pinchable(element, stage) {
  const points = new Map();
  let gesture = null;
  let lastTap = 0;
  /// Whether anything since the last quiet moment was a pinch or a drag.
  /// Lifting two fingers off a pinch is two `pointerup`s in a row, which is
  /// exactly what a double tap looks like -- so a gesture has to say it was
  /// one, or every pinch would undo itself.
  let gestured = false;

  /// While a tool is on, every gesture on the page belongs to the tool.
  const busy = () => {
    if (!tool) return false;
    points.clear();
    gesture = null;
    return true;
  };

  const apply = () => {
    const { scale, x, y } = element.zoom;
    stage.style.transform =
      scale === 1 ? "" : `translate(${x}px, ${y}px) scale(${scale})`;
    const zoomed = scale > 1;
    element.classList.toggle("zoomed", zoomed);
    // While a page is zoomed it owns every gesture on it; otherwise the
    // sideways swipe belongs to the scroller. A tool is the same claim: a
    // drag across the page is a highlight, not a page turn.
    element.style.touchAction = zoomed || tool ? "none" : "pan-x";
    container.classList.toggle("locked", zoomed);
  };
  element.applyZoom = apply;

  /// Keep the picture from being dragged off the screen.
  ///
  /// The stage sits centred in the page and is scaled about its own top-left
  /// corner, so a page zoomed in may be moved until one of its edges reaches
  /// the corresponding edge of the screen, and one that still fits is centred
  /// rather than left wherever the fingers put it.
  const clamp = () => {
    const zoom = element.zoom;
    if (zoom.scale === 1) {
      zoom.x = 0;
      zoom.y = 0;
      return;
    }
    const axis = (offset, size, frame) => {
      const rest = (frame - size) / 2;
      const scaled = size * zoom.scale;
      if (scaled <= frame) return -rest + (frame - scaled) / 2;
      return Math.min(-rest, Math.max(frame - scaled - rest, offset));
    };
    zoom.x = axis(zoom.x, stage.offsetWidth, element.clientWidth);
    zoom.y = axis(zoom.y, stage.offsetHeight, element.clientHeight);
  };

  element.addEventListener("pointerdown", (event) => {
    if (busy()) return;
    element.setPointerCapture(event.pointerId);
    points.set(event.pointerId, { x: event.clientX, y: event.clientY });

    if (points.size === 2) {
      gestured = true;
      const [a, b] = [...points.values()];
      gesture = {
        distance: Math.hypot(a.x - b.x, a.y - b.y) || 1,
        midpoint: middle(a, b, element),
        start: { ...element.zoom },
      };
    } else if (points.size === 1 && element.zoom.scale > 1) {
      gesture = { pan: { x: event.clientX, y: event.clientY }, start: { ...element.zoom } };
    }
  });

  element.addEventListener("pointermove", (event) => {
    if (busy()) return;
    if (!points.has(event.pointerId)) return;
    points.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (!gesture) return;

    if (points.size >= 2 && gesture.distance) {
      const [a, b] = [...points.values()];
      const distance = Math.hypot(a.x - b.x, a.y - b.y) || 1;
      const scale = clampScale(gesture.start.scale * (distance / gesture.distance));
      // The point between the fingers is the one that must not move: the
      // transform origin is the stage's corner, so the offset moves with it.
      const growth = scale / gesture.start.scale;
      element.zoom = {
        scale,
        x: gesture.midpoint.x - (gesture.midpoint.x - gesture.start.x) * growth,
        y: gesture.midpoint.y - (gesture.midpoint.y - gesture.start.y) * growth,
      };
      clamp();
      apply();
      event.preventDefault();
    } else if (gesture.pan) {
      const dx = event.clientX - gesture.pan.x;
      const dy = event.clientY - gesture.pan.y;
      if (Math.hypot(dx, dy) > 4) gestured = true;
      element.zoom = {
        scale: gesture.start.scale,
        x: gesture.start.x + dx,
        y: gesture.start.y + dy,
      };
      clamp();
      apply();
      event.preventDefault();
    }
  });

  const release = (event) => {
    if (busy()) return;
    points.delete(event.pointerId);
    if (points.size === 0) {
      gesture = null;
      settleZoom(element);
    } else if (points.size === 1) {
      // A finger lifted mid-pinch turns it into a pan from where it is.
      const [only] = [...points.values()];
      gesture = { pan: { x: only.x, y: only.y }, start: { ...element.zoom } };
    }
  };
  for (const done of ["pointerup", "pointercancel"]) {
    element.addEventListener(done, release);
  }

  // Two quick taps: back to the whole page, or in to a comfortable reading
  // size, with the spot that was tapped staying where it was. The gesture
  // everybody already knows.
  element.addEventListener("pointerup", (event) => {
    if (tool) return;
    // A pinch or a drag is not a tap, however it ended.
    if (gestured) {
      lastTap = 0;
      if (points.size === 0) gestured = false;
      return;
    }
    const now = Date.now();
    if (now - lastTap < 300) {
      if (element.zoom.scale > 1) {
        element.zoom = { scale: 1, x: 0, y: 0 };
      } else {
        const box = element.getBoundingClientRect();
        const rest = (element.clientWidth - stage.offsetWidth) / 2;
        const restY = (element.clientHeight - stage.offsetHeight) / 2;
        element.zoom = {
          scale: 2,
          x: -(event.clientX - box.left - rest),
          y: -(event.clientY - box.top - restY),
        };
      }
      clamp();
      apply();
      settleZoom(element);
      lastTap = 0;
      return;
    }
    lastTap = now;
  });
}

function clampScale(scale) {
  return Math.min(MAX_ZOOM, Math.max(1, scale));
}

/// Where the fingers are, in the element's own coordinates.
function middle(a, b, element) {
  const box = element.getBoundingClientRect();
  return { x: (a.x + b.x) / 2 - box.left, y: (a.y + b.y) / 2 - box.top };
}

/// The gesture is over. If the reader ended up closer than the picture was
/// drawn for, ask for it again at the scale they are actually looking at --
/// and only swap it in once it has arrived, so nothing flickers.
function settleZoom(element) {
  if (!open) return;
  const number = Number(element.dataset.page);
  const raster = element.querySelector(".raster");
  const wanted = scaleFor(element.zoom.scale);
  if (!raster.src || Number(raster.dataset.scale) >= wanted) return;

  const sharper = new Image();
  sharper.decoding = "async";
  sharper.onload = () => {
    raster.src = sharper.src;
    raster.dataset.scale = String(wanted);
  };
  sharper.src = pageUrl(open.id, number, wanted);
}

// ---------------------------------------------------------------------------
// Where a finger is, in the document's own coordinates
// ---------------------------------------------------------------------------

/// A point on the screen, in PDF points on `number`.
///
/// The stage's box on screen is the page, whatever the pinch has done to it --
/// `getBoundingClientRect` reports the transformed box -- so the number of CSS
/// pixels per point falls straight out of it. PDF counts up from the bottom of
/// the page and screens count down from the top, which is the flip.
function pointOn(stage, number, event) {
  const size = open.pages[number - 1];
  const box = stage.getBoundingClientRect();
  const scaleX = box.width / size.width || 1;
  const scaleY = box.height / size.height || 1;
  const clamp = (value, limit) => Math.min(Math.max(value, 0), limit);
  return {
    x: clamp((event.clientX - box.left) / scaleX, size.width),
    y: clamp(size.height - (event.clientY - box.top) / scaleY, size.height),
  };
}

/// Two corners in any order, as the rectangle they bound.
function rectangle(from, to) {
  return {
    min: { x: Math.min(from.x, to.x), y: Math.min(from.y, to.y) },
    max: { x: Math.max(from.x, to.x), y: Math.max(from.y, to.y) },
  };
}

/// Where a rectangle sits in its page, as percentages -- so a box drawn over
/// the picture stays lined up through a pinch without being recomputed.
function place(element, number, rect) {
  const size = open.pages[number - 1];
  element.style.left = `${(rect.min.x / size.width) * 100}%`;
  element.style.width = `${((rect.max.x - rect.min.x) / size.width) * 100}%`;
  // The top edge of the box is its *higher* y, counted down from the top.
  element.style.top = `${((size.height - rect.max.y) / size.height) * 100}%`;
  element.style.height = `${((rect.max.y - rect.min.y) / size.height) * 100}%`;
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

const tools = {
  highlight: document.getElementById("tool-highlight"),
  note: document.getElementById("tool-note"),
};
const deleteButton = document.getElementById("delete-annotation");

for (const [name, button] of Object.entries(tools)) {
  button.addEventListener("click", () => setTool(tool === name ? null : name));
}

/// Turn a tool on, off, or over to the other one.
function setTool(next) {
  tool = next;
  for (const [name, button] of Object.entries(tools)) {
    button.setAttribute("aria-pressed", String(tool === name));
  }
  unpick();
  if (open) {
    for (const element of open.sections) {
      if (element.applyZoom) element.applyZoom();
    }
  }
  message(
    tool === "highlight"
      ? "Drag across what you want highlighted. Tap a mark to remove it."
      : tool === "note"
        ? "Drag a box for the note. Tap a mark to remove it."
        : "",
  );
}

/// A drag on this page draws; a tap picks. Only while a tool is on -- with
/// none, every listener here stands down and the page is for reading.
function drawable(element, stage, number) {
  let drag = null;

  const finish = () => {
    if (drag && drag.preview) drag.preview.remove();
    drag = null;
  };

  element.addEventListener("pointerdown", (event) => {
    if (!tool || !event.isPrimary || drag) return;
    element.setPointerCapture(event.pointerId);
    const preview = document.createElement("div");
    preview.className = tool === "note" ? "draft note" : "draft";
    stage.append(preview);
    drag = { pointer: event.pointerId, from: pointOn(stage, number, event), preview };
    place(preview, number, rectangle(drag.from, drag.from));
    event.preventDefault();
  });

  element.addEventListener("pointermove", (event) => {
    if (!drag || event.pointerId !== drag.pointer) return;
    place(drag.preview, number, rectangle(drag.from, pointOn(stage, number, event)));
    event.preventDefault();
  });

  element.addEventListener("pointercancel", (event) => {
    if (drag && event.pointerId === drag.pointer) finish();
  });

  element.addEventListener("pointerup", async (event) => {
    if (!drag || event.pointerId !== drag.pointer) return;
    const from = drag.from;
    const drawn = tool;
    const to = pointOn(stage, number, event);
    finish();

    const rect = rectangle(from, to);
    // A drag that went nowhere is a tap, and a tap asks about what is already
    // there rather than adding something nobody can see.
    if (rect.max.x - rect.min.x < TAP && rect.max.y - rect.min.y < TAP) {
      pick(number, to);
      return;
    }
    unpick();
    if (drawn === "highlight") await addHighlight(number, rect);
    else await addNote(number, rect);
  });
}

/// The next id nobody is using.
///
/// The desktop app hands out `max(id) + 1` when it reloads a sidecar
/// (`AnnotationStore::restore`), so this has to agree with it or the two would
/// eventually give two annotations the same number. It is worked out from the
/// markup the server has just sent, never from a copy held while the reader
/// was thinking.
function nextId(annotations) {
  return annotations.reduce((highest, a) => Math.max(highest, a.id || 0), 0) + 1;
}

async function addHighlight(number, rect) {
  const failure = await save((annotations) => [
    ...annotations,
    {
      id: nextId(annotations),
      page: number - 1,
      kind: "Highlight",
      rect,
      style: {
        stroke: CLEAR,
        stroke_width: 0,
        fill: HIGHLIGHTER,
        opacity: 0.35,
      },
    },
  ]);
  await settle(number, failure);
}

async function addNote(number, rect) {
  const text = await askForNote();
  if (text === null) return;

  // A note is a container for words: one dragged too small to hold any is
  // grown from its top-left corner -- where the text starts -- rather than
  // turned down, and kept on the page.
  const size = open.pages[number - 1];
  const width = Math.min(size.width, Math.max(rect.max.x - rect.min.x, MIN_NOTE.width));
  const height = Math.min(size.height, Math.max(rect.max.y - rect.min.y, MIN_NOTE.height));
  const left = Math.min(rect.min.x, size.width - width);
  const top = Math.max(rect.max.y, height);
  const box = {
    min: { x: left, y: top - height },
    max: { x: left + width, y: top },
  };

  const failure = await save((annotations) => [
    ...annotations,
    {
      id: nextId(annotations),
      page: number - 1,
      kind: { TextBox: { text, font_size: NOTE_FONT, align: "Left" } },
      rect: box,
      // The stroke colour is the ink: `write_annotation` draws TextBox text in
      // it and fills the box with `fill`.
      style: {
        stroke: NOTE_INK,
        stroke_width: 0,
        fill: NOTE_PAPER,
        opacity: 0.95,
      },
    },
  ]);
  await settle(number, failure);
}

/// Say what went wrong, or redraw the page it went right on.
async function settle(number, failure) {
  if (failure) {
    message(failure);
    return;
  }
  message("");
  await drawOverlay(number);
}

// ---------------------------------------------------------------------------
// Picking something already there
// ---------------------------------------------------------------------------

/// What the reader tapped, if anything: the topmost annotation on the page
/// whose rectangle holds that point.
///
/// The hit test is ours rather than the SVG's because the overlay is not
/// interactive -- it sits under `pointer-events: none` so a pinch on a
/// highlight is still a pinch -- and because the rectangles are already here.
function pick(number, point) {
  unpick();
  const on = (open.markup.annotations || []).filter((a) => a.page === number - 1);
  const hit = [...on].reverse().find((a) => holds(a.rect, point));
  if (!hit) return;

  selected = { page: number, id: hit.id };
  const element = open.sections[number - 1];
  const outline = document.createElement("div");
  outline.className = "selection";
  place(outline, number, hit.rect);
  element.querySelector(".stage").append(outline);
  deleteButton.hidden = false;
}

function holds(rect, point) {
  return (
    point.x >= rect.min.x &&
    point.x <= rect.max.x &&
    point.y >= rect.min.y &&
    point.y <= rect.max.y
  );
}

function unpick() {
  selected = null;
  deleteButton.hidden = true;
  for (const outline of container.querySelectorAll(".selection")) outline.remove();
}

deleteButton.addEventListener("click", async () => {
  if (!selected) return;
  const { page, id } = selected;
  unpick();
  const failure = await save((annotations) => annotations.filter((a) => a.id !== id));
  await settle(page, failure);
});

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

/// Read the markup, change it, and write it back if nobody else did first.
///
/// `change` is given the annotations the server has *now* and returns what they
/// should become; it is called again on a conflict, so it must not depend on
/// anything worked out earlier -- which is why ids are allocated inside it.
/// One retry: a second conflict is two writers, not a stale read, and the
/// reader should hear about it rather than watch evo loop.
///
/// Returns a sentence on failure and nothing at all on success.
async function save(change) {
  const url = `/api/docs/${open.id}/markup`;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const current = await get(url);
    if (!current.ok) {
      return reason(current, "evo could not read this document's markup.");
    }
    const markup = current.data;
    const annotations = change(markup.annotations || []);
    const answer = await api(url, {
      method: "PUT",
      headers: {
        "content-type": "application/json",
        // What was just read. The server refuses the write if the markup has
        // moved on since, which is the whole reason this is a round trip.
        "If-Match": current.headers.get("etag") || "*",
      },
      body: JSON.stringify({ version: markup.version, annotations }),
    });
    if (answer.ok) {
      open.markup = { ...markup, annotations };
      return null;
    }
    if (answer.status !== 409) {
      return reason(answer, "evo could not save that markup.");
    }
  }
  return "Somebody else is changing this document's markup. Try that again.";
}

/// The markup as the server has it, for hit-testing. Failing is not worth a
/// message: the page is still readable and the next write re-reads it anyway.
async function readMarkup() {
  const answer = await get(`/api/docs/${open.id}/markup`);
  if (answer.ok) open.markup = answer.data;
}

// ---------------------------------------------------------------------------
// The note sheet
// ---------------------------------------------------------------------------

const noteSheet = document.getElementById("note-sheet");
const noteForm = document.getElementById("note-form");
const noteText = document.getElementById("note-text");
let askingForNote = null;

/// What the note should say, or `null` if the reader thought better of it.
function askForNote() {
  noteText.value = "";
  noteSheet.hidden = false;
  noteText.focus();
  return new Promise((resolve) => {
    askingForNote = resolve;
  });
}

function answerNote(text) {
  noteSheet.hidden = true;
  const resolve = askingForNote;
  askingForNote = null;
  if (resolve) resolve(text);
}

noteForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = noteText.value.trim();
  // An empty note is a rectangle with nothing in it; that is a cancellation.
  answerNote(text === "" ? null : text);
});

document
  .getElementById("note-cancel")
  .addEventListener("click", () => answerNote(null));
