// Collapsible sections (<details>) open on the way to the address's anchor, so a link or a redirect to
// `#inv-<id>` or `#grant` shows what it points to even when the sections around it are collapsed. Without this script the
// server has opened the sections that matter. Nothing is built from data.
(function () {
  "use strict";
  function reveal() {
    var id = decodeURIComponent((location.hash || "").replace(/^#/, ""));
    if (!id) return;
    var target = document.getElementById(id);
    if (!target) return;
    var node = target;
    while (node) {
      if (node.tagName === "DETAILS") node.open = true;
      node = node.parentElement;
    }
    target.scrollIntoView();
  }
  // Expand all / Collapse all for the character & storage tree.
  function tools() {
    var tree = document.querySelector(".tree-panel");
    if (!tree) return;
    var bar = document.createElement("div");
    bar.className = "tree-tools";
    [["Expand all", true], ["Collapse all", false]].forEach(function (entry) {
      var button = document.createElement("button");
      button.type = "button";
      button.appendChild(document.createTextNode(entry[0]));
      button.addEventListener("click", function () {
        Array.prototype.forEach.call(tree.querySelectorAll("details.node"), function (node) { node.open = entry[1]; });
      });
      bar.appendChild(button);
    });
    tree.insertBefore(bar, tree.firstChild);
  }
  window.addEventListener("hashchange", reveal);
  tools();
  reveal();
})();
