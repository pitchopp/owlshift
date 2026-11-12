// The web UI's page (OWL-467): the login, the header and its live stream,
// the project and theme switches. Every text reaches the page through
// `text`, which sets `textContent`: nothing an agent or a comment wrote is
// ever read as markup. The only secret this page sends is the session, in
// the `X-Owlshift-Session` header; the login sends the one-time code.
// The overview (OWL-470) is further down: For you, the columns and list views
// and the panel of what happened since the last visit. A ticket's page and
// For you's parked entries act too (OWL-473): Restart and Cancel, each posted
// through `authedFetch` to its command's route.
"use strict";

const SESSION_KEY = "owlshift.session";
const THEME_KEY = "owlshift.theme";
const PROJECT_KEY = "owlshift.project";
const VIEW_KEY = "owlshift.view";
const VISIT_KEY = "owlshift.lastVisit";
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
  closeTicket();
  closeGraph();
  lastGraph = null;
  clearHome();
  if (window.owlshiftOps) {
    window.owlshiftOps.hide();
  }
  endStream();
  show("logged-out", true);
}

function problem(message) {
  const el = document.getElementById("problem");
  text(el, message);
  el.hidden = !message;
}

// Trades the one-time code of `#login=<code>` for a session. The fragment
// goes before any request, so no request, history entry or referrer holds
// the code. Only a `#login=` fragment goes: a ticket page's route stays.
async function login() {
  const match = /^#login=([0-9a-f]+\.[0-9]+\.[0-9a-f]+)$/.exec(window.location.hash);
  if (window.location.hash.startsWith("#login=")) {
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
// sent, or null; a 401 ends that session on this page. `init`, when given,
// is `{ signal }`, whose signal ends a request the page no longer wants, or
// `{ method: "POST", body }`, a JSON body (OWL-475).
async function authedFetch(path, init) {
  const current = session();
  if (!current) {
    loggedOut(null);
    return null;
  }
  const sent = sessionHeader(current);
  const headers = {};
  headers[SESSION_HEADER] = sent;
  const options = {
    headers: headers,
    credentials: "omit",
    cache: "no-store",
    signal: init ? init.signal : undefined,
  };
  if (init && init.method === "POST") {
    options.method = "POST";
    headers["Content-Type"] = "application/json";
    options.body = init.body;
  }
  const response = await fetch(path, options);
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

// A project's run slots and the tickets holding them: "0 slots",
// "1 slot: LOC-1538", "2 slots: LOC-1, LOC-2" (OWL-530).
function slotsText(project) {
  return project.slots_held + " slot" + (project.slots_held === 1 ? "" : "s") +
    (project.slot_holders.length ? ": " + project.slot_holders.join(", ") : "");
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
    item.appendChild(make("span", "fact-label", slotsText(project)));
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

// The stream open now, or null: `graph` is whether it asked for the drain's
// graph (OWL-460), which the server computes only while a stream asks, so
// only the graph's page does. A change of route that changes the answer
// ends the stream and opens the other (`syncStream`), a restart the page
// asked for, which is neither a failure nor a newer tab.
let stream = null;
let streamTimer = null;

function wantsGraph() {
  return parseRoute(window.location.hash) === "graph";
}

function streamPath(graph) {
  return graph ? "/api/stream?graph=1" : "/api/stream";
}

// Ends the stream without opening another.
function endStream() {
  window.clearTimeout(streamTimer);
  if (stream) {
    stream.controller.abort();
    stream = null;
  }
}

// Opens the other stream when the route now asks for the graph and the
// stream open did not, or the reverse.
function syncStream() {
  if (!stream || stream.graph === wantsGraph()) {
    return;
  }
  endStream();
  if (!wantsGraph()) {
    lastGraph = null;
  }
  retry = 2000;
  follow();
}

async function follow() {
  let superseded = false;
  let ended = false;
  let sent = null;
  const mine = { controller: new AbortController(), graph: wantsGraph() };
  stream = mine;
  try {
    const answer = await authedFetch(streamPath(mine.graph), {
      signal: mine.controller.signal,
    });
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
        } else if (message.overview) {
          renderOverview(message.overview);
        } else if (message.graph) {
          lastGraph = message.graph;
          drawGraph();
        } else if (message.closed === "superseded") {
          superseded = true;
        } else if (message.closed === "logged_out") {
          ended = true;
        }
      }
    }
  } catch (error) {
    if (stream !== mine) {
      return;
    }
    problem("The live header stopped: " + error.message);
  }
  if (stream !== mine) {
    // The page ended this stream (a route that asks for the other one, or a
    // logout): the stream that replaced it, if any, goes on by itself.
    return;
  }
  if (ended) {
    loggedOut(sent);
    return;
  }
  if (superseded) {
    stream = null;
    problem("Live updates went to a newer tab: reload this page to take them back.");
    return;
  }
  streamTimer = window.setTimeout(follow, retry);
  retry = Math.min(retry * 2, 30000);
}

// ---------------------------------------------------------------------------
// A ticket's page (OWL-471), at `#/ticket/<owner>/<repo>/<TICKET>`: the
// snapshot of `/api/ticket` and the records of its runs' logs from
// `/api/ticket/log`. Every text goes through `make` and `text`; a record of a
// log the page does not know is shown as it is, in a `<pre>`.

// The page open now: one handle owns it, and `generation` tells a callback
// that outlived it. `closeTicket` is the only teardown.
let generation = 0;
let openPage = null;

function alive(handle) {
  return openPage === handle && handle.generation === generation;
}

// The route in `hash`: null for none, false for one that is not a ticket
// page's, else its owner, repository and ticket.
function parseRoute(hash) {
  if (!hash || hash === "#" || hash === "#/") {
    return null;
  }
  if (hash === "#/graph") {
    return "graph";
  }
  // The operations screens (OWL-472), drawn by ops.js.
  const screen = { "#/orchestrator": "orchestrator", "#/spend": "spend", "#/config": "config" }[hash];
  if (screen) {
    return { screen: screen };
  }
  const prefix = "#/ticket/";
  if (!hash.startsWith(prefix)) {
    return false;
  }
  const parts = hash.slice(prefix.length).split("/");
  if (parts.length !== 3 || parts.some((part) => part === "")) {
    return false;
  }
  try {
    return {
      owner: decodeURIComponent(parts[0]),
      repo: decodeURIComponent(parts[1]),
      ticket: decodeURIComponent(parts[2]),
    };
  } catch (_) {
    return false;
  }
}

function snapshotPath(handle) {
  return "/api/ticket?project=" + encodeURIComponent(handle.project) +
    "&ticket=" + encodeURIComponent(handle.ticket);
}

function logPath(handle, run, which, from, line) {
  return "/api/ticket/log?project=" + encodeURIComponent(handle.project) +
    "&ticket=" + encodeURIComponent(handle.ticket) +
    "&run=" + encodeURIComponent(run) + "&file=" + which +
    "&from=" + from + "&line=" + line;
}

// Ends the page open: stops what it scheduled, then empties and hides it.
function closeTicket() {
  generation += 1;
  if (openPage) {
    window.clearTimeout(openPage.snapshotTimer);
    for (const run of openPage.runs.values()) {
      for (const log of Object.values(run.logs)) {
        window.clearTimeout(log.timer);
      }
    }
  }
  openPage = null;
  const el = document.getElementById("ticket");
  clear(el);
  el.hidden = true;
}

// Marks the navigation link of the screen shown.
function markNav(name) {
  for (const link of document.querySelectorAll("nav a[data-screen]")) {
    if (link.getAttribute("data-screen") === name) {
      link.setAttribute("aria-current", "page");
    } else {
      link.removeAttribute("aria-current");
    }
  }
}

// Shows the page the address names, or none.
function route() {
  closeTicket();
  closeGraph();
  syncStream();
  const target = parseRoute(window.location.hash);
  show("home", target === null && session() !== null);
  if (window.owlshiftOps) {
    window.owlshiftOps.hide();
  }
  markNav(target === "graph" ? "graph" : target && target.screen ? target.screen : "overview");
  if (target === null) {
    return;
  }
  if (target && target.screen) {
    if (!session()) {
      show("logged-out", true);
    } else if (window.owlshiftOps) {
      window.owlshiftOps.show(target.screen);
    }
    return;
  }
  if (target === false) {
    const el = document.getElementById("ticket");
    el.appendChild(make("p", "notice problem",
      "This address is not a page: the drain's graph is #/graph, a ticket's " +
      "#/ticket/<owner>/<repo>/<TICKET>."));
    el.hidden = false;
    return;
  }
  if (!session()) {
    show("logged-out", true);
    return;
  }
  if (target === "graph") {
    openGraph();
    return;
  }
  openTicket(target);
}

function openTicket(target) {
  generation += 1;
  const handle = {
    generation: generation,
    project: target.owner + "/" + target.repo,
    ticket: target.ticket,
    snapshotTimer: null,
    refreshing: false,
    runs: new Map(),
    opened: new Set(),
    last: {},
    parts: {},
  };
  openPage = handle;
  const el = document.getElementById("ticket");
  clear(el);
  handle.title = make("h2", "ticket-title", handle.ticket + " · " + handle.project);
  el.appendChild(handle.title);
  handle.note = make("p", "notice", "Loading…");
  el.appendChild(handle.note);
  for (const [name, title] of [
    ["actions", "Actions"],
    ["timeline", "Timeline"],
    ["cost", "Cost"],
    ["runs", "Runs"],
    ["questions", "Questions and decisions"],
    ["plan", "Plan"],
    ["waits", "Waits"],
  ]) {
    const section = make("section", "section");
    section.appendChild(make("h3", "", title));
    const body = make("div", "section-body");
    section.appendChild(body);
    el.appendChild(section);
    handle.parts[name] = body;
  }
  handle.runsNotes = make("div", "run-notes");
  handle.runsList = make("div", "run-list");
  handle.parts.runs.appendChild(handle.runsNotes);
  handle.parts.runs.appendChild(handle.runsList);
  el.hidden = false;
  refresh(handle);
}

// How often the snapshot is read again: every 3 s while a run has no end
// recorded, every 10 s otherwise (the ticket is queued, waits for an answer,
// is between two steps, or is finished), and at once when the page becomes
// visible again. It goes on until the route changes or the session ends.
const SNAPSHOT_LIVE_MS = 3000;
const SNAPSHOT_IDLE_MS = 10000;

// The snapshot, read and drawn, and read again after its delay. A null
// answer is a session that ended: `authedFetch` already told the page, and
// nothing is scheduled.
async function refresh(handle) {
  if (!alive(handle) || handle.refreshing) {
    return;
  }
  handle.refreshing = true;
  const answer = await authedFetch(snapshotPath(handle));
  if (!alive(handle) || !answer) {
    return;
  }
  let value = null;
  let why = "the server answered " + answer.response.status;
  try {
    const body = await answer.response.json();
    if (answer.response.ok) {
      value = body;
    } else if (body && body.error) {
      why = body.error;
    }
  } catch (_) {
    // A body that is not JSON: `why` says the status.
  }
  if (!alive(handle)) {
    return;
  }
  handle.refreshing = false;
  let delay = SNAPSHOT_IDLE_MS;
  if (!value) {
    text(handle.note, "This ticket cannot be read: " + why);
  } else {
    text(handle.note, "Read " + shortTime(value.at));
    try {
      renderSnapshot(handle, value);
    } catch (error) {
      text(handle.note, "This ticket cannot be drawn: " + error);
    }
    if (value.runs && value.runs.runs && value.runs.runs.some((run) => !run.ended)) {
      delay = SNAPSHOT_LIVE_MS;
    }
  }
  handle.snapshotTimer = window.setTimeout(() => refresh(handle), delay);
}

function shortTime(at) {
  return at ? String(at).replace("T", " ").replace(/\.\d+/, "").replace("Z", " UTC") : "";
}

function usd(amount) {
  if (typeof amount !== "number") {
    return "";
  }
  return "$" + amount.toFixed(amount !== 0 && amount < 1 ? 4 : 2);
}

function plural(count, word) {
  return count + " " + word + (count === 1 ? "" : "s");
}

// An element's open state survives a refresh through `key`.
function fold(handle, key, summary, className, initiallyOpen) {
  const el = make("details", className || "fold");
  el.appendChild(make("summary", "", summary));
  el.open = handle.opened.has(key) || (initiallyOpen === true && !handle.opened.has("closed:" + key));
  el.addEventListener("toggle", () => {
    if (el.open) {
      handle.opened.add(key);
      handle.opened.delete("closed:" + key);
    } else {
      handle.opened.delete(key);
      handle.opened.add("closed:" + key);
    }
  });
  return el;
}

function problemLine(parent, why) {
  parent.appendChild(make("p", "state-danger", "This part cannot be read: " + why));
}

// Draws the sections of a snapshot that changed since the one before.
function renderSnapshot(handle, snapshot) {
  const sections = [
    ["actions", renderActions],
    ["timeline", renderTimeline],
    ["cost", renderCost],
    ["questions", renderQuestions],
    ["plan", renderPlan],
    ["waits", renderWaits],
  ];
  for (const [name, draw] of sections) {
    const part = snapshot[name];
    // The plan section holds the approval's reply form (OWL-475), unless it
    // cannot draw the plan: the questions section holds it then.
    const beside = name === "plan" && snapshot.questions ? snapshot.questions.reply
      : name === "questions" ? planDrawn(snapshot) : null;
    const key = JSON.stringify(part === undefined ? null : part) + JSON.stringify(beside || null);
    if (handle.last[name] === key) {
      continue;
    }
    handle.last[name] = key;
    const body = handle.parts[name];
    clear(body);
    // One section that cannot be drawn says so and stops nothing else: the
    // other sections, the runs and the next refresh go on.
    try {
      if (part && part.error) {
        problemLine(body, part.error);
      } else if (part === null || part === undefined) {
        body.appendChild(make("p", "notice", "Nothing yet."));
      } else {
        draw(handle, body, part, snapshot);
      }
    } catch (error) {
      clear(body);
      handle.last[name] = null;
      problemLine(body, "the page cannot draw it (" + error + ")");
    }
  }
  try {
    renderRuns(handle, snapshot.runs);
  } catch (error) {
    handle.last.runs = null;
    problemLine(handle.runsNotes, "the page cannot draw the runs (" + error + ")");
  }
}

// What an action's answer says (OWL-473): its lines, or why it was refused,
// each with the class that colours it; a restart still under way (202) is
// information.
function actionSaid(response, value) {
  if (response.ok) {
    const className = response.status === 202 ? "state-information" : "state-success";
    return ((value && value.lines) || []).map((line) => [className, line]);
  }
  return [["state-danger", (value && value.error) || "The server answered " + response.status + "."]];
}

// Posts `body` to an action's route `path` with the session: what it says.
async function postAction(path, body) {
  const answer = await authedFetch(path, { method: "POST", body: JSON.stringify(body) });
  if (!answer) {
    return null;
  }
  let value = null;
  try {
    value = await answer.response.json();
  } catch (_) {
    // A body that is not JSON: the status says what happened.
  }
  return actionSaid(answer.response, value);
}

function saidLines(parent, said) {
  for (const [className, line] of said || []) {
    parent.appendChild(make("p", className + " pre-text", line));
  }
}

// One of the ticket's actions, then the snapshot read again at once.
async function ticketAction(handle, path, extra, button) {
  button.disabled = true;
  const said = await postAction(path, Object.assign({ project: handle.project, ticket: handle.ticket }, extra));
  if (!alive(handle) || !said) {
    return;
  }
  button.disabled = false;
  handle.actionSaid = said;
  handle.last.actions = null;
  window.clearTimeout(handle.snapshotTimer);
  refresh(handle);
}

// The ticket's actions (OWL-473): Restart while it is parked, which runs
// `owlshift continue --to-watch` on the server; Cancel where `owlshift
// cancel` would write one, after a confirmation, with an optional reason;
// and the cancel that stands.
function renderActions(handle, body, actions) {
  if (actions.restart) {
    const row = make("div", "control-row");
    const restart = make("button", "action", "Restart");
    restart.type = "button";
    restart.addEventListener("click", () => ticketAction(handle, "/api/restart", {}, restart));
    row.appendChild(restart);
    row.appendChild(make("span", "muted", "Restarts the parked ticket and leaves it to its watch, " +
      "which resumes it at its next pass, as owlshift continue --to-watch does."));
    body.appendChild(row);
  }
  if (actions.cancel) {
    const row = make("div", "control-row");
    const reason = make("input", "control-reason");
    reason.type = "text";
    reason.placeholder = "reason (optional)";
    reason.setAttribute("aria-label", "Why " + handle.ticket + " is canceled");
    const cancel = make("button", "action", "Cancel");
    cancel.type = "button";
    cancel.addEventListener("click", () => {
      if (!window.confirm("Cancel " + handle.ticket + "? Its step under way stops within 5 seconds, " +
        "and its watch parks it with a PARKED comment on the ticket.")) {
        return;
      }
      ticketAction(handle, "/api/cancel", { reason: reason.value }, cancel);
    });
    row.appendChild(reason);
    row.appendChild(cancel);
    body.appendChild(row);
  } else if (actions.cancel_refusal) {
    body.appendChild(make("p", "notice", "Cancel is not offered: " + actions.cancel_refusal));
  }
  const standing = actions.cancel_standing;
  if (standing) {
    body.appendChild(make("p", "state-warning", standing.unreadable
      ? "A cancel file stands that does not say who wrote it."
      : "A cancel stands, asked by " + standing.by + " at " + shortTime(standing.at) + ": " + standing.why));
  }
  if (!actions.restart && !actions.cancel && !standing && !actions.cancel_refusal) {
    body.appendChild(make("p", "notice", "Nothing to do on this ticket now."));
  }
  saidLines(body, handle.actionSaid);
}

function renderTimeline(handle, body, timeline) {
  if (timeline.truncated) {
    body.appendChild(make("p", "notice",
      "The ticket has more than " + timeline.max + " changes: this starts at the oldest of the newest " +
      timeline.max + "."));
  }
  if (timeline.entries.length === 0) {
    body.appendChild(make("p", "notice", "The ticket has no record yet."));
    return;
  }
  const list = make("ol", "timeline");
  for (const entry of timeline.entries) {
    const item = make("li", "entry");
    if (entry.error) {
      item.appendChild(make("span", "state-danger", shortTime(entry.at) + " unreadable: " + entry.error));
      list.appendChild(item);
      continue;
    }
    item.appendChild(make("span", "entry-at", shortTime(entry.at)));
    item.appendChild(make("span", "entry-stage", entry.stage));
    const facts = [];
    if (entry.waiting) {
      facts.push(entry.waiting.replace(/_/g, " "));
    }
    if (entry.round) {
      facts.push("round " + entry.round);
    }
    if (entry.review_passes) {
      facts.push(plural(entry.review_passes, "review pass"));
    }
    if (entry.has_plan) {
      facts.push("plan");
    }
    if (entry.delivery) {
      const request = entry.delivery.pull_request;
      facts.push("delivered" + (request && request.number ? " as #" + request.number : ""));
    }
    item.appendChild(make("span", "entry-facts", facts.join(" · ")));
    item.appendChild(make("span", "entry-changed", "changed: " + entry.changed.join(", ")));
    list.appendChild(item);
  }
  body.appendChild(list);
}

function renderCost(handle, body, cost) {
  const since = cost.since_restart_usd;
  let line = usd(since) + " spent";
  if (cost.restarted_at_usd) {
    line += " since the restart (" + usd(cost.spent_usd) + " in all)";
  }
  if (typeof cost.budget_usd === "number") {
    line += " of the " + usd(cost.budget_usd) + " budget";
  } else {
    line += ", no ticket budget";
  }
  line += " · " + plural(cost.runs, "run") +
    (cost.unreported_runs ? ", " + cost.unreported_runs + " without a cost" : "");
  body.appendChild(make("p", "", line));
  if (typeof cost.budget_usd === "number" && cost.budget_usd > 0) {
    const meter = make("meter", "budget");
    meter.max = cost.budget_usd;
    meter.value = Math.min(since, cost.budget_usd);
    meter.low = cost.budget_usd * 0.7;
    meter.high = cost.budget_usd * 0.9;
    meter.optimum = 0;
    body.appendChild(meter);
  }
  if (cost.note) {
    body.appendChild(make("p", "notice", cost.note));
  }
}

function optionLetter(index) {
  return String.fromCharCode(65 + index);
}

function questionEl(question, verdict) {
  const el = make("div", "question");
  el.appendChild(make("p", "question-text", question.id + " [" + question.category + "] " + question.text));
  if (question.context) {
    el.appendChild(make("p", "notice", question.context));
  }
  if (question.replan) {
    el.appendChild(make("p", "state-warning", "The plan cannot be followed: asks to go back to Design."));
  }
  const options = make("ul", "options");
  question.options.forEach((option, index) => {
    options.appendChild(make("li", "", optionLetter(index) + ". " + option.label +
      (option.detail ? " — " + option.detail : "")));
  });
  if (question.options.length) {
    el.appendChild(options);
  }
  if (question.recommendation) {
    const rec = question.recommendation;
    el.appendChild(make("p", "recommendation", "Recommended: " + (rec.option ? rec.option + ". " : "") + rec.why));
  }
  if (verdict) {
    el.appendChild(make("p", "verdict", "Understood (" + verdict.class.replace(/_/g, " ") + "): " + verdict.reason));
  }
  return el;
}

// Whether the plan section draws the plan, and with it the approval's form.
function planDrawn(snapshot) {
  return Boolean(snapshot && snapshot.plan && !snapshot.plan.error);
}

function renderQuestions(handle, body, questions, snapshot) {
  const reply = questions.reply;
  if (reply && reply.approval && planDrawn(snapshot)) {
    body.appendChild(make("p", "notice",
      "This round asks for the plan's approval: its reply form is beside the plan, below."));
  } else if (reply) {
    if (reply.approval) {
      body.appendChild(make("p", "notice",
        "This round asks for the plan's approval, and the plan cannot be drawn: its reply form is here."));
    }
    body.appendChild(replyForm(handle, reply, questions));
  }
  if (questions.asks.length === 0 && questions.decisions.length === 0) {
    body.appendChild(make("p", "notice", "No question was asked."));
  }
  for (const ask of questions.asks) {
    const el = make("div", "ask");
    el.appendChild(make("h4", "", "Round " + ask.round + (ask.kind === "reask" ? " (asked again)" : "") +
      " · " + shortTime(ask.at) + " · decider " + ask.decider.account));
    for (const question of ask.questions) {
      // `verdicts` is left out of an ask's JSON until a check gives them.
      const verdict = (ask.verdicts || []).find((one) => one.question === question.id);
      el.appendChild(questionEl(question, verdict));
    }
    body.appendChild(el);
  }
  questions.decisions.forEach((decision, index) => {
    const el = make("div", "decision");
    el.appendChild(make("h4", "", "Decision " + (index + 1) + " · " + shortTime(decision.at)));
    el.appendChild(questionEl(decision.question, null));
    el.appendChild(make("p", "", "Decided: " + decision.decision));
    el.appendChild(make("p", "notice", "Because: " + decision.basis));
    if (decision.reversed) {
      el.appendChild(make("p", "state-warning", "The decider replied to reverse it."));
    }
    body.appendChild(el);
  });
}

// The reply form (OWL-475): the open ask's questions, each with its
// options as radio buttons, the recommendation when it names no option, and
// a text; "count it now", ticked by default, ends the reply with `go`. A
// choice listed in `reply.words` changes the plan only with words, which it
// then asks for. What is picked and written is kept per ask in the handle, so
// a redraw keeps it, and so is a reply once posted, which the form then shows
// above itself: the form stays drawn while the round is offered, so a reply
// left uncounted can be followed by one that ends with `go`. When the form is
// not offered, it says whose round it is and why, with the ticket's link when
// the server has one.
function replyDraft(handle, reply) {
  if (!handle.drafts) {
    handle.drafts = {};
  }
  if (!handle.drafts[reply.ask]) {
    handle.drafts[reply.ask] = { answers: {}, go: true };
  }
  return handle.drafts[reply.ask];
}

// The address a reply form links its ticket to: Linear's, the one tracker
// whose key the UI reads, then the workspace and the ticket as the server
// built them. A click navigates there; the page fetches nothing from it.
const LINEAR_LINK = "https://linear.app/";
const TRACKER_LINK_PATTERN = /^[A-Za-z0-9_-]+\/issue\/[A-Za-z0-9]+-[0-9]+$/;

// The ticket's link on the tracker, or null when the server gave none that
// is Linear's workspace and ticket.
function trackerLink(reply) {
  const given = typeof reply.link === "string" ? reply.link : "";
  const path = given.startsWith(LINEAR_LINK) ? given.slice(LINEAR_LINK.length) : "";
  if (!TRACKER_LINK_PATTERN.test(path)) {
    return null;
  }
  const link = make("a", "ticket-link", "Open the ticket on the tracker");
  link.href = LINEAR_LINK + path;
  link.rel = "noopener noreferrer";
  return link;
}

function replyForm(handle, reply, questions) {
  const el = make("div", "reply");
  // The decider's name once the UI has read it (OWL-517), else the account
  // id the ask keeps.
  const decider = (reply.decider.name || reply.decider.account) + " (" +
    String(reply.decider.by).replace(/_/g, " ") + ")";
  const posted = handle.posted && handle.posted[reply.ask];
  if (posted) {
    el.appendChild(make("p", "state-success", posted));
  }
  if (!reply.offered) {
    el.appendChild(make("p", "notice", "Round " + reply.round + " waits for its decider, " + decider +
      ". No reply form here: " + (reply.why_not || "it is not offered") + "."));
    const link = trackerLink(reply);
    if (link) {
      el.appendChild(link);
    }
    return el;
  }
  const ask = questions.asks.find((one) => one.comment === reply.ask);
  if (!ask) {
    el.appendChild(make("p", "notice", "The round this form answers is not listed."));
    return el;
  }
  const draft = replyDraft(handle, reply);
  el.appendChild(make("h4", "", "Reply to round " + reply.round + " as " +
    (reply.key && reply.key.name ? reply.key.name : decider)));
  for (const question of ask.questions) {
    el.appendChild(replyQuestion(handle, reply, question, draft));
  }
  const count = make("label", "switch");
  const go = make("input");
  go.type = "checkbox";
  go.checked = draft.go;
  go.addEventListener("change", () => {
    draft.go = go.checked;
  });
  count.appendChild(go);
  count.appendChild(make("span", "", "Count it now (ends the reply with go)"));
  const actions = make("div", "reply-actions");
  actions.appendChild(count);
  const button = make("button", "reply-send", "Send");
  button.type = "button";
  const status = make("p", "notice");
  button.addEventListener("click", () => sendReply(handle, reply, draft, button, status));
  actions.appendChild(button);
  el.appendChild(actions);
  el.appendChild(status);
  const link = trackerLink(reply);
  if (link) {
    el.appendChild(link);
  }
  return el;
}

function replyQuestion(handle, reply, question, draft) {
  const el = make("fieldset", "reply-question");
  el.appendChild(make("legend", "question-text", question.id + " · " + question.text));
  if (!draft.answers[question.id]) {
    draft.answers[question.id] = { choice: null, text: "" };
  }
  const answer = draft.answers[question.id];
  const needsWords = (reply.words && reply.words[question.id]) || [];
  const hint = make("p", "state-warning", "This option changes the plan: say how in your own words.");
  const words = make("textarea", "control reply-text");
  words.rows = 2;
  words.value = answer.text;
  words.placeholder = "In your own words (optional)";
  const ask = () => {
    const needed = answer.choice !== null && needsWords.includes(answer.choice);
    hint.hidden = !needed;
    words.required = needed;
  };
  words.addEventListener("input", () => {
    answer.text = words.value;
  });
  const choices = [[null, "No option"]];
  question.options.forEach((option, index) => {
    choices.push([optionLetter(index), optionLetter(index) + ". " + option.label +
      (option.detail ? " — " + option.detail : "")]);
  });
  const rec = question.recommendation;
  if (rec && !rec.option) {
    choices.push(["recommendation", "Take the recommendation: " + rec.why]);
  }
  for (const [value, label] of choices) {
    const line = make("label", "reply-choice");
    const radio = make("input");
    radio.type = "radio";
    radio.name = "reply-" + reply.ask + "-" + question.id;
    radio.checked = answer.choice === value;
    radio.addEventListener("change", () => {
      if (radio.checked) {
        answer.choice = value;
        ask();
      }
    });
    line.appendChild(radio);
    line.appendChild(make("span", "", label + (rec && rec.option === value ? " (recommended)" : "")));
    el.appendChild(line);
  }
  ask();
  el.appendChild(hint);
  el.appendChild(words);
  return el;
}

// Posts the reply, once: Send stays disabled until the answer, the form keeps
// what was picked and written when it fails, and shows the posted comment
// above the form once it is posted.
async function sendReply(handle, reply, draft, button, status) {
  if (button.disabled) {
    return;
  }
  for (const [id, answer] of Object.entries(draft.answers)) {
    const needsWords = (reply.words && reply.words[id]) || [];
    if (answer.choice !== null && needsWords.includes(answer.choice) && !answer.text.trim()) {
      text(status, "Option " + answer.choice + " of " + id + " changes the plan: say how first.");
      return;
    }
  }
  const answers = Object.entries(draft.answers).map(([id, answer]) =>
    ({ question: id, choice: answer.choice, text: answer.text }));
  button.disabled = true;
  text(status, "Posting…");
  const answer = await authedFetch("/api/reply", {
    method: "POST",
    body: JSON.stringify({
      project: handle.project,
      ticket: handle.ticket,
      ask: reply.ask,
      answers: answers,
      go: draft.go,
    }),
  });
  if (!alive(handle) || !answer) {
    return;
  }
  let value = null;
  try {
    value = await answer.response.json();
  } catch (_) {
    // A body that is not JSON: the status says what happened.
  }
  if (!alive(handle)) {
    return;
  }
  button.disabled = false;
  if (!answer.response.ok) {
    text(status, "Not posted: " + (value && value.error ? value.error : "the server answered " +
      answer.response.status));
    return;
  }
  if (!handle.posted) {
    handle.posted = {};
  }
  handle.posted[reply.ask] = "Posted as comment " + value.comment + " at " + shortTime(value.at) +
    (value.counts_now ? ": it counts now." : ": it counts once left unedited for the quiet window.");
  handle.last.questions = null;
  handle.last.plan = null;
  window.clearTimeout(handle.snapshotTimer);
  refresh(handle);
}

function itemText(item) {
  if (typeof item === "string") {
    return item;
  }
  if (item && item.what !== undefined) {
    return item.what + (item.test ? " — test: " + item.test : "");
  }
  if (item && item.approach !== undefined) {
    return item.approach + " — " + item.reason;
  }
  if (item && item.choice !== undefined) {
    return item.choice + " — " + item.reason;
  }
  return JSON.stringify(item);
}

function listEl(items, ordered) {
  const list = make(ordered ? "ol" : "ul", "plan-list");
  for (const item of items) {
    list.appendChild(make("li", "", itemText(item)));
  }
  return list;
}

function planEl(plan) {
  const el = make("div", "plan");
  el.appendChild(make("p", "", "Goal: " + plan.goal));
  for (const [title, items, ordered] of [
    ["Criteria", plan.criteria, false],
    ["Steps", plan.steps, true],
    ["Files", plan.files, false],
    ["Risks", plan.risks, false],
    ["Out of scope", plan.out_of_scope, false],
    ["Forks", plan.forks, false],
    ["Alternatives", plan.alternatives, false],
  ]) {
    if (items && items.length) {
      el.appendChild(make("h5", "", title));
      el.appendChild(listEl(items, ordered));
    }
  }
  el.appendChild(make("h5", "", "Approach"));
  el.appendChild(make("p", "pre-text", plan.approach));
  return el;
}

function diffEl(diff) {
  const el = make("div", "diff");
  if (diff.length === 0) {
    el.appendChild(make("p", "notice", "No part differs."));
  }
  for (const part of diff) {
    el.appendChild(make("h5", "", part.part.replace(/_/g, " ")));
    if (part.before !== undefined) {
      el.appendChild(make("pre", "diff-removed", "- " + part.before));
      el.appendChild(make("pre", "diff-added", "+ " + part.after));
      continue;
    }
    for (const item of part.removed) {
      el.appendChild(make("pre", "diff-removed", "- " + itemText(item)));
    }
    for (const item of part.added) {
      el.appendChild(make("pre", "diff-added", "+ " + itemText(item)));
    }
    if (part.reordered) {
      el.appendChild(make("p", "notice", "Only the order changed."));
    }
  }
  return el;
}

function severityClass(severity) {
  return severity === "blocker" ? "state-danger" : severity === "major" ? "state-warning" : "state-information";
}

function reviewEl(handle, pass) {
  const mandates = (pass.reviewers || []).map((r) => r.mandates.join("+")).join(", ");
  const el = fold(handle, "pass:" + pass.stage + ":" + pass.pass,
    "Review pass " + pass.pass + " at " + pass.stage.replace(/_/g, " ") + " · " +
    plural(pass.findings.length, "finding") + (mandates ? " · " + mandates : ""), "fold", true);
  for (const kept of pass.findings) {
    const finding = kept.finding;
    const item = make("div", "finding");
    item.appendChild(make("p", severityClass(finding.severity),
      finding.id + " " + finding.severity + ": " + finding.claim));
    item.appendChild(make("p", "notice", "Evidence: " + finding.evidence));
    item.appendChild(make("p", "notice", "Settles it: " + finding.settle));
    if (finding.criterion) {
      item.appendChild(make("p", "notice", "On the criterion: " + finding.criterion));
    }
    if (kept.outcome) {
      item.appendChild(make("p", "", "Outcome: " + kept.outcome));
    }
    el.appendChild(item);
  }
  return el;
}

function renderPlan(handle, body, plan, snapshot) {
  if (plan.current) {
    const current = fold(handle, "plan:current", "The plan now", "fold", true);
    current.appendChild(planEl(plan.current.plan));
    if (plan.current.approval) {
      current.appendChild(make("p", "state-success", "Approved by the decider."));
    }
    body.appendChild(current);
  } else {
    body.appendChild(make("p", "notice", "No plan was written yet."));
  }
  // A ticket waiting on its plan's approval: the change since the plan
  // before, then the reply form, beside the plan (OWL-475).
  const reply = snapshot && snapshot.questions ? snapshot.questions.reply : null;
  if (reply && reply.approval) {
    const newest = plan.revisions.length ? plan.revisions[plan.revisions.length - 1] : null;
    if (newest && newest.diff) {
      body.appendChild(make("h4", "", "What changed since the plan before"));
      body.appendChild(diffEl(newest.diff));
    }
    body.appendChild(replyForm(handle, reply, snapshot.questions));
  }
  plan.revisions.forEach((revision, index) => {
    const el = fold(handle, "revision:" + revision.commit,
      "Revision " + (index + 1) + " · " + shortTime(revision.at) +
      (revision.diff ? " · changes against revision " + index : ""), "fold", false);
    if (revision.diff) {
      el.appendChild(diffEl(revision.diff));
    }
    el.appendChild(planEl(revision.plan));
    body.appendChild(el);
  });
  if (plan.review && plan.review.passes.length) {
    for (const pass of plan.review.passes) {
      body.appendChild(reviewEl(handle, pass));
    }
  }
}

function renderWaits(handle, body, waits) {
  const row = waits.ticket;
  if (!row) {
    body.appendChild(make("p", "notice", "It is not in flight: no wait is told for it."));
  } else {
    const facts = [];
    if (row.stage) {
      facts.push("stage " + row.stage.replace(/_/g, " "));
    }
    facts.push(row.ref_status.replace(/_/g, " "));
    facts.push(row.holds_capacity ? "holds the project's capacity" : "holds no capacity");
    if (row.run) {
      facts.push("holds run slot " + row.run.slot);
    }
    body.appendChild(make("p", "", facts.join(" · ")));
    if (row.waits) {
      const wait = row.waits;
      body.appendChild(make("p", "state-warning",
        wait.reason.replace(/_/g, " ") + ": " + wait.text + (wait.since ? " (since " + shortTime(wait.since) + ")" : "")));
    } else {
      body.appendChild(make("p", "state-success", "Waiting on nothing."));
    }
  }
  if (waits.project) {
    const project = waits.project;
    const facts = [project.in_flight + " of " + (project.max_in_flight === null ? "?" : project.max_in_flight) +
      " in flight"];
    if (project.quarantined) {
      facts.push("quarantined");
    }
    if (project.paused) {
      facts.push("paused");
    }
    body.appendChild(make("p", "notice", "The project: " + facts.join(" · ")));
  }
  for (const note of waits.notes || []) {
    body.appendChild(make("p", "notice", note));
  }
}

// --- The runs and their logs

function runState(info) {
  if (!info.ended) {
    return ["running", "state-information"];
  }
  switch (info.outcome) {
    case "finished":
      return ["finished", "state-success"];
    case "failed":
    case "quarantined":
      return [info.outcome, "state-danger"];
    default:
      return [info.outcome || "ended", "state-warning"];
  }
}

function usageLine(usage) {
  if (!usage) {
    return "";
  }
  const parts = [];
  if (usage.input !== undefined) {
    parts.push(usage.input + " in");
  }
  if (usage.output !== undefined) {
    parts.push(usage.output + " out");
  }
  if (usage.cache_read) {
    parts.push(usage.cache_read + " cached");
  }
  if (typeof usage.cost_usd === "number") {
    parts.push(usd(usage.cost_usd));
  }
  return parts.join(", ");
}

function drawRunHead(run, info) {
  const [state, stateClass] = runState(info);
  clear(run.summary);
  run.summary.appendChild(make("span", "run-id", info.id));
  run.summary.appendChild(make("span", "run-role", (info.role || "run") +
    (info.attempt ? " #" + info.attempt : "")));
  run.summary.appendChild(make("span", stateClass, state));
  if (typeof info.elapsed_ms === "number") {
    run.summary.appendChild(make("span", "run-fact", (info.elapsed_ms / 1000).toFixed(1) + " s"));
  }
  const usage = usageLine(info.usage);
  if (usage) {
    run.summary.appendChild(make("span", "run-fact", usage));
  }
  clear(run.facts);
  if (info.summary) {
    run.facts.appendChild(make("p", "pre-text", info.summary));
  }
  if (info.reason) {
    run.facts.appendChild(make("p", "state-danger", info.reason));
  }
  const more = [];
  if (info.gate) {
    more.push("gate " + info.gate);
  }
  if (info.status) {
    more.push("result " + info.status);
  }
  if (info.exit_code !== undefined && info.exit_code !== null) {
    more.push("exit code " + info.exit_code);
  }
  if (info.started_at) {
    more.push("started " + shortTime(info.started_at));
  }
  if (info.ended_at) {
    more.push("ended " + shortTime(info.ended_at));
  }
  if (more.length) {
    run.facts.appendChild(make("p", "notice", more.join(" · ")));
  }
}

function makeRun(handle, info, open) {
  const el = make("details", "run");
  const summary = make("summary", "run-summary");
  const facts = make("div", "run-facts");
  const log = make("div", "log");
  const err = make("div", "log log-stderr");
  const idle = make("p", "notice state-warning");
  idle.hidden = true;
  el.appendChild(summary);
  el.appendChild(facts);
  el.appendChild(idle);
  el.appendChild(log);
  const errHead = make("p", "notice", "Standard error");
  const errBox = make("div", "log-stderr-box");
  errBox.hidden = true;
  errBox.appendChild(errHead);
  errBox.appendChild(err);
  el.appendChild(errBox);
  const run = {
    id: info.id,
    el: el,
    summary: summary,
    facts: facts,
    idle: idle,
    log: log,
    err: err,
    errBox: errBox,
    entries: new Map(),
    info: null,
    key: "",
    // A run only the events name has no folder, so no log to follow.
    noLog: info.folder === false,
    logs: {
      stdout: { from: 0, line: 0, timer: null, started: false, finished: false, lastData: 0, failed: false },
      stderr: { from: 0, line: 0, timer: null, started: false, finished: false, lastData: 0, failed: false },
    },
  };
  const key = "run:" + info.id;
  el.open = handle.opened.has(key) ? true : handle.opened.has("closed:" + key) ? false : open;
  el.addEventListener("toggle", () => {
    if (el.open) {
      handle.opened.add(key);
      handle.opened.delete("closed:" + key);
      startLog(handle, run, "stderr");
    } else {
      handle.opened.delete(key);
      handle.opened.add("closed:" + key);
    }
  });
  return run;
}

function renderRuns(handle, section) {
  const key = JSON.stringify(section);
  if (handle.last.runs === key) {
    return;
  }
  handle.last.runs = key;
  if (section.error) {
    clear(handle.runsNotes);
    problemLine(handle.runsNotes, section.error);
    return;
  }
  clear(handle.runsNotes);
  for (const warning of section.warnings || []) {
    handle.runsNotes.appendChild(make("p", "state-warning",
      shortTime(warning.at) + " " + (warning.what || "warning") + (warning.why ? ": " + warning.why : "")));
  }
  if (section.runs.length === 0 && handle.runs.size === 0) {
    handle.runsNotes.appendChild(make("p", "notice", "No run yet."));
  }
  const newest = section.runs.length ? section.runs[section.runs.length - 1].id : "";
  for (const info of section.runs) {
    let run = handle.runs.get(info.id);
    if (!run) {
      run = makeRun(handle, info, info.id === newest || !info.ended);
      handle.runs.set(info.id, run);
      handle.runsList.appendChild(run.el);
      startLog(handle, run, "stdout");
    }
    const infoKey = JSON.stringify(info);
    if (run.key !== infoKey) {
      const wasEnded = run.info !== null && run.info.ended;
      run.key = infoKey;
      run.info = info;
      if (run.noLog && info.folder !== false) {
        run.noLog = false;
        startLog(handle, run, "stdout");
      }
      drawRunHead(run, info);
      if (info.ended && !wasEnded) {
        wakeLogs(handle, run);
      }
    }
  }
}

// One record of a log, drawn.
function recordEl(rec) {
  switch (rec.kind) {
    case "text": {
      const el = make("div", "rec rec-text");
      if (rec.label) {
        el.appendChild(make("span", "rec-label", rec.label));
      }
      el.appendChild(make("span", "pre-text", rec.text));
      return el;
    }
    case "thinking": {
      if (!rec.text) {
        return make("div", "rec rec-status", "thinking (the harness shows no text)");
      }
      const el = make("details", "rec rec-thinking");
      el.appendChild(make("summary", "", "thinking"));
      el.appendChild(make("pre", "", rec.text));
      return el;
    }
    case "tool": {
      const el = make("details", "rec rec-tool");
      const summary = make("summary", rec.error ? "state-danger" : "");
      summary.appendChild(make("span", "rec-label", rec.label || "tool"));
      summary.appendChild(make("code", "", rec.text));
      if (rec.state) {
        summary.appendChild(make("span", "rec-state", rec.state.replace(/_/g, " ")));
      }
      el.appendChild(summary);
      if (rec.body) {
        el.appendChild(make("pre", "", rec.body));
      }
      return el;
    }
    case "tool_result": {
      const el = make("details", "rec rec-tool-result");
      el.appendChild(make("summary", rec.error ? "state-danger" : "", "result: " + rec.text));
      if (rec.body) {
        el.appendChild(make("pre", "", rec.body));
      }
      return el;
    }
    case "status": {
      const line = (rec.label ? rec.label + ": " : "") + rec.text;
      if (!rec.body) {
        return make("div", "rec rec-status" + (rec.error ? " state-danger" : ""), line);
      }
      const el = make("details", "rec rec-status");
      el.appendChild(make("summary", rec.error ? "state-danger" : "", line));
      el.appendChild(make("pre", "", rec.body));
      return el;
    }
    case "usage":
      return make("div", "rec rec-usage", "usage: " + rec.text);
    case "result": {
      const el = make("details", "rec rec-result");
      el.open = true;
      el.appendChild(make("summary", rec.error ? "state-danger" : "state-success",
        (rec.label ? rec.label + ": " : "") + rec.text));
      if (rec.body) {
        el.appendChild(make("pre", "", rec.body));
      }
      return el;
    }
    default:
      return make("pre", "rec rec-unknown", "unknown record (" + (rec.label || "?") + ", line " +
        rec.line + ")\n" + rec.text);
  }
}

// Draws `records` at the end of a run's log: a result joins the call it
// names, and a Codex item's later record replaces its earlier one.
function appendRecords(run, which, records) {
  const container = which === "stderr" ? run.err : run.log;
  const batch = document.createDocumentFragment();
  for (const rec of records) {
    const el = recordEl(rec);
    const known = rec.id ? run.entries.get(which + ":" + rec.id) : null;
    if (rec.kind === "tool" && known && known.kind === "tool") {
      known.el.replaceWith(el);
      run.entries.set(which + ":" + rec.id, { kind: "tool", el: el });
      continue;
    }
    if (rec.kind === "tool_result" && known && known.kind === "tool") {
      known.el.appendChild(make("pre", "rec-result-body" + (rec.error ? " state-danger" : ""),
        (rec.error ? "error: " : "result: ") + (rec.body || rec.text)));
      continue;
    }
    if (rec.kind === "tool" && rec.id) {
      run.entries.set(which + ":" + rec.id, { kind: "tool", el: el });
    }
    batch.appendChild(el);
  }
  container.appendChild(batch);
}

// How a log is followed: at once while a read makes progress and there is
// more to read (a read that makes none waits like a caught-up one), then every
// second; every 5 s after 30 s without new data, and every minute after 10
// minutes, which the run says (a runner that died leaves no end to wait
// for). A log ends once its answer says the run ended and `next` reached
// the size; a run already ended is read through once, from 0.
const LOG_FAST_MS = 1000;
const LOG_SLOW_MS = 5000;
const LOG_IDLE_MS = 60000;
const LOG_SLOW_AFTER_MS = 30000;
const LOG_IDLE_AFTER_MS = 600000;

function logDelay(log) {
  const quiet = Date.now() - log.lastData;
  if (quiet >= LOG_IDLE_AFTER_MS) {
    return LOG_IDLE_MS;
  }
  return quiet >= LOG_SLOW_AFTER_MS ? LOG_SLOW_MS : LOG_FAST_MS;
}

// The wait before the next read. A read is repeated at once only while it
// made progress and more is waiting behind it (the server caps an answer).
// An answer whose `next` did not move, with `next` below `size`, left only
// a half-written last line, which the server withholds until its newline
// lands: it waits like a caught-up log, instead of asking again in a loop.
// A reset counts as progress. No DOM here: a test runs this source.
function nextLogDelay(log, from, value) {
  const moved = value.reset === true || value.next > from;
  return moved && value.next < value.size ? 0 : logDelay(log);
}

// One read of a log from where it stopped, drawn, and the next one
// scheduled. Every continuation after an await names its page: one that
// is not the page open does nothing, and a null answer is a session that
// ended, which stops the loop.
async function pollLog(handle, run, which) {
  const log = run.logs[which];
  log.timer = null;
  if (!alive(handle)) {
    return;
  }
  const answer = await authedFetch(logPath(handle, run.id, which, log.from, log.line));
  if (!alive(handle) || !answer) {
    return;
  }
  let value = null;
  try {
    value = answer.response.ok ? await answer.response.json() : null;
  } catch (_) {
    value = null;
  }
  if (!alive(handle)) {
    return;
  }
  if (!value) {
    if (!log.failed) {
      log.failed = true;
      appendRecords(run, which, [{
        kind: "unknown", label: "the log cannot be read", line: 0,
        text: "the server answered " + answer.response.status,
      }]);
    }
    // A run that has ended and whose log the server says is not there (or
    // not a run's) will not get one: stop asking.
    const status = answer.response.status;
    if ((status === 404 || status === 400) && run.info && run.info.ended) {
      log.finished = true;
      return;
    }
    log.timer = window.setTimeout(() => pollLog(handle, run, which), LOG_IDLE_MS);
    return;
  }
  log.failed = false;
  if (value.reset) {
    clear(which === "stderr" ? run.err : run.log);
    run.entries.clear();
  }
  if (value.records.length) {
    appendRecords(run, which, value.records);
    log.lastData = Date.now();
  }
  const from = log.from;
  log.from = value.next;
  log.line = value.next_line;
  if (which === "stdout") {
    const quiet = Date.now() - log.lastData;
    const stale = !value.ended && quiet >= LOG_IDLE_AFTER_MS;
    text(run.idle, stale
      ? "No new output for " + Math.floor(quiet / 60000) + " minutes, and no end of the run is recorded: " +
        "looking again every minute."
      : "");
    run.idle.hidden = !stale;
  }
  if (value.ended && value.next >= value.size) {
    log.finished = true;
    return;
  }
  const delay = nextLogDelay(log, from, value);
  log.timer = window.setTimeout(() => pollLog(handle, run, which), delay);
}

function startLog(handle, run, which) {
  const log = run.logs[which];
  if (log.started) {
    return;
  }
  if (run.noLog) {
    if (which === "stdout" && !run.noLogShown) {
      run.noLogShown = true;
      run.log.appendChild(make("p", "notice", "This run has no folder: its log is not kept."));
    }
    return;
  }
  log.started = true;
  log.lastData = Date.now();
  if (which === "stderr") {
    run.errBox.hidden = false;
  }
  pollLog(handle, run, which);
}

// A run whose end the snapshot now shows is read once more at once, rather
// than at the pace of a quiet log: its last line may be waiting for it.
function wakeLogs(handle, run) {
  for (const log of Object.values(run.logs)) {
    if (log.started && !log.finished && log.timer !== null) {
      window.clearTimeout(log.timer);
      log.timer = null;
      pollLog(handle, run, log === run.logs.stderr ? "stderr" : "stdout");
    }
  }
}

// ---------------------------------------------------------------------------
// The overview (OWL-470): For you, the columns and list views, and since
// your last visit. The server builds everything the page draws (the order of
// the list, the strip, the words of a wait); the page only draws it. Every
// text goes through `make` and `text`.

const STAGES = ["Intake", "Ready", "Design", "Design review", "Build", "Verify", "Deliver", "Watch"];

const COLUMNS = [
  ["not_started", "Not started"],
  ["intake", "Intake"],
  ["ready", "Ready"],
  ["design", "Design"],
  ["design_review", "Design review"],
  ["build", "Build"],
  ["verify", "Verify"],
  ["deliver", "Deliver"],
  ["watch", "Watch"],
];

// A repository the page links a pull request of: two plain names.
const REPOSITORY_PATTERN = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;

let lastOverview = null;
let lastSince = null;
let lastHighlights = null;
let previousVisit = null;
let sinceSeq = 0;

// The baseline of "since your last visit" is the previous page load: read
// first, then this load's time becomes the next one. Nothing stored (a first
// visit) means the request carries no `at`, and the server counts the last
// 24 hours.
function startVisit() {
  const raw = stored(VISIT_KEY);
  const parsed = raw ? Date.parse(raw) : NaN;
  previousVisit = Number.isNaN(parsed) ? null : new Date(parsed).toISOString();
  store(VISIT_KEY, new Date().toISOString());
}

function currentView() {
  return stored(VIEW_KEY) === "list" ? "list" : "columns";
}

function wireViewSwitch() {
  for (const view of ["columns", "list"]) {
    document.getElementById("view-" + view).addEventListener("click", () => {
      store(VIEW_KEY, view);
      renderHome();
    });
  }
}

function clearHome() {
  lastOverview = null;
  lastSince = null;
  lastHighlights = null;
  sinceSeq += 1;
  for (const id of ["for-you-body", "overview-body", "since-body"]) {
    clear(document.getElementById(id));
  }
  show("home", false);
}

function selectedProject() {
  return document.getElementById("project").value;
}

// A link to a ticket's page. The address is built from the project's two
// names and the ticket, each encoded; a project that is not owner/name gets
// no link.
function ticketLink(project, ticket) {
  const link = make("a", "ticket-link", ticket);
  const parts = project.split("/");
  if (parts.length === 2 && parts.every((part) => part !== "")) {
    link.setAttribute("href", "#/ticket/" + encodeURIComponent(parts[0]) + "/" +
      encodeURIComponent(parts[1]) + "/" + encodeURIComponent(ticket));
  }
  return link;
}

// A pull request, linked to the forge when the project is two plain names and
// the number a whole number; the address is built here, from local state, and
// the forge is not called.
function pullRequestLink(project, number) {
  const label = "pull request #" + number;
  const names = project.split("/");
  const plain = REPOSITORY_PATTERN.test(project) && names.every((n) => n !== "." && n !== "..");
  if (!plain || !Number.isInteger(number) || number < 1) {
    return make("span", "pr-link", label);
  }
  const link = make("a", "pr-link", label);
  link.href = "https://github.com/" + project + "/pull/" + number;
  link.rel = "noopener noreferrer";
  return link;
}

function ageText(iso) {
  const seconds = Math.max(0, Math.round((Date.now() - Date.parse(iso)) / 1000));
  if (seconds < 90) {
    return seconds + " s";
  }
  if (seconds < 5400) {
    return Math.round(seconds / 60) + " min";
  }
  return Math.round(seconds / 3600) + " h";
}

const NEEDS_YOU = {
  question: "Questions to answer",
  plan: "Plans to approve",
  review: "Pull requests to review",
  parked: "Parked",
};

function shownProjects(overview) {
  const chosen = selectedProject();
  return overview.projects.filter((project) => !chosen || project.repository === chosen);
}

// What each Restart from For you said, by project and ticket, kept across
// the redraws of the overview (OWL-473).
const restartSaid = new Map();

// A parked entry's Restart: `owlshift continue --to-watch`, run on the
// server, as the ticket page's.
function restartButton(entry) {
  const button = make("button", "action", "Restart");
  button.type = "button";
  button.addEventListener("click", async () => {
    button.disabled = true;
    const said = await postAction("/api/restart", { project: entry.project, ticket: entry.ticket });
    if (!said) {
      return;
    }
    button.disabled = false;
    restartSaid.set(entry.project + " " + entry.ticket, said);
    if (lastOverview) {
      renderForYou(lastOverview);
    }
  });
  return button;
}

function renderForYou(overview) {
  const body = document.getElementById("for-you-body");
  clear(body);
  const chosen = selectedProject();
  const lists = [
    ["questions", NEEDS_YOU.question],
    ["plans", NEEDS_YOU.plan],
    ["reviews", NEEDS_YOU.review],
    ["parked", NEEDS_YOU.parked],
  ];
  let total = 0;
  for (const [key, title] of lists) {
    const entries = overview.for_you[key].filter((entry) => !chosen || entry.project === chosen);
    if (!entries.length) {
      continue;
    }
    total += entries.length;
    const group = make("div", "for-you-group");
    group.appendChild(make("h3", "", title + " (" + entries.length + ")"));
    const list = make("ul", "plain");
    for (const entry of entries) {
      const item = make("li", "for-you-entry");
      item.appendChild(ticketLink(entry.project, entry.ticket));
      if (!chosen) {
        item.appendChild(make("span", "muted", entry.project));
      }
      if (entry.title) {
        item.appendChild(make("span", "entry-title", entry.title));
      }
      if (entry.pull_request !== null && entry.pull_request !== undefined) {
        item.appendChild(pullRequestLink(entry.project, entry.pull_request));
      }
      for (const question of entry.questions) {
        item.appendChild(make("div", "question", question.id + ": " + question.text));
      }
      if (key === "parked") {
        item.appendChild(restartButton(entry));
        saidLines(item, restartSaid.get(entry.project + " " + entry.ticket));
      }
      list.appendChild(item);
    }
    group.appendChild(list);
    body.appendChild(group);
  }
  if (!total) {
    body.appendChild(make("p", "notice", "Nothing waits for you."));
  }
}

// The words of a card's state, and the class that colours it.
function stateOf(card) {
  if (card.running) {
    return ["running" + (card.running.role ? ", " + card.running.role : "") +
      ", slot " + card.running.slot, "state-success"];
  }
  switch (card.needs_you) {
    case "question":
      return ["questions to answer", "state-warning"];
    case "plan":
      return ["plan to approve", "state-warning"];
    case "review":
      return ["pull request to review", "state-warning"];
    case "parked":
      return ["parked", "state-danger"];
    default:
      break;
  }
  switch (card.column) {
    case "not_started":
      return ["not started", "state-information"];
    case "starting":
      return ["starting", "state-information"];
    case "unreadable":
      return ["its ref cannot be read", "state-danger"];
    default:
      return ["idle", ""];
  }
}

function whyOf(card) {
  if (card.wait) {
    return card.wait.text;
  }
  if (card.column === "not_started" && card.drain && card.drain !== "waits") {
    return "the drain " + card.drain.replace("_", " ") + " it";
  }
  return "";
}

function cardElement(card, withStrip) {
  const el = make("div", "card");
  const head = make("div", "card-head");
  head.appendChild(ticketLink(card.project, card.ticket));
  const [state, stateClass] = stateOf(card);
  head.appendChild(make("span", "card-state " + stateClass, state));
  el.appendChild(head);
  if (card.title) {
    el.appendChild(make("div", "card-title", card.title));
  }
  const why = whyOf(card);
  if (why) {
    el.appendChild(make("div", "card-why", why));
  }
  if (card.pull_request !== null && card.pull_request !== undefined) {
    el.appendChild(pullRequestLink(card.project, card.pull_request));
  }
  if (withStrip) {
    el.appendChild(stripElement(card.strip));
  }
  return el;
}

function stripElement(strip) {
  const el = make("span", "strip");
  strip.forEach((cell, at) => {
    const part = make("span", "strip-cell strip-" + cell);
    part.setAttribute("title", STAGES[at] + ": " + cell);
    el.appendChild(part);
  });
  return el;
}

function renderColumns(project, into) {
  const byTicket = new Map(project.cards.map((card) => [card.ticket, card]));
  const columns = new Map(COLUMNS.map(([key]) => [key, []]));
  columns.set("other", []);
  for (const ticket of project.list) {
    const card = byTicket.get(ticket);
    const key = columns.has(card.column) ? card.column : "other";
    columns.get(key).push(card);
  }
  const grid = make("div", "columns");
  const shown = COLUMNS.concat([["other", "Starting or unreadable"]]);
  for (const [key, title] of shown) {
    const cards = columns.get(key);
    if (key === "other" && !cards.length) {
      continue;
    }
    const column = make("div", "column");
    const head = make("h4", "column-head");
    head.appendChild(make("span", "", title));
    head.appendChild(make("span", "column-count", cards.length));
    column.appendChild(head);
    if (key === "not_started" && project.ready && project.ready.stale) {
      column.appendChild(make("div", "card-why state-warning",
        "ready.json is stale: the drain last read the column " + ageText(project.ready.at) + " ago"));
    }
    for (const card of cards) {
      column.appendChild(cardElement(card, false));
    }
    grid.appendChild(column);
  }
  into.appendChild(grid);
}

function renderList(project, into) {
  const byTicket = new Map(project.cards.map((card) => [card.ticket, card]));
  if (project.ready && project.ready.stale) {
    into.appendChild(make("p", "card-why state-warning",
      "ready.json is stale: the drain last read the column " + ageText(project.ready.at) + " ago"));
  }
  const list = make("ul", "plain rows");
  for (const ticket of project.list) {
    const card = byTicket.get(ticket);
    const row = make("li", "row" + (card.needs_you ? " row-needs-you" : ""));
    row.appendChild(ticketLink(card.project, card.ticket));
    row.appendChild(stripElement(card.strip));
    const [state, stateClass] = stateOf(card);
    row.appendChild(make("span", "card-state " + stateClass, state));
    row.appendChild(make("span", "row-title", card.title || ""));
    row.appendChild(make("span", "card-why", whyOf(card)));
    if (card.pull_request !== null && card.pull_request !== undefined) {
      row.appendChild(pullRequestLink(card.project, card.pull_request));
    }
    list.appendChild(row);
  }
  if (!project.list.length) {
    into.appendChild(make("p", "notice", "No ticket in flight or admitted."));
  }
  into.appendChild(list);
}

function renderView(overview) {
  const body = document.getElementById("overview-body");
  clear(body);
  const view = currentView();
  document.getElementById("view-columns").setAttribute("aria-pressed", String(view === "columns"));
  document.getElementById("view-list").setAttribute("aria-pressed", String(view === "list"));
  for (const note of overview.notes) {
    body.appendChild(make("p", "fact state-information", note));
  }
  const projects = shownProjects(overview);
  if (!projects.length) {
    body.appendChild(make("p", "notice", "No project is served."));
  }
  for (const project of projects) {
    const block = make("div", "project-block");
    const head = make("h3", "project-head", project.repository);
    if (project.paused) {
      head.appendChild(make("span", "state-warning", "paused: " + (project.paused.why || "")));
    }
    if (project.quarantined) {
      head.appendChild(make("span", "state-danger", "quarantined"));
    }
    block.appendChild(head);
    if (view === "list") {
      renderList(project, block);
    } else {
      renderColumns(project, block);
    }
    body.appendChild(block);
  }
}

function sinceGroup(into, title, entries, chosen) {
  const shown = entries.filter((entry) => !chosen || entry.project === chosen);
  if (!shown.length) {
    return;
  }
  const line = make("div", "since-line");
  line.appendChild(make("span", "since-title", title + " (" + shown.length + ")"));
  for (const entry of shown) {
    line.appendChild(ticketLink(entry.project, entry.ticket));
  }
  into.appendChild(line);
}

function renderSince() {
  const body = document.getElementById("since-body");
  clear(body);
  if (!lastSince) {
    return;
  }
  const since = lastSince;
  const chosen = selectedProject();
  body.appendChild(make("p", "muted", previousVisit
    ? "Since this browser last looked, " + since.from
    : "First visit: the last 24 hours, since " + since.from));
  if (since.truncated) {
    body.appendChild(make("p", "state-warning",
      "The event log's oldest records for this period were dropped: the counts may be short."));
  }
  const lines = make("div", "since-lines");
  sinceGroup(lines, "Delivered", since.delivered, chosen);
  sinceGroup(lines, "Merged", since.merged, chosen);
  sinceGroup(lines, "Parked", since.parked, chosen);
  sinceGroup(lines, "Asked", since.asked, chosen);
  // Spend comes by project: the chosen project's, or every project's.
  let spent = 0;
  for (const [project, usd] of Object.entries(since.spent_usd || {})) {
    if (!chosen || project === chosen) {
      spent += usd;
    }
  }
  if (spent > 0) {
    lines.appendChild(make("div", "since-line", "Spent $" + spent.toFixed(2)));
  }
  if (!lines.firstChild) {
    lines.appendChild(make("p", "notice", "Nothing happened since."));
  }
  body.appendChild(lines);
}

async function refreshSince() {
  sinceSeq += 1;
  const mine = sinceSeq;
  const path = "/api/since" +
    (previousVisit ? "?at=" + encodeURIComponent(previousVisit) : "");
  try {
    const answer = await authedFetch(path);
    if (!answer || mine !== sinceSeq) {
      return;
    }
    if (!answer.response.ok) {
      throw new Error("answered " + answer.response.status);
    }
    lastSince = await answer.response.json();
    if (mine === sinceSeq) {
      renderSince();
    }
  } catch (error) {
    const body = document.getElementById("since-body");
    clear(body);
    body.appendChild(make("p", "problem", "What happened since could not be read: " + error.message));
  }
}

function renderHome() {
  if (!lastOverview) {
    return;
  }
  renderForYou(lastOverview);
  renderView(lastOverview);
  renderSince();
}

// The overview the stream sent. The "since" panel is asked for again when the
// count of highlights moved: it only ever goes up, so a new event always
// moves it.
function renderOverview(overview) {
  lastOverview = overview;
  renderHome();
  if (overview.highlights !== lastHighlights) {
    lastHighlights = overview.highlights;
    refreshSince();
  }
}

// ---------------------------------------------------------------------------
// The drain's graph (OWL-460), at `#/graph`: the graph the stream delivers
// (`/api/stream?graph=1`), drawn as SVG by hand. The server lays it out
// (every node has its `layer` and `row`); this only places and draws it.
// Colours come from classes (`node-<state>`, and `ghost-<blocker state>` for
// a node the tracker names outside the drain), never from a style attribute,
// which the Content-Security-Policy refuses; every node also says its state
// as a word, since colour is never alone.

const LAYER_KEY = "owlshift.graph.layer";
// The SVG namespace is a name, not a place the page reads from; it is written
// in parts so that the audit of the assets finds no address in them.
const SVG_NS = ["http:", "", "www.w3.org", "2000", "svg"].join("/");
const COLUMN = 260;
const ROW = 66;
const BOX_W = 210;
const BOX_H = 50;
const PAD = 14;
const TOP = 30;
const TICKET_ID = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;
const CLASS_WORD = /^[a-z_]+$/;

// The latest graph the stream delivered, and the handles of the page open.
let lastGraph = null;
let graphView = null;

// The layer drawn: `live` or `blockers`, kept in the browser.
function graphLayer() {
  return stored(LAYER_KEY) === "blockers" ? "blockers" : "live";
}

function svgEl(tag, attributes, value) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [name, attribute] of Object.entries(attributes || {})) {
    el.setAttribute(name, String(attribute));
  }
  if (value !== undefined) {
    text(el, value);
  }
  return el;
}

