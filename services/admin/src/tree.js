// A picker for the inventory pages: a select whose options carry their parent (data-parent) and path (data-path) is
// shown as a tree on the page itself, always visible, with guide lines for the levels, so a container sits under the
// place or the item that holds it. Branches expand and collapse; arrow keys walk it and Enter chooses. The select
// stays in the form (hidden) and keeps the value; without this script it is a plain indented list. Everything is
// built from text nodes.
(function () {
  "use strict";
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.appendChild(document.createTextNode(text));
    return n;
  }

  function enhance(select) {
    var nodes = {}, order = [], roots = [];
    Array.prototype.forEach.call(select.options, function (o) {
      var node = { value: o.value, label: o.dataset.label || o.textContent, full: o.dataset.path || "", parent: o.dataset.parent || "", kids: [] };
      nodes[o.value] = node;
      order.push(node);
    });
    order.forEach(function (n) {
      if (n.parent && nodes[n.parent]) nodes[n.parent].kids.push(n); else roots.push(n);
    });
    // Open by default while the whole tree is short; else the roots and the way to the chosen one.
    var open = {};
    function openAll() { order.forEach(function (n) { if (n.kids.length) open[n.value] = true; }); }
    function reveal(value) {
      var n = nodes[value] && nodes[nodes[value].parent];
      while (n) { open[n.value] = true; n = nodes[n.parent]; }
    }
    function path(value) {
      var n = nodes[value];
      if (!n) return "";
      if (n.full) return n.full;
      var names = [];
      while (n) { names.unshift(n.label); n = nodes[n.parent]; }
      return names.join(" › ");
    }

    var wrap = el("div", "treepick");
    var bar = el("div", "treepick-bar");
    var current = el("span", "treepick-current");
    var expand = el("button", "link", "Expand all");
    var collapse = el("button", "link", "Collapse all");
    expand.type = collapse.type = "button";
    bar.appendChild(current);
    bar.appendChild(expand);
    bar.appendChild(collapse);
    var list = el("ul", "treepick-list");
    list.setAttribute("role", "tree");
    wrap.appendChild(bar);
    wrap.appendChild(list);

    function show() {
      current.textContent = "";
      current.appendChild(el("span", "muted", "Chosen: "));
      current.appendChild(el("strong", "", select.value ? path(select.value) : "none"));
    }
    function labels() { return Array.prototype.slice.call(list.querySelectorAll(".treepick-label")); }
    function focusValue(value) {
      var found = labels().filter(function (b) { return b.dataset.value === value; })[0];
      if (found) found.focus();
    }
    function choose(n) {
      select.value = n.value;
      select.dispatchEvent(new Event("change", { bubbles: true }));
      show();
      draw();
      focusValue(n.value);
    }
    function branch(items, into) {
      items.forEach(function (n) {
        var isOpen = !!open[n.value];
        var li = el("li", "treepick-node");
        li.setAttribute("role", "treeitem");
        if (n.kids.length) li.setAttribute("aria-expanded", isOpen ? "true" : "false");
        var row = el("div", "treepick-row" + (n.value === select.value ? " chosen" : ""));
        if (n.kids.length) {
          var t = el("button", "treepick-toggle", isOpen ? "▾" : "▸");
          t.type = "button";
          t.tabIndex = -1;
          t.setAttribute("aria-label", (isOpen ? "Collapse " : "Expand ") + n.label);
          t.addEventListener("click", function () { open[n.value] = !open[n.value]; draw(); focusValue(n.value); });
          row.appendChild(t);
        } else {
          row.appendChild(el("span", "treepick-toggle blank", "•"));
        }
        var pick = el("button", "treepick-label", n.label);
        pick.type = "button";
        pick.dataset.value = n.value;
        pick.setAttribute("aria-selected", n.value === select.value ? "true" : "false");
        if (n.kids.length) pick.appendChild(el("span", "treepick-count", " (" + n.kids.length + ")"));
        pick.addEventListener("click", function () { choose(n); });
        row.appendChild(pick);
        li.appendChild(row);
        if (n.kids.length && isOpen) {
          var ul = el("ul", "treepick-list");
          ul.setAttribute("role", "group");
          branch(n.kids, ul);
          li.appendChild(ul);
        }
        into.appendChild(li);
      });
    }
    function draw() {
      list.textContent = "";
      branch(roots, list);
    }
    expand.addEventListener("click", function () { openAll(); draw(); });
    collapse.addEventListener("click", function () { open = {}; reveal(select.value); draw(); });
    // Down/Up walk the visible rows, Right opens a branch (or steps into it), Left closes it (or goes to the parent),
    // Enter and Space choose (a button's click).
    list.addEventListener("keydown", function (e) {
      var all = labels(), at = all.indexOf(document.activeElement);
      if (e.key === "ArrowDown") { e.preventDefault(); if (all.length) all[Math.min(all.length - 1, at + 1)].focus(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); if (all.length) all[Math.max(0, at - 1)].focus(); }
      else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        var n = at >= 0 ? nodes[all[at].dataset.value] : null;
        if (!n) return;
        e.preventDefault();
        if (e.key === "ArrowRight" && n.kids.length && !open[n.value]) { open[n.value] = true; draw(); focusValue(n.value); }
        else if (e.key === "ArrowLeft" && n.kids.length && open[n.value]) { open[n.value] = false; draw(); focusValue(n.value); }
        else if (e.key === "ArrowLeft" && n.parent && nodes[n.parent]) focusValue(n.parent);
        else if (e.key === "ArrowRight" && n.kids.length) focusValue(n.kids[0].value);
      }
    });

    if (order.length <= 30) openAll();
    reveal(select.value);
    roots.forEach(function (n) { if (n.kids.length && order.length <= 60) open[n.value] = true; });
    select.hidden = true;
    select.parentNode.insertBefore(wrap, select);
    show();
    draw();
  }

  Array.prototype.forEach.call(document.querySelectorAll("select[data-tree]"), enhance);
})();
