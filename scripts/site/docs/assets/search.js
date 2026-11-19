// The documentation's search (OWL-607; build plan, "The public site", "The
// documentation", "Search"). The box stays hidden until this script shows it.
// The index, search-index.js beside this script, is loaded as a script the
// first time the box is used: the page's policy refuses fetch. A query is
// lower-cased and split into words at whatever is not a letter or a digit;
// a section matches when it holds every word; the sections with a word in
// their heading come first, then the others, each in the pages' order; the
// first ten are listed. Results are built from text nodes, never as HTML.
(function () {
  "use strict";

  var MAX = 10;
  // Read now: document.currentScript is null once the page has loaded.
  var script = document.currentScript;
  var base = script ? script.src : document.baseURI;
  var state = "idle"; // idle, loading, ready or failed
  var waiting = [];

  function load(then) {
    if (state === "ready" || state === "failed") {
      then();
      return;
    }
    waiting.push(then);
    if (state !== "idle") {
      return;
    }
    state = "loading";
    var tag = document.createElement("script");
    tag.src = new URL("search-index.js", base).href;
    tag.addEventListener("load", function () {
      state = Array.isArray(window.owlshiftSearchIndex) ? "ready" : "failed";
      flush();
    });
    tag.addEventListener("error", function () {
      state = "failed";
      flush();
    });
    document.head.appendChild(tag);
  }

  function flush() {
    var calls = waiting;
    waiting = [];
    for (var i = 0; i < calls.length; i += 1) {
      calls[i]();
    }
  }

  function words(query) {
    return query.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(function (word) {
      return word.length > 0;
    });
  }

  function search(query) {
    var wanted = words(query);
    var first = [];
    var then = [];
    if (wanted.length === 0) {
      return [];
    }
    var index = window.owlshiftSearchIndex;
    for (var i = 0; i < index.length && first.length < MAX; i += 1) {
      var entry = index[i];
      var heading = entry.heading.toLowerCase();
      var all = heading + " " + entry.text.toLowerCase();
      var holds = true;
      var inHeading = false;
      for (var w = 0; w < wanted.length; w += 1) {
        if (all.indexOf(wanted[w]) < 0) {
          holds = false;
          break;
        }
        if (heading.indexOf(wanted[w]) >= 0) {
          inHeading = true;
        }
      }
      if (holds) {
        (inHeading ? first : then).push(entry);
      }
    }
    return first.concat(then).slice(0, MAX);
  }

  document.addEventListener("DOMContentLoaded", function () {
    var box = document.querySelector(".search");
    if (!box) {
      return;
    }
    var input = box.querySelector(".search-box");
    var status = box.querySelector(".search-status");
    var list = box.querySelector(".search-results");

    function show() {
      var query = input.value;
      while (list.firstChild) {
        list.removeChild(list.firstChild);
      }
      status.textContent = "";
      if (words(query).length === 0) {
        return;
      }
      if (state === "failed") {
        status.textContent = "Search is unavailable.";
        return;
      }
      var found = search(query);
      if (found.length === 0) {
        status.textContent = "No section holds every word.";
        return;
      }
      for (var i = 0; i < found.length; i += 1) {
        var item = document.createElement("li");
        var link = document.createElement("a");
        link.href = new URL(found[i].page + "#" + found[i].anchor, base).href;
        link.textContent = found[i].heading;
        var page = document.createElement("span");
        page.className = "search-page";
        page.textContent = found[i].title;
        item.appendChild(link);
        item.appendChild(page);
        list.appendChild(item);
      }
    }

    input.addEventListener("focus", function () {
      load(function () {});
    });
    input.addEventListener("input", function () {
      load(show);
    });
    box.hidden = false;
  });
})();
