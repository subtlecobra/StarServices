// The reputation editor. The rank badges are also the progress slider: pressing or dragging on a badge sets the rank (the
// badge) and the progress in it (where across the badge), the exact score beside them follows, and Save lights up once the
// score differs from the one the page was drawn with. A cell of the matrix opens its editor without leaving the page.
// Without this script the cells are links, and the score is typed. Nothing is built from data.
(function () {
  "use strict";
  var PERCENT = 100;
  var RANK_GAP = 1;
  var NEXT_WORD = " to ";
  var STATE_SEPARATOR = " · ";

  function scoreAt(badge, percent) {
    var min = Number(badge.getAttribute("data-min"));
    var next = badge.getAttribute("data-next");
    if (next === "") return min;
    var span = Math.max(Number(next) - min - RANK_GAP, 1);
    return min + Math.round((span * percent) / PERCENT);
  }
  function badgesOf(editor) { return Array.prototype.slice.call(editor.querySelectorAll(".rep-rank")); }
  function nameOf(badge) { return badge.querySelector("b").textContent; }

  function paint(editor, index, percent) {
    var badges = badgesOf(editor);
    badges.forEach(function (badge, at) {
      var top = badge.getAttribute("data-next") === "";
      var fill = at < index ? PERCENT : at === index ? (top ? PERCENT : percent) : 0;
      badge.querySelector(".rep-fill").style.width = fill + "%";
      badge.classList.toggle("cur", at === index);
      badge.classList.toggle("full", fill === PERCENT);
      badge.setAttribute("aria-valuenow", String(fill));
      badge.tabIndex = at === index ? 0 : -1;
      badge.querySelector(".rep-pc").textContent = at === index ? (top ? "complete" : fill + "%") : "";
    });
    var current = badges[index];
    var top = current.getAttribute("data-next") === "";
    var state = editor.querySelector("#rep-state");
    if (state) state.textContent = nameOf(current) + STATE_SEPARATOR + (top ? PERCENT + "%" : percent + "%" + NEXT_WORD + nameOf(badges[index + 1]));
  }

  function bind(editor) {
    var form = editor.querySelector("form.rep-set");
    var number = editor.querySelector(".rep-score");
    var save = form && form.querySelector("button[type=submit]");
    var ranks = editor.querySelector(".rep-ranks");
    if (!form || !number || !ranks) return;
    var original = number.value;
    var index = -1;
    var percent = 0;
    badgesOf(editor).forEach(function (badge, at) { if (badge.classList.contains("cur")) { index = at; percent = Number(badge.getAttribute("aria-valuenow")); } });
    var dragging = false;

    function changed() {
      form.classList.toggle("changed", number.value !== original);
      if (save && original !== "") save.disabled = number.value === original;
    }
    function set(at, pct) {
      var badge = badgesOf(editor)[at];
      index = at;
      percent = Math.max(0, Math.min(PERCENT, pct));
      number.value = String(scoreAt(badge, percent));
      paint(editor, index, percent);
      changed();
    }
    function fromPointer(event) {
      var hit = document.elementFromPoint(event.clientX, event.clientY);
      var badge = hit && hit.closest ? hit.closest(".rep-rank") : null;
      if (!badge || !ranks.contains(badge)) return;
      var box = badge.getBoundingClientRect();
      set(badgesOf(editor).indexOf(badge), Math.round(((event.clientX - box.left) * PERCENT) / box.width));
    }
    ranks.addEventListener("pointerdown", function (event) { dragging = true; fromPointer(event); event.preventDefault(); });
    ranks.addEventListener("pointermove", function (event) { if (dragging) fromPointer(event); });
    window.addEventListener("pointerup", function () { dragging = false; });
    ranks.addEventListener("keydown", function (event) {
      var step = event.shiftKey ? 10 : 1;
      var at = index < 0 ? 0 : index;
      var pct = index < 0 ? 0 : percent;
      var badges = badgesOf(editor);
      if (event.key === "ArrowRight") set(at, pct + step);
      else if (event.key === "ArrowLeft") set(at, pct - step);
      else if (event.key === "ArrowUp") set(Math.min(badges.length - 1, at + 1), 0);
      else if (event.key === "ArrowDown") set(Math.max(0, at - 1), 0);
      else return;
      event.preventDefault();
      badgesOf(editor)[index].focus();
    });
    number.addEventListener("input", function () {
      if (number.value === "") return changed();
      var score = Number(number.value);
      var badges = badgesOf(editor);
      var at = 0;
      badges.forEach(function (badge, i) { if (Number(badge.getAttribute("data-min")) <= score) at = i; });
      var min = Number(badges[at].getAttribute("data-min"));
      var next = badges[at].getAttribute("data-next");
      var pct = next === "" ? PERCENT : Math.round(((score - min) * PERCENT) / Math.max(Number(next) - min - RANK_GAP, 1));
      index = at;
      percent = Math.max(0, Math.min(PERCENT, pct));
      paint(editor, index, percent);
      changed();
    });
    if (original !== "") changed();
  }

  function slots() { return Array.prototype.slice.call(document.querySelectorAll(".rep-editor")); }
  function closeAll() {
    slots().forEach(function (slot) { slot.innerHTML = ""; slot.removeAttribute("id"); });
    Array.prototype.forEach.call(document.querySelectorAll(".rep-cell.on"), function (on) { on.classList.remove("on"); });
  }
  function select(href, cell) {
    var slot = cell.closest(".rep-cluster").querySelector(".rep-editor");
    return fetch(href, { credentials: "same-origin" }).then(function (response) { return response.text(); }).then(function (text) {
      var fresh = new DOMParser().parseFromString(text, "text/html").getElementById("rep-editor");
      if (!fresh) throw new Error("no editor");
      closeAll();
      slot.innerHTML = fresh.innerHTML;
      slot.id = "rep-editor";
      cell.classList.add("on");
      history.replaceState(null, "", href.replace(/#.*$/, ""));
      bind(slot);
    });
  }

  var open = document.getElementById("rep-editor");
  if (open) bind(open);
  var section = document.getElementById("reputations");
  if (section) {
    section.addEventListener("click", function (event) {
      var cell = event.target.closest("a.rep-cell");
      if (!cell || event.ctrlKey || event.metaKey || event.shiftKey || event.button !== 0) return;
      event.preventDefault();
      if (cell.classList.contains("on")) {
        closeAll();
        history.replaceState(null, "", cell.href.replace(/#.*$/, ""));
        return;
      }
      select(cell.href, cell).catch(function () { location.href = cell.href; });
    });
  }
})();
