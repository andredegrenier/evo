// Talking to evo. Every other module goes through this one, so there is a
// single place that knows about the CSRF header and about what a refusal
// looks like.

/// The header the server insists on for anything that changes something. Its
/// value does not matter; that a form on another site cannot set a header at
/// all is the point.
const CSRF = { "X-Evo": "1" };

/// A request, and its answer taken apart.
///
/// Never throws for an HTTP status: a 401 and a 409 are both answers this app
/// has something to do about, and only a dropped connection is exceptional.
export async function api(path, options = {}) {
  const headers = { ...CSRF, ...(options.headers || {}) };
  let response;
  try {
    response = await fetch(path, { ...options, headers });
  } catch {
    return { status: 0, data: {}, headers: new Headers(), ok: false };
  }
  let data = {};
  const type = response.headers.get("content-type") || "";
  if (type.includes("json")) {
    try {
      data = await response.json();
    } catch {
      // A body-less error is still an error; the status carries it.
    }
  }
  return {
    status: response.status,
    ok: response.ok,
    data,
    headers: response.headers,
  };
}

export function get(path) {
  return api(path);
}

export function postJson(path, body) {
  return api(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

/// What the server says went wrong, or something true when it said nothing.
export function reason(answer, fallback) {
  if (answer.status === 0) return "evo is not answering. Check the connection.";
  return answer.data.error || fallback;
}

/// Ids are digests, and a URL built from anything else is a mistake worth
/// catching in the app rather than arguing about with the server.
export function isDocId(id) {
  return typeof id === "string" && /^[0-9a-f]{64}$/.test(id);
}

/// How many device pixels evo should render a page at.
///
/// Three buckets, because every distinct scale is another PNG on the server's
/// disk and a phone cannot tell 2.6 from 3 at arm's length.
export function scaleFor(zoom = 1) {
  const wanted = (window.devicePixelRatio || 1) * zoom;
  return Math.min(3, Math.max(1, Math.ceil(wanted)));
}

export const pageUrl = (id, page, scale) =>
  `/api/docs/${id}/page/${page}.png?scale=${scale}`;
export const overlayUrl = (id, page) => `/api/docs/${id}/markup.svg?page=${page}`;
export const thumbUrl = (id) => `/api/docs/${id}/thumb.png`;