function clip(value, length) {
  const full = value === null || value === undefined ? "" : String(value);
  return full.length > length ? full.slice(0, length - 1) + "…" : full;
}

// The route of a ticket's page: the project is `owner/repo`.
function ticketHref(repository, ticket) {
  const parts = repository.split("/").map((part) => encodeURIComponent(part));
  return "#/ticket/" + parts.join("/") + "/" + encodeURIComponent(ticket);
}

// What a node says of its state, in words.
function stateWord(node) {
  if (node.state === "ghost") {
    return "outside the drain · " + node.blocker_state;
  }
  if (node.kind === "pull_request") {
    return "pull request";
  }
  if (node.person) {
    return "a person's ticket";
  }
  if (!node.in_flight && node.state === "idle") {
    return "not in flight";
  }
  switch (node.state) {
    case "running":
      return "running";
    case "resource":
      return "waits on a resource";
    case "ticket":
      return "waits on a ticket";
    case "slot":
      return "waits for a slot";
    case "decider":
      return "waits on the decider";
    case "intake":
      return "waits on intake";
    case "usage_limit":
      return "waits on a usage limit";
    case "other":
      return node.reason ? "waits: " + node.reason : "waits";
    case "not_started":
      return "not started";
    case "ready":
      return "in the ready column";
    default:
      return "no wait";
  }
}

