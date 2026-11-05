// The web UI's page (OWL-467): the login, the header and its live stream,
// the project and theme switches. Every text reaches the page through
// `text`, which sets `textContent`: nothing an agent or a comment wrote is
// ever read as markup. The only secret this page sends is the session, in
// the `X-Owlshift-Session` header; the login sends the one-time code.
"use strict";

const SESSION_KEY = "owlshift.session";
const THEME_KEY = "owlshift.theme";
const PROJECT_KEY = "owlshift.project";
const SESSION_HEADER = "X-Owlshift-Session";

function text(el, value) {
  el.textContent = value === null || value === undefined ? "" : String(value);
  return el;
}

function make(tag, className, value) {
  const el = document.createElement(tag);
  if (className) {
    el.className = className;
  }
  if (value !== undefined) {
    text(el, value);
  }
  return el;
}

function clear(el) {
  while (el.firstChild) {
    el.removeChild(el.firstChild);
  }
}

function stored(key) {
  try {
    return window.localStorage.getItem(key);
  } catch (_) {
    return null;
  }
}

function store(key, value) {
  try {
    if (value === null) {
      window.localStorage.removeItem(key);
    } else {
      window.localStorage.setItem(key, value);
    }
  } catch (_) {
    // A browser that keeps nothing: the page works for this visit.
  }
}

// The theme switch: system, light or dark, kept in the origin's storage.
function applyTheme(choice) {
  if (choice === "light" || choice === "dark") {
    document.documentElement.setAttribute("data-theme", choice);
  } else {
    document.documentElement.removeAttribute("data-theme");
  }
}

function session() {
  const raw = stored(SESSION_KEY);
  if (!raw) {
    return null;
  }
  try {
    const value = JSON.parse(raw);
    if (typeof value.id === "string" && typeof value.secret === "string") {
      return value;
    }
  } catch (_) {
    // Not a session: dropped below.
  }
  store(SESSION_KEY, null);
  return null;
}

// The `X-Owlshift-Session` value of a session.
function sessionHeader(current) {
  return current.id + "." + current.secret;
}

function show(id, shown) {
  document.getElementById(id).hidden = !shown;
}

// Ends the session on this page. `sent` is the session a refused request
// carried: the stored session is removed only while it is still that one,
// since every tab of the origin shares the storage and a newer tab may have
// logged in and stored its own since.
function loggedOut(sent) {
  const current = session();
  if (current && sessionHeader(current) === sent) {
    store(SESSION_KEY, null);
  }
  clear(document.getElementById("header"));
  show("header", false);
  show("logged-out", true);
}

function problem(message) {
  const el = document.getElementById("problem");
  text(el, message);
  el.hidden = !message;
}

// Trades the one-time code of `#login=<code>` for a session. The fragment
// goes before any request, so no request, history entry or referrer holds
// the code.
async function login() {
  const match = /^#login=([0-9a-f]+\.[0-9]+\.[0-9a-f]+)$/.exec(window.location.hash);
  if (window.location.hash) {
    window.history.replaceState(null, "", window.location.pathname);
  }
  if (!match) {
    return;
  }
  const response = await fetch("/api/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code: match[1] }),
    credentials: "omit",
    cache: "no-store",
  });
  if (!response.ok) {
    problem("This link was used or is too old: run `owlshift ui` again.");
    return;
  }
  const value = await response.json();
  store(SESSION_KEY, JSON.stringify({ id: value.id, secret: value.secret }));
}

// A fetch with the session, answering the response and the session it
// sent, or null; a 401 ends that session on this page.
async function authedFetch(path) {
  const current = session();
  if (!current) {
    loggedOut(null);
    return null;
  }
  const sent = sessionHeader(current);
  const headers = {};
  headers[SESSION_HEADER] = sent;
  const response = await fetch(path, {
    headers: headers,
    credentials: "omit",
    cache: "no-store",
  });
  if (response.status === 401) {
    loggedOut(sent);
    return null;
  }
  return { response: response, sent: sent };
}

function fact(label, value, className) {
  const el = make("span", "fact" + (className ? " " + className : ""));
  el.appendChild(make("span", "fact-label", label));
  el.appendChild(make("span", "fact-value", value));
  return el;
}

function daemonState(daemon) {
  switch (daemon && daemon.state) {
    case "running":
      return ["running, pid " + daemon.pid + ", " + daemon.version, "state-success"];
    case "stopping":
      return ["stopping, asked by " + daemon.by, "state-warning"];
    case "upgrading":
      return ["draining for an upgrade, asked by " + daemon.by, "state-warning"];
    case "not_running":
      return ["not running", "state-danger"];
    default:
      return ["unknown: " + (daemon && daemon.why), "state-warning"];
  }
}

function usageText(usage) {
  if (usage.state === "limited") {
    return usage.harness + " limited" + (usage.until ? " until " + usage.until : "");
  }
  return usage.harness + " " + Math.round(usage.share * 100) + "%, resets " + usage.resets_at;
}

