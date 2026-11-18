// The public site's theme switch (OWL-576): Dark, Light or Auto (the
// system's), kept in the browser's local storage. Loaded in the <head>, so
// the stored choice is applied before the page is drawn; the stylesheet
// follows the system by itself, and without this script the page is whole
// and the switch stays hidden.
(function () {
  "use strict";

  var KEY = "owlshift.theme";
  var root = document.documentElement;

  function stored() {
    try {
      return window.localStorage.getItem(KEY);
    } catch (_) {
      return null;
    }
  }

  function store(choice) {
    try {
      if (choice === "auto") {
        window.localStorage.removeItem(KEY);
      } else {
        window.localStorage.setItem(KEY, choice);
      }
    } catch (_) {
      // A browser that keeps nothing: the choice holds for this visit.
    }
  }

  function apply(choice) {
    if (choice === "light" || choice === "dark") {
      root.setAttribute("data-theme", choice);
    } else {
      root.removeAttribute("data-theme");
    }
  }

  function current() {
    var choice = root.getAttribute("data-theme");
    return choice === "light" || choice === "dark" ? choice : "auto";
  }

  apply(stored());

  document.addEventListener("DOMContentLoaded", function () {
    var buttons = document.querySelectorAll("[data-theme-choice]");
    var i;

    function press() {
      var choice = current();
      for (i = 0; i < buttons.length; i += 1) {
        buttons[i].setAttribute(
          "aria-pressed",
          String(buttons[i].getAttribute("data-theme-choice") === choice)
        );
      }
    }

    for (i = 0; i < buttons.length; i += 1) {
      buttons[i].addEventListener("click", function (event) {
        var choice = event.currentTarget.getAttribute("data-theme-choice");
        store(choice);
        apply(choice);
        press();
      });
    }
    press();

    var switches = document.querySelectorAll(".switch");
    for (i = 0; i < switches.length; i += 1) {
      switches[i].hidden = false;
    }
  });
})();