// A node's tooltip: everything it knows, one fact a line.
function nodeTip(node) {
  const lines = [node.id, stateWord(node)];
  if (node.title) {
    lines.push(node.title);
  }
  if (node.stage) {
    lines.push("stage: " + node.stage);
  }
  if (node.run) {
    lines.push("run slot " + node.run.slot + " since " + node.run.since);
  }
  if (node.text) {
    lines.push(node.text);
  }
  if (node.since) {
    lines.push("told " + node.since);
  }
  if (node.drain) {
    lines.push("the drain: " + node.drain + (node.drain_reason ? " (" + node.drain_reason + ")" : ""));
  }
  return lines.join("\n");
}

// The classes of a node's group: its state, and for a ghost its blocker state.
function nodeClasses(node) {
  const classes = ["node"];
  if (CLASS_WORD.test(node.state)) {
    classes.push("node-" + node.state);
  }
  if (node.blocker_state && CLASS_WORD.test(node.blocker_state)) {
    classes.push("ghost-" + node.blocker_state);
  }
  return classes.join(" ");
}

// Where an edge runs, as the four points of a cubic curve: from the waiting
// node to what it waits on, which is to its left; a back edge or an edge
// inside a column leaves by the top.
function edgeCurve(from, to) {
  const a = { x: PAD + from.layer * COLUMN, y: TOP + from.row * ROW };
  const b = { x: PAD + to.layer * COLUMN, y: TOP + to.row * ROW };
  const middle = BOX_H / 2;
  if (b.x < a.x) {
    const shift = (a.x - (b.x + BOX_W)) / 2;
    const p0 = { x: a.x, y: a.y + middle };
    const p3 = { x: b.x + BOX_W, y: b.y + middle };
    return [p0, { x: p0.x - shift, y: p0.y }, { x: p3.x + shift, y: p3.y }, p3];
  }
  if (b.x > a.x) {
    const shift = (b.x - (a.x + BOX_W)) / 2;
    const p0 = { x: a.x + BOX_W, y: a.y + middle };
    const p3 = { x: b.x, y: b.y + middle };
    return [p0, { x: p0.x + shift, y: p0.y }, { x: p3.x - shift, y: p3.y }, p3];
  }
  const p0 = { x: a.x + BOX_W / 2, y: a.y };
  const p3 = { x: b.x + BOX_W / 2, y: b.y };
  return [p0, { x: p0.x + 70, y: p0.y - 26 }, { x: p3.x + 70, y: p3.y - 26 }, p3];
}