function waitingText(waiting) {
  return waiting.questions + " to answer, " + waiting.reviews + " to review, " +
    waiting.parked + " parked";
}

let lastHeader = null;

function renderProjects(header) {
  const select = document.getElementById("project");
  const chosen = stored(PROJECT_KEY) || "";
  const names = header.projects.map((project) => project.repository);
  const have = Array.from(select.options).slice(1).map((option) => option.value);
  if (names.join("\n") !== have.join("\n")) {
    while (select.options.length > 1) {
      select.remove(1);
    }
    for (const name of names) {
      const option = make("option", "", name);
      option.value = name;
      select.appendChild(option);
    }
  }
  select.value = names.includes(chosen) ? chosen : "";
}

function render(header) {
  lastHeader = header;
  renderProjects(header);
  const chosen = document.getElementById("project").value;
  const el = document.getElementById("header");
  clear(el);
  const [state, stateClass] = daemonState(header.daemon);
  el.appendChild(fact("Daemon", state, stateClass));
  if (header.machine_pause) {
    el.appendChild(fact("Paused", header.machine_pause.why, "state-warning"));
  }
  if (header.slots) {
    el.appendChild(fact("Run slots", header.slots.held + " of " + header.slots.of));
  }
  for (const usage of header.usage) {
    el.appendChild(fact("Usage", usageText(usage),
      usage.state === "limited" ? "state-warning" : ""));
  }
  const shown = header.projects.filter((project) => !chosen || project.repository === chosen);
  const waiting = chosen && shown.length ? shown[0].waiting : header.waiting;
  const count = waiting.questions + waiting.reviews + waiting.parked;
  const badge = make("span", count ? "badge" : "", waitingText(waiting));
  const waits = make("span", "fact");
  waits.appendChild(make("span", "fact-label", "For you"));
  waits.appendChild(badge);
  el.appendChild(waits);
  for (const project of shown) {
    const item = make("span", "fact project");
    item.appendChild(make("span", "fact-value", project.repository));
    item.appendChild(make("span", "fact-label",
      project.slots_held + " slot" + (project.slots_held === 1 ? "" : "s")));
    if (project.paused) {
      item.appendChild(make("span", "state-warning", "paused: " + project.paused.why));
    }
    if (project.quarantined) {
      item.appendChild(make("span", "state-danger", "quarantined: " + project.quarantined));
    }
    el.appendChild(item);
  }
  for (const note of header.notes) {
    el.appendChild(make("span", "fact state-information", note));
  }
  show("logged-out", false);
  show("header", true);
}

// The live header: a streamed fetch read line by line. It reconnects after
// an end, 2 s then doubling to 30 s, but not once a newer tab took its
// place, which would take the stream back and forth between tabs, nor once
// the server says the session ended.
let retry = 2000;

async function follow() {
  let superseded = false;
  let ended = false;
  let sent = null;
  try {
    const answer = await authedFetch("/api/stream");
    if (!answer) {
      return;
    }
    sent = answer.sent;
    const response = answer.response;
    if (!response.ok || !response.body) {
      throw new Error("the stream answered " + response.status);
    }
    problem("");
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffered = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) {
        break;
      }
      buffered += decoder.decode(value, { stream: true });
      let end;
      while ((end = buffered.indexOf("\n")) >= 0) {
        const line = buffered.slice(0, end);
        buffered = buffered.slice(end + 1);
        if (!line) {
          continue;
        }
        const message = JSON.parse(line);
        if (message.header) {
          retry = 2000;
          render(message.header);
        } else if (message.closed === "superseded") {
          superseded = true;
        } else if (message.closed === "logged_out") {
          ended = true;
        }
      }
    }
  } catch (error) {
    problem("The live header stopped: " + error.message);
  }
  if (ended) {
    loggedOut(sent);
    return;
  }
  if (superseded) {
    problem("Live updates went to a newer tab: reload this page to take them back.");
    return;
  }
  window.setTimeout(follow, retry);
  retry = Math.min(retry * 2, 30000);
}

async function start() {
  const theme = document.getElementById("theme");
  theme.value = stored(THEME_KEY) || "system";
  applyTheme(theme.value);
  theme.addEventListener("change", () => {
    store(THEME_KEY, theme.value === "system" ? null : theme.value);
    applyTheme(theme.value);
  });
  const project = document.getElementById("project");
  project.addEventListener("change", () => {
    store(PROJECT_KEY, project.value || null);
    if (lastHeader) {
      render(lastHeader);
    }
  });
  try {
    await login();
  } catch (error) {
    problem("The login failed: " + error.message);
  }
  if (!session()) {
    loggedOut(null);
    return;
  }
  follow();
}

applyTheme(stored(THEME_KEY));
document.addEventListener("DOMContentLoaded", start);
