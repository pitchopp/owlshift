// The operations screens of the web UI (OWL-472): the Orchestrator, Spend and
// Config, at `#/orchestrator`, `#/spend` and `#/config`. They draw what
// `owlshift status`, `logs`, `doctor` and `config show` tell, read-only.
// Every text reaches the page through `make` and `text` of app.js, which set
// `textContent`: nothing an agent, a model or a comment wrote is read as
// markup. One timer refreshes the screen shown, and stops when it is left:
// the Orchestrator every 2 s with the cursors it was given, Spend and Config
// every 10 s with the `rev` of the answer they hold, which answers
// `unchanged` when nothing changed so nothing is redrawn.
"use strict";

(function () {
  const ORCHESTRATOR_MS = 2000;
  const SLOW_MS = 10000;
  const FEED_KEPT = 500;
  const LOG_KEPT = 1000;
  const LOG_CHARS = 4096;

  // The screen open: `epoch` tells a callback that outlived it.
  let epoch = 0;
  let timer = null;
  let current = null;

  function root() {
    return document.getElementById("ops");
  }

  function chosenProject() {
    const select = document.getElementById("project");
    return select ? select.value : "";
  }

  function sameProject(name) {
    const chosen = chosenProject();
    return !chosen || String(name || "").toLowerCase() === chosen.toLowerCase();
  }

  function usd(micros) {
    const amount = (micros || 0) / 1000000;
    return "$" + amount.toFixed(amount !== 0 && amount < 1 ? 4 : 2);
  }

  function clock(iso) {
    if (!iso) {
      return "";
    }
    const date = new Date(iso);
    return isNaN(date.getTime()) ? String(iso) : date.toLocaleString();
  }

  function percent(share) {
    return Math.round(share * 100) + " %";
  }

  // A request with the session. `body` makes it a POST of JSON. A 401 ends
  // the session on this page, as app.js does for its own requests.
  async function request(path, body) {
    const own = session();
    if (!own) {
      loggedOut(null);
      return null;
    }
    const sent = sessionHeader(own);
    const headers = {};
    headers[SESSION_HEADER] = sent;
    const options = { headers: headers, credentials: "omit", cache: "no-store" };
    if (body !== undefined) {
      options.method = "POST";
      options.headers["Content-Type"] = "application/json";
      options.body = JSON.stringify(body);
    }
    const response = await fetch(path, options);
    if (response.status === 401) {
      loggedOut(sent);
      return null;
    }
    let value = null;
    try {
      value = await response.json();
    } catch (_) {
      value = null;
    }
    return { status: response.status, value: value };
  }

  function section(title) {
    const el = make("section", "section");
    el.appendChild(make("h3", "", title));
    const body = make("div", "section-body");
    el.appendChild(body);
    return { el: el, body: body };
  }

  function stateClass(status) {
    switch (status) {
      case "pass":
      case "ok":
        return "state-success";
      case "warn":
      case "warning":
        return "state-warning";
      case "fail":
        return "state-danger";
      default:
        return "state-information";
    }
  }

  function lineBlock(lines) {
    const pre = make("pre", "pre-text");
    text(pre, lines.join("\n"));
    return pre;
  }

  function note(parent, message) {
    parent.appendChild(make("p", "notice", message));
  }

  // -------------------------------------------------------------------------
  // The Orchestrator.

  function orchestratorState() {
    return {
      gen: null,
      after: null,
      log: null,
      entries: new Map(),
      logLines: [],
      busy: false,
      parts: {},
      lastDoctor: null,
    };
  }

  function orchestratorPath(state) {
    const query = [];
    if (state.gen !== null && state.after !== null) {
      query.push("gen=" + state.gen);
      query.push("after=" + state.after);
    }
    if (state.log !== null) {
      query.push("log=" + state.log);
    }
    return "/api/orchestrator" + (query.length ? "?" + query.join("&") : "");
  }

  // The status lines of the project chosen: a project's block runs from its
  // heading over the indented kinds that follow it.
  function visibleLines(answer) {
    const chosen = chosenProject().toLowerCase();
    const out = [];
    let inBlock = false;
    let skipping = false;
    for (let at = 0; at < answer.lines.length; at += 1) {
      const part = answer.line_parts[at];
      const line = answer.lines[at];
      if (part === "project") {
        inBlock = true;
        skipping = chosen !== "" && !line.toLowerCase().startsWith(chosen + " ");
      } else if (!["record", "watch", "project_pause", "quarantine", "draining"].includes(part)) {
        inBlock = false;
        skipping = false;
      }
      if (!(inBlock && skipping)) {
        out.push(line);
      }
    }
    return out;
  }

  function drawStatus(state, answer) {
    const body = state.parts.status;
    clear(body);
    body.appendChild(lineBlock(visibleLines(answer)));
    for (const message of answer.notes || []) {
      body.appendChild(make("p", "notice state-information", message));
    }
    note(body, "Left to other screens: " + answer.left_out.join(", ") + ".");
  }

  function drawUsage(state, answer) {
    const body = state.parts.usage;
    clear(body);
    for (const harness of answer.usage) {
      const head = make("div", "usage-head");
      head.appendChild(make("strong", "", harness.name));
      head.appendChild(make("span", "muted",
        harness.cap_percent === null ? " no usage cap" : " cap " + harness.cap_percent + " %"));
      if (harness.standing && harness.standing.state === "limited") {
        head.appendChild(make("span", "state-warning",
          "at its limit" + (harness.standing.until ? " until " + clock(harness.standing.until) : "")));
      }
      body.appendChild(head);
      if (harness.windows.length === 0) {
        note(body, "No window recorded.");
      }
      for (const window_ of harness.windows) {
        const row = make("div", "entry");
        row.appendChild(make("span", "entry-stage", window_.name));
        row.appendChild(make("span", "", percent(window_.share)));
        row.appendChild(make("span", "entry-at", "resets " + clock(window_.resets_at)));
        row.appendChild(make("span", "entry-at", "read " + clock(window_.read_at)));
        if (!window_.under_way) {
          row.appendChild(make("span", "muted", "reset"));
        } else if (window_.holds) {
          row.appendChild(make("span", "state-warning", "holds the harness"));
        } else {
          row.appendChild(make("span", "state-success", "free"));
        }
        body.appendChild(row);
      }
    }
  }

  function entryElement(entry) {
    const row = make("li", "entry");
    if (entry.raw !== undefined) {
      row.appendChild(make("span", "entry-at", "not an event"));
      row.appendChild(make("code", "pre-text", entry.raw));
      return row;
    }
    row.appendChild(make("span", "entry-at", clock(entry.at)));
    row.appendChild(make("span", entry.kind === "warning" ? "state-warning" : "entry-stage", entry.kind));
    row.appendChild(make("span", "", entry.project));
    row.appendChild(make("span", "", entry.ticket || "-"));
    row.appendChild(make("code", "pre-text", entry.line));
    return row;
  }

  function drawFeed(state) {
    const list = state.parts.feed;
    clear(list);
    const offsets = Array.from(state.entries.keys()).sort((a, b) => b - a);
    let shown = 0;
    for (const offset of offsets) {
      const entry = state.entries.get(offset);
      if (entry.raw === undefined && !sameProject(entry.project)) {
        continue;
      }
      list.appendChild(entryElement(entry));
      shown += 1;
    }
    if (shown === 0) {
      list.appendChild(make("li", "notice", "No decision or warning yet."));
    }
  }

  function takeEvents(state, events) {
    if (events.reset) {
      state.entries.clear();
    }
    for (const entry of events.entries) {
      state.entries.set(entry.offset, entry);
    }
    const offsets = Array.from(state.entries.keys()).sort((a, b) => a - b);
    while (offsets.length > FEED_KEPT) {
      state.entries.delete(offsets.shift());
    }
    state.gen = events.generation;
    if (events.cursor !== null) {
      state.after = events.cursor;
    } else if (events.reset) {
      state.after = null;
    }
  }

  function drawLog(state, log) {
    const pre = state.parts.log;
    if (log.absent) {
      text(pre, log.absent);
      state.log = null;
      return;
    }
    if (log.reset) {
      state.logLines = [];
    }
    for (const line of log.lines) {
      state.logLines.push(line.length > LOG_CHARS ? line.slice(0, LOG_CHARS) + "…" : line);
    }
    if (state.logLines.length > LOG_KEPT) {
      state.logLines.splice(0, state.logLines.length - LOG_KEPT);
    }
    state.log = log.next;
    text(pre, state.logLines.length ? state.logLines.join("\n") : "The daemon's log is empty.");
  }

  function drawChecks(parent, entry) {
    if (entry.error) {
      parent.appendChild(make("p", "state-danger", entry.error));
      return;
    }
    const report = entry.report;
    parent.appendChild(make("p", report.ready ? "state-success" : "state-danger",
      report.ready ? "Ready" : report.problems + " problem" + (report.problems === 1 ? "" : "s")));
    for (const check of report.checks) {
      const row = make("div", "entry");
      row.appendChild(make("span", stateClass(check.status), check.status));
      row.appendChild(make("span", "entry-stage", check.section));
      row.appendChild(make("span", "", check.subject));
      row.appendChild(make("span", "pre-text", check.detail));
      if (check.status !== "pass" && check.why) {
        row.appendChild(make("span", "muted pre-text", check.why));
      }
      for (const step of check.fix || []) {
        row.appendChild(make("code", "pre-text", step.run !== undefined ? step.run : step.do));
      }
      parent.appendChild(row);
    }
  }

  async function runDoctor(state, project, button) {
    button.disabled = true;
    const results = state.parts.checks;
    clear(results);
    note(results, "Running the checks…");
    let answer = null;
    try {
      answer = await request("/api/doctor", project ? { project: project } : {});
    } catch (error) {
      answer = { status: 0, value: { error: error.message } };
    }
    button.disabled = false;
    if (!answer || !current || current.state !== state) {
      return;
    }
    clear(results);
    if (answer.status !== 200) {
      results.appendChild(make("p", "state-danger",
        (answer.value && answer.value.error) || "The checks did not run."));
      return;
    }
    for (const entry of answer.value.reports) {
      results.appendChild(make("h4", "", entry.project || "The personal file alone"));
      drawChecks(results, entry);
    }
  }

  function buildOrchestrator(state, el) {
    const status = section("Daemon and watches");
    const usage = section("Usage windows");
    const checks = section("Checks");
    const feed = section("Decisions and warnings");
    const log = section("The daemon's log");
    state.parts.status = status.body;
    state.parts.usage = usage.body;
    state.parts.checks = checks.body;
    state.parts.feed = make("ul", "timeline");
    feed.body.appendChild(state.parts.feed);
    state.parts.log = make("pre", "pre-text log");
    log.body.appendChild(state.parts.log);
    const bar = make("div", "section-head");
    const all = make("button", "toggle", "Run the checks");
    all.type = "button";
    all.addEventListener("click", () => runDoctor(state, chosenProject(), all));
    bar.appendChild(all);
    bar.appendChild(make("span", "muted",
      "Runs owlshift doctor on demand, once per served project; it never hands the daemon over."));
    checks.el.insertBefore(bar, checks.body);
    state.parts.checks.appendChild(make("p", "notice", "Nothing run yet."));
    for (const part of [status, usage, checks, feed, log]) {
      el.appendChild(part.el);
    }
  }

  async function tickOrchestrator(state, mine) {
    if (state.busy) {
      return;
    }
    state.busy = true;
    try {
      const answer = await request(orchestratorPath(state));
      if (answer && mine === epoch) {
        if (answer.status === 200) {
          problem("");
          const value = answer.value;
          state.last = value;
          drawStatus(state, value);
          drawUsage(state, value);
          takeEvents(state, value.events);
          drawFeed(state);
          drawLog(state, value.log);
        } else {
          problem((answer.value && answer.value.error) || "The Orchestrator did not answer.");
        }
      }
    } catch (error) {
      problem("The Orchestrator stopped: " + error.message);
    }
    state.busy = false;
  }

  // -------------------------------------------------------------------------
  // Spend.

  function spendState() {
    return { days: stored("owlshift.spendDays") || "30", rev: null, key: null, busy: false, parts: {} };
  }

  function spendPath(state) {
    const query = ["days=" + encodeURIComponent(state.days)];
    const project = chosenProject();
    if (project) {
      query.push("project=" + encodeURIComponent(project));
    }
    // The rev only goes back with the same question: another period or
    // project is another answer.
    const key = query.join("&");
    if (state.rev !== null && state.key === key) {
      query.push("rev=" + encodeURIComponent(state.rev));
    }
    return { path: "/api/spend?" + query.join("&"), key: key };
  }

  function table(headings, rows) {
    const el = make("table", "table");
    const head = make("tr");
    for (const heading of headings) {
      head.appendChild(make("th", "", heading));
    }
    el.appendChild(head);
    for (const row of rows) {
      const tr = make("tr");
      for (const cell of row) {
        tr.appendChild(cell instanceof Node ? cell : make("td", "", cell));
      }
      el.appendChild(tr);
    }
    return el;
  }

  function cell(child) {
    const td = make("td");
    td.appendChild(child);
    return td;
  }

  function budgetBar(counted, budget) {
    const wrap = make("span", "budget-cell");
    const bar = make("progress", "budget");
    bar.max = budget;
    bar.value = Math.min(counted, budget);
    wrap.appendChild(bar);
    const over = counted >= budget;
    wrap.appendChild(make("span", over ? "state-danger" : "muted",
      usd(counted) + " of " + usd(budget)));
    return wrap;
  }

  function drawSpend(state, answer) {
    state.rev = answer.rev;
    const parts = state.parts;
    for (const key of ["total", "days", "tickets", "roles", "harnesses", "usage"]) {
      clear(parts[key]);
    }
    note(parts.total, "The last " + (answer.period === "all" ? "recorded history" : answer.period + " days") +
      " by the usage events: " + usd(answer.events.total_micros) + " in " + answer.events.records +
      " runs reported. A run whose harness reports no cost adds nothing.");
    for (const message of answer.notes) {
      parts.total.appendChild(make("p", "notice state-information", message));
    }
    parts.days.appendChild(table(["Day", "Cost"],
      answer.days.map((day) => [day.day, usd(day.micros)])));
    parts.tickets.appendChild(table(
      ["Ticket", "Project", "Events in the period", "Every run (cost.json)", "Runs", "Budget"],
      answer.tickets.map((ticket) => {
        const cost = ticket.cost;
        let budget = make("td", "muted", "");
        if (cost && cost.budget_micros !== null) {
          budget = cell(budgetBar(cost.counted_micros, cost.budget_micros));
        } else if (cost) {
          budget = make("td", "muted", "no budget");
        }
        return [
          cell(make("a", "", ticket.ticket)),
          ticket.project,
          usd(ticket.events_micros),
          cost ? usd(cost.spent_micros) : "",
          cost ? cost.runs + (cost.unreported_runs ? " (" + cost.unreported_runs + " without a cost)" : "") : "",
          budget,
        ];
      })));
    answer.tickets.forEach((ticket, at) => {
      const link = parts.tickets.querySelectorAll("tr")[at + 1].querySelector("a");
      link.href = "#/ticket/" + ticket.project.split("/").map(encodeURIComponent).join("/") +
        "/" + encodeURIComponent(ticket.ticket);
    });
    parts.roles.appendChild(table(["Role", "Cost"],
      answer.roles.map((role) => [role.role, usd(role.micros)])));
    parts.harnesses.appendChild(table(["Harness", "Cost"],
      answer.harnesses.map((harness) => [harness.harness, usd(harness.micros)])));
    for (const harness of answer.usage) {
      const head = make("div", "usage-head");
      head.appendChild(make("strong", "", harness.name));
      head.appendChild(make("span", "muted",
        harness.cap_percent === null ? " no usage cap" : " cap " + harness.cap_percent + " %"));
      parts.usage.appendChild(head);
      for (const window_ of harness.windows) {
        const row = make("div", "entry");
        row.appendChild(make("span", "entry-stage", window_.name));
        row.appendChild(make("span", "", percent(window_.share)));
        row.appendChild(make("span", "entry-at", "resets " + clock(window_.resets_at)));
        if (window_.under_way && window_.holds) {
          row.appendChild(make("span", "state-warning", "holds the harness"));
        }
        parts.usage.appendChild(row);
      }
    }
  }

  function buildSpend(state, el) {
    const bar = make("div", "section-head");
    bar.appendChild(make("span", "switch-label", "Period"));
    const select = make("select", "control");
    select.setAttribute("aria-label", "Period");
    for (const [value, label] of [["7", "7 days"], ["30", "30 days"], ["90", "90 days"], ["all", "All recorded history"]]) {
      const option = make("option", "", label);
      option.value = value;
      select.appendChild(option);
    }
    select.value = state.days;
    select.addEventListener("change", () => {
      state.days = select.value;
      store("owlshift.spendDays", state.days);
      state.rev = null;
      tick();
    });
    bar.appendChild(select);
    el.appendChild(bar);
    for (const [key, title] of [
      ["total", "Total"],
      ["days", "Per day"],
      ["tickets", "Per ticket"],
      ["roles", "Per role"],
      ["harnesses", "Per harness"],
      ["usage", "Usage caps and windows"],
    ]) {
      const part = section(title);
      state.parts[key] = part.body;
      el.appendChild(part.el);
    }
  }

  async function tickSpend(state, mine) {
    if (state.busy) {
      return;
    }
    state.busy = true;
    try {
      const asked = spendPath(state);
      const answer = await request(asked.path);
      if (answer && mine === epoch) {
        if (answer.status !== 200) {
          problem((answer.value && answer.value.error) || "Spend did not answer.");
        } else if (!answer.value.unchanged) {
          problem("");
          state.key = asked.key;
          drawSpend(state, answer.value);
        }
      }
    } catch (error) {
      problem("Spend stopped: " + error.message);
    }
    state.busy = false;
  }

  // -------------------------------------------------------------------------
  // Config.

  function configState() {
    return { rev: null, key: null, busy: false, parts: {} };
  }

  function drawRecord(parent, record) {
    if (record.error) {
      parent.appendChild(make("p", "state-danger", record.error));
      return;
    }
    if (record.kept) {
      parent.appendChild(make("p", "", "Runs on " + record.kept.name + ", branch " +
        record.kept.branch + ", commit " + record.kept.commit.slice(0, 12) + ", since " +
        clock(record.kept.since) + "."));
    } else {
      parent.appendChild(make("p", "state-warning", "No project file kept yet."));
    }
    if (record.refused) {
      parent.appendChild(make("p", "state-warning", "The latest file is refused since " +
        clock(record.refused.since) + ": " + record.refused.reason));
    }
  }

  function drawConfig(state, answer) {
    state.rev = answer.rev;
    const body = state.parts.config;
    clear(body);
    const machine = section("The machine");
    machine.body.appendChild(lineBlock(answer.machine.lines));
    body.appendChild(machine.el);
    const chosen = chosenProject();
    for (const project of answer.projects) {
      if (!sameProject(project.repository)) {
        continue;
      }
      const part = section(project.repository);
      drawRecord(part.body, project.record || {});
      if (project.refusal) {
        part.body.appendChild(make("p", "state-danger", project.refusal));
      }
      if (project.config) {
        part.body.appendChild(lineBlock(project.config.lines));
      }
      body.appendChild(part.el);
    }
    if (chosen && answer.projects.every((project) => !sameProject(project.repository))) {
      note(body, "The project chosen is not served.");
    }
  }

  function buildConfig(state, el) {
    const body = make("div");
    state.parts.config = body;
    el.appendChild(body);
  }

  async function tickConfig(state, mine) {
    if (state.busy) {
      return;
    }
    state.busy = true;
    try {
      const path = "/api/config" + (state.rev !== null ? "?rev=" + encodeURIComponent(state.rev) : "");
      const answer = await request(path);
      if (answer && mine === epoch) {
        if (answer.status !== 200) {
          problem((answer.value && answer.value.error) || "Config did not answer.");
        } else if (!answer.value.unchanged) {
          problem("");
          state.last = answer.value;
          drawConfig(state, answer.value);
        }
      }
    } catch (error) {
      problem("Config stopped: " + error.message);
    }
    state.busy = false;
  }

  // -------------------------------------------------------------------------
  // The screen open.

  const SCREENS = {
    orchestrator: { title: "Orchestrator", state: orchestratorState, build: buildOrchestrator, tick: tickOrchestrator, every: ORCHESTRATOR_MS },
    spend: { title: "Spend", state: spendState, build: buildSpend, tick: tickSpend, every: SLOW_MS },
    config: { title: "Config", state: configState, build: buildConfig, tick: tickConfig, every: SLOW_MS },
  };

  function tick() {
    if (!current) {
      return;
    }
    const mine = epoch;
    const screen = current.screen;
    window.clearTimeout(timer);
    screen.tick(current.state, mine).then(() => {
      if (mine === epoch) {
        timer = window.setTimeout(tick, screen.every);
      }
    });
  }

  function hide() {
    epoch += 1;
    window.clearTimeout(timer);
    timer = null;
    current = null;
    const el = root();
    if (el) {
      clear(el);
      el.hidden = true;
    }
  }

  function open(name) {
    hide();
    const screen = SCREENS[name];
    if (!screen) {
      return;
    }
    epoch += 1;
    const el = root();
    clear(el);
    el.appendChild(make("h2", "ticket-title", screen.title));
    const state = screen.state();
    current = { name: name, screen: screen, state: state };
    screen.build(state, el);
    el.hidden = false;
    tick();
  }

  // The project switch changed: what the screen shows is filtered again.
  function refresh() {
    if (!current) {
      return;
    }
    const state = current.state;
    if (current.name === "orchestrator" && state.last) {
      drawStatus(state, state.last);
      drawFeed(state);
    } else if (current.name === "config" && state.last) {
      drawConfig(state, state.last);
    } else {
      state.rev = null;
      tick();
    }
  }

  window.owlshiftOps = { show: open, hide: hide, refresh: refresh };
})();