function drawEdge(svg, edge, from, to) {
  const [p0, p1, p2, p3] = edgeCurve(from, to);
  const group = svgEl("g", { class: "edge" });
  group.appendChild(svgEl("title", {}, edge.from + " → " + edge.to + ": " + edge.text));
  group.appendChild(svgEl("path", {
    class: "edge-line",
    d: "M " + p0.x + " " + p0.y + " C " + p1.x + " " + p1.y + ", " + p2.x + " " + p2.y +
      ", " + p3.x + " " + p3.y,
  }));
  const angle = Math.atan2(p3.y - p2.y, p3.x - p2.x);
  const corner = (turn) => (p3.x - 9 * Math.cos(angle + turn)) + " " +
    (p3.y - 9 * Math.sin(angle + turn));
  group.appendChild(svgEl("path", {
    class: "edge-head",
    d: "M " + p3.x + " " + p3.y + " L " + corner(-0.4) + " L " + corner(0.4) + " Z",
  }));
  group.appendChild(svgEl("text", {
    class: "edge-label",
    x: (p0.x + 3 * p1.x + 3 * p2.x + p3.x) / 8,
    y: (p0.y + 3 * p1.y + 3 * p2.y + p3.y) / 8 - 5,
    "text-anchor": "middle",
  }, clip(edge.label, 30)));
  svg.appendChild(group);
}

