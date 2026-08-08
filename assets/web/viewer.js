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

import { get, reason, pageUrl, overlayUrl, scaleFor } from "./api.js";

const container = document.getElementById("pages");
const titleEl = document.getElementById("doc-title");
const indicator = document.getElementById("page-indicator");

/// Zooming past this stops being reading and starts being pixels.
const MAX_ZOOM = 6;
/// How many pages either side of the one on screen are fetched ahead.
const PREFETCH = 1;
/// And how far away a page has to be before its picture is let go of again.
const FORGET = 4;

let open = null;

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
  };
  titleEl.textContent = manifest.title || "";
  message("");
  build();
  goTo(page);
  return true;
}

/// Leave the document. The pages go with it: a phone should not be holding
/// twenty page images of something nobody is looking at.
export function closeDocument() {
  if (open && open.observer) open.observer.disconnect();
  open = null;
  container.replaceChildren();
  titleEl.textContent = "";
  indicator.textContent = "";
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

  // The overlay is markup, which changes, so it is fetched rather than cached
  // -- and injected as an element so it scales with the page instead of
  // sitting in an <img> at a fixed size.
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

  const apply = () => {
    const { scale, x, y } = element.zoom;
    stage.style.transform =
      scale === 1 ? "" : `translate(${x}px, ${y}px) scale(${scale})`;
    const zoomed = scale > 1;
    element.classList.toggle("zoomed", zoomed);
    // While a page is zoomed it owns every gesture on it; otherwise the
    // sideways swipe belongs to the scroller.
    element.style.touchAction = zoomed ? "none" : "pan-x";
    container.classList.toggle("locked", zoomed);
  };

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