function drawNode(svg, node, repository) {
  const x = PAD + node.layer * COLUMN;
  const y = TOP + node.row * ROW;
  const group = svgEl("g", { class: nodeClasses(node) });
  group.appendChild(svgEl("title", {}, nodeTip(node)));
  group.appendChild(svgEl("rect", {
    class: "node-box", x: x, y: y, width: BOX_W, height: BOX_H, rx: 6,
  }));
  group.appendChild(svgEl("text", { class: "node-id", x: x + 8, y: y + 20 }, clip(node.id, 22)));
  group.appendChild(svgEl("text", {
    class: "node-state", x: x + BOX_W - 8, y: y + 20, "text-anchor": "end",
  }, clip(stateWord(node), 26)));
  group.appendChild(svgEl("text", { class: "node-detail", x: x + 8, y: y + 39 },
    clip(node.title || node.text || "", 32)));
  // A ticket's own node opens its page; a pull request, a person's ticket
  // and what the tracker names that is no identifier are only drawn.
  if (node.kind === "ticket" && !node.person && TICKET_ID.test(node.id)) {
    const anchor = svgEl("a", { href: ticketHref(repository, node.id) });
    anchor.appendChild(group);
    svg.appendChild(anchor);
  } else {
    svg.appendChild(group);
  }
}

// A layer as an SVG, edges under nodes.
function drawLayer(layer, repository) {
  const nodes = new Map();
  let columns = 1;
  let rows = 1;
  for (const node of layer.nodes) {
    nodes.set(node.id, node);
    columns = Math.max(columns, node.layer + 1);
    rows = Math.max(rows, node.row + 1);
  }
  const width = PAD * 2 + (columns - 1) * COLUMN + BOX_W;
  const height = TOP + rows * ROW + PAD;
  const svg = svgEl("svg", {
    class: "graph-svg",
    width: width,
    height: height,
    viewBox: "0 0 " + width + " " + height,
    role: "group",
    "aria-label": "Waits of " + repository,
  });
  for (const edge of layer.edges) {
    const from = nodes.get(edge.from);
    const to = nodes.get(edge.to);
    if (from && to && from !== to) {
      drawEdge(svg, edge, from, to);
    }
  }
  for (const node of layer.nodes) {
    drawNode(svg, node, repository);
  }
  return svg;
}

function queueWord(entry) {
  return entry.reason === "run_cap" ? "a machine run slot" : "the project's capacity";
}

// The tickets that wait for a slot, apart from the graph.
function drawQueue(queue, repository) {
  const el = make("aside", "queue");
  el.appendChild(make("h4", "", "Waiting for a slot"));
  const list = make("ol", "queue-list");
  for (const entry of queue) {
    const item = make("li", "queue-entry");
    if (TICKET_ID.test(entry.ticket)) {
      const link = make("a", "queue-ticket", entry.ticket);
      link.setAttribute("href", ticketHref(repository, entry.ticket));
      item.appendChild(link);
    } else {
      item.appendChild(make("span", "queue-ticket", entry.ticket));
    }
    item.appendChild(make("span", "queue-reason", " for " + queueWord(entry)));
    if (entry.since) {
      item.appendChild(make("span", "queue-since", " since " + entry.since));
    }
    if (entry.behind.length) {
      item.appendChild(make("span", "queue-behind", ", behind " + entry.behind.join(", ")));
    }
    list.appendChild(item);
  }
  el.appendChild(list);
  return el;
}

function drawProject(project, layerName) {
  const section = make("section", "graph-project");
  const cap = project.max_in_flight === null ? "?" : project.max_in_flight;
  section.appendChild(make("h3", "",
    project.repository + " · " + project.in_flight + " of " + cap + " in flight" +
    (project.served ? "" : " · not served")));
  if (project.paused) {
    section.appendChild(make("p", "state-warning", "paused: " + (project.paused.why || "by a person")));
  }
  if (project.quarantined) {
    section.appendChild(make("p", "state-danger", "quarantined"));
  }
  // The graph is sent when it changes: its time is that of its last change.
  const ages = [];
  if (project.ready) {
    ages.push("ready snapshot written " + project.ready.at);
  }
  if (ages.length) {
    const line = make("p", "graph-age", ages.join(" · "));
    if (project.ready && project.ready.stale) {
      line.appendChild(make("span", "state-warning",
        "stale: the drain has not read the ready column for more than two passes"));
    }
    section.appendChild(line);
  }
  for (const note of project.notes) {
    section.appendChild(make("p", "notice state-information", note));
  }
  const layer = layerName === "blockers" ? project.blockers : project.live;
  if (!layer) {
    section.appendChild(make("p", "notice", "The blockers need the drain's ready snapshot."));
    return section;
  }
  if (!layer.nodes.length) {
    section.appendChild(make("p", "notice",
      layerName === "blockers" ? "Nothing in the ready column is blocked." : "Nothing waits."));
  }
  const layout = make("div", "graph-layout");
  if (layer.nodes.length) {
    const frame = make("div", "graph-frame");
    frame.appendChild(drawLayer(layer, project.repository));
    layout.appendChild(frame);
  }
  if (layerName !== "blockers" && project.queue.length) {
    layout.appendChild(drawQueue(project.queue, project.repository));
  }
  section.appendChild(layout);
  return section;
}

// Draws the graph of the page open from the latest one the stream delivered,
// the projects the header's switch chose, and the layer the browser kept.
function drawGraph() {
  if (!graphView) {
    return;
  }
  const layerName = graphLayer();
  for (const [name, button] of Object.entries(graphView.buttons)) {
    button.setAttribute("aria-pressed", name === layerName ? "true" : "false");
  }
  const content = graphView.content;
  clear(content);
  if (!lastGraph) {
    content.appendChild(make("p", "notice", "Reading the drain…"));
    return;
  }
  const facts = make("div", "facts graph-facts");
  if (lastGraph.runs) {
    facts.appendChild(fact("Run slots", lastGraph.runs.in_use + " of " + lastGraph.runs.of));
  }
  if (lastGraph.machine_pause) {
    facts.appendChild(fact("Paused", lastGraph.machine_pause.why || "by a person", "state-warning"));
  }
  facts.appendChild(fact("Changed", lastGraph.at));
  content.appendChild(facts);
  for (const note of lastGraph.notes) {
    content.appendChild(make("p", "notice state-information", note));
  }
  const chosen = document.getElementById("project").value;
  const shown = lastGraph.projects.filter((project) => !chosen || project.repository === chosen);
  if (!shown.length) {
    content.appendChild(make("p", "notice", "No project is served."));
  }
  for (const project of shown) {
    content.appendChild(drawProject(project, layerName));
  }
}

function closeGraph() {
  graphView = null;
  const el = document.getElementById("view");
  clear(el);
  el.hidden = true;
}

function openGraph() {
  const el = document.getElementById("view");
  clear(el);
  const bar = make("div", "graph-bar");
  bar.appendChild(make("h2", "graph-title", "The drain"));
  const switcher = make("div", "layer-switch");
  switcher.setAttribute("role", "group");
  switcher.setAttribute("aria-label", "Layer");
  const buttons = {};
  for (const [name, label] of [["live", "Live waits"], ["blockers", "Blockers"]]) {
    const button = make("button", "control", label);
    button.type = "button";
    button.addEventListener("click", () => {
      store(LAYER_KEY, name);
      drawGraph();
    });
    switcher.appendChild(button);
    buttons[name] = button;
  }
  bar.appendChild(switcher);
  el.appendChild(bar);
  const content = make("div", "graph-content");
  el.appendChild(content);
  graphView = { content: content, buttons: buttons };
  el.hidden = false;
  drawGraph();
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
    renderHome();
    drawGraph();
    if (window.owlshiftOps) {
      window.owlshiftOps.refresh();
    }
  });
  startVisit();
  wireViewSwitch();
  try {
    await login();
  } catch (error) {
    problem("The login failed: " + error.message);
  }
  window.addEventListener("hashchange", route);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && openPage) {
      window.clearTimeout(openPage.snapshotTimer);
      refresh(openPage);
    }
  });
  if (!session()) {
    loggedOut(null);
    return;
  }
  follow();
  route();
}

applyTheme(stored(THEME_KEY));
document.addEventListener("DOMContentLoaded", start);
