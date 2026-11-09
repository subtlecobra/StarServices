// The inventory page's item catalog and cart: filters the classes as you type, by the game's inventory categories and
// their sub-filters, and gives several items at once. An item joins the cart only when its + is clicked, with the
// amount beside it for an item that stacks. Everything is built from text nodes, so nothing a class carries is read as
// markup.
(function () {
  "use strict";
  var root = document.getElementById("catalog");
  if (!root) return;
  var table;
  try { table = JSON.parse(root.dataset.table); } catch (e) { return; }
  var classic = document.getElementById("classic");
  if (classic) classic.hidden = true;

  var SVG = "http://www.w3.org/2000/svg";
  // One simple outline per category, in a 24 x 24 box.
  var ICONS = {
    all: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
    weapons: "M3 9h13l2 2v3h-5l-1 5H8l1-5H3z",
    armor: "M12 3l7 3v5c0 5-3 8-7 10-4-2-7-5-7-10V6z",
    undersuit: "M8 3L3 7l2 4 3-1v11h8V10l3 1 2-4-5-4-2 2h-4z",
    clothing: "M9 3L3 6l2 5 3-1v10h8V10l3 1 2-5-6-3c0 2-6 2-6 0z",
    ammo: "M10 3h4c1 2 1 4 1 6v12H9V9c0-2 0-4 1-6zM9 17h6",
    utility: "M15 4a4 4 0 0 0-3 6l-8 8 2 2 8-8a4 4 0 0 0 6-3l-3 2-2-2 2-3z",
    sustenance: "M6 4h12l-1 16H7zM6.5 9h11",
    vehicle: "M12 3l4 9v5l-4-2-4 2v-5zM4 14l4-2M20 14l-4-2",
    containers: "M4 8l8-4 8 4v8l-8 4-8-4zM4 8l8 4 8-4M12 12v8",
    other: "M5 12h.01M12 12h.01M19 12h.01",
    helmet: "M5 14a7 7 0 0 1 14 0v3H5zM9 14h6",
    box: "M4 8l8-4 8 4v8l-8 4-8-4zM4 8l8 4 8-4M12 12v8"
  };
  var LABELS = table.labels;
  var WEIGHTS = table.weights;
  var CATEGORY_OF = {};
  table.categories.forEach(function (c) { c.types.forEach(function (t) { CATEGORY_OF[t] = c.id; }); });

  function icon(name) {
    var svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "1.7");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    var path = document.createElementNS(SVG, "path");
    path.setAttribute("d", ICONS[name] || ICONS.box);
    svg.appendChild(path);
    return svg;
  }
  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }
  function iconFor(item) {
    if (item.type === "Char_Armor_Helmet") return "helmet";
    if (item.type === "Char_Armor_Undersuit") return "undersuit";
    return CATEGORY_OF[item.type] || "other";
  }
  function scu(micro) {
    if (!micro) return "";
    var v = micro / 1000000;
    return (v >= 1 ? v.toFixed(v % 1 ? 2 : 0) : v.toFixed(4).replace(/0+$/, "")) + " SCU";
  }
  function pressed(node, on) { node.setAttribute("aria-pressed", String(on)); }

  // ---- the filters: one category at a time, its sub-filters below it and no other category's ----
  var category = null, chosen = {};
  var search = el("input", "cat-search");
  search.type = "search";
  search.placeholder = "Search items by name, e.g. coda";
  search.setAttribute("aria-label", "Search items");
  var cats = el("div", "chips");
  var groups = el("div", "groups");
  var status = el("div", "cat-status");
  status.setAttribute("aria-live", "polite");
  var cards = el("div", "cards");

  function chip(label, iconName, extra) {
    var node = el("button", "chip " + (extra || ""));
    node.type = "button";
    pressed(node, false);
    if (iconName) node.appendChild(icon(iconName));
    node.appendChild(document.createTextNode(label));
    return node;
  }
  var all = chip("All", "all");
  pressed(all, true);
  cats.appendChild(all);
  var catButtons = {};
  function choose(id) {
    category = id;
    chosen = {};
    pressed(all, !id);
    Object.keys(catButtons).forEach(function (key) { pressed(catButtons[key], key === id); });
    renderGroups();
    refresh();
  }
  all.addEventListener("click", function () { choose(null); });
  table.categories.forEach(function (c) {
    var b = chip(c.label, c.id);
    b.addEventListener("click", function () { choose(category === c.id ? null : c.id); });
    catButtons[c.id] = b;
    cats.appendChild(b);
  });
  function renderGroups() {
    groups.textContent = "";
    var current = table.categories.filter(function (c) { return c.id === category; })[0];
    groups.hidden = !current || !current.groups.length;
    if (!current) return;
    current.groups.forEach(function (group) {
      var row = el("div", "chips group");
      row.appendChild(el("span", "group-label muted", group.label));
      group.choices.forEach(function (choice) {
        var b = chip(choice.label, null, group.id === "weight" ? choice.id : "");
        b.addEventListener("click", function () {
          var on = b.getAttribute("aria-pressed") !== "true";
          pressed(b, on);
          if (on) chosen[choice.id] = true; else delete chosen[choice.id];
          refresh();
        });
        row.appendChild(b);
      });
      groups.appendChild(row);
    });
  }
  renderGroups();

  root.appendChild(search);
  root.appendChild(cats);
  root.appendChild(groups);
  root.appendChild(status);
  root.appendChild(cards);

  // ---- the cart: what the + buttons added, kept in the form's `items` as guid:amount:copies ----
  var cart = [];
  var cartBox = document.getElementById("cart");
  var itemsInput = document.getElementById("items");
  var MAX_COPIES = 20;
  if (itemsInput) { itemsInput.type = "hidden"; itemsInput.removeAttribute("required"); itemsInput.value = ""; }

  function badges(counts) {
    Array.prototype.forEach.call(cards.children, function (node) {
      var count = counts[node.dataset.guid] || 0;
      var badge = node.querySelector(".incart");
      badge.textContent = count ? "In cart: " + count : "";
    });
  }
  function renderCart() {
    if (itemsInput) itemsInput.value = cart.map(function (l) { return l.item.guid + ":" + l.amount + ":" + l.copies; }).join(",");
    var counts = {};
    cart.forEach(function (l) { counts[l.item.guid] = l.item.stackLimit ? l.amount : l.copies; });
    badges(counts);
    if (!cartBox) return;
    cartBox.textContent = "";
    var total = cart.reduce(function (sum, l) { return sum + l.copies; }, 0);
    var head = el("div", "cart-head");
    head.appendChild(el("strong", "", cart.length ? "To give (" + total + " item" + (total === 1 ? "" : "s") + ")" : "Nothing added yet: press + on an item."));
    if (cart.length) {
      var clear = el("button", "link", "Clear");
      clear.type = "button";
      clear.addEventListener("click", function () { cart = []; renderCart(); });
      head.appendChild(clear);
    }
    cartBox.appendChild(head);
    cart.forEach(function (l, index) {
      var row = el("div", "cart-row");
      var holder = el("span", "icon");
      holder.appendChild(icon(iconFor(l.item)));
      row.appendChild(holder);
      row.appendChild(el("span", "cart-name", l.item.name || l.item.class));
      row.appendChild(el("span", "tag", l.item.stackLimit ? l.amount + " in a stack" : "× " + l.copies));
      var remove = el("button", "link", "Remove");
      remove.type = "button";
      remove.setAttribute("aria-label", "Remove " + (l.item.name || l.item.class));
      remove.addEventListener("click", function () { cart.splice(index, 1); renderCart(); });
      row.appendChild(remove);
      cartBox.appendChild(row);
    });
  }
  function addToCart(item, amount) {
    var line = cart.filter(function (l) { return l.item.guid === item.guid; })[0];
    if (!line) { line = { item: item, amount: 1, copies: 1 }; cart.push(line); if (item.stackLimit) line.amount = 0; else line.copies = 0; }
    // A stackable item grows its stack; one that does not stack gets one more of it.
    if (item.stackLimit) line.amount = Math.min(item.stackLimit, line.amount + amount);
    else line.copies = Math.min(MAX_COPIES, line.copies + 1);
    renderCart();
  }
  var form = itemsInput && itemsInput.form;
  if (form) form.addEventListener("submit", function (event) {
    if (!cart.length) { event.preventDefault(); if (cartBox) cartBox.querySelector("strong").textContent = "Add some items first: press + on an item."; }
  });

  // ---- the cards ----
  function card(item) {
    var node = el("div", "card");
    node.dataset.guid = item.guid;
    var holder = el("span", "icon");
    holder.appendChild(icon(iconFor(item)));
    node.appendChild(holder);
    node.appendChild(el("span", "name", item.name || item.class));
    var tags = el("span", "meta");
    tags.appendChild(el("span", "tag", LABELS[item.type + ":" + item.sub] || LABELS[item.type] || item.type.replace(/^(Char_|FPS_)/, "").replace(/_/g, " ")));
    var weight = CATEGORY_OF[item.type] === "armor" ? WEIGHTS[item.sub] : null;
    if (weight) tags.appendChild(el("span", "tag " + weight.id, weight.label));
    else if (item.sub && item.sub !== "UNDEFINED") tags.appendChild(el("span", "tag", item.sub));
    node.appendChild(tags);
    var meta = [];
    if (item.room) meta.push(scu(item.room));
    if (item.container) meta.push("holds " + scu(item.capacity));
    node.appendChild(el("span", "meta", meta.join(" · ") || item.class));
    var buy = el("span", "buy");
    buy.appendChild(el("span", "incart muted"));
    var amount = null;
    if (item.stackLimit) {
      amount = el("input", "amount");
      amount.type = "number";
      amount.min = "1";
      amount.max = String(item.stackLimit);
      amount.value = "1";
      amount.setAttribute("aria-label", "Amount of " + (item.name || item.class));
      buy.appendChild(amount);
    }
    var add = el("button", "add", "+");
    add.type = "button";
    add.title = "Add to the items to give";
    add.setAttribute("aria-label", "Add " + (item.name || item.class));
    add.addEventListener("click", function () {
      var n = amount ? Math.max(1, Math.min(item.stackLimit, parseInt(amount.value, 10) || 1)) : 1;
      addToCart(item, n);
    });
    buy.appendChild(add);
    node.appendChild(buy);
    return node;
  }

  var timer = null, controller = null;
  function refresh() {
    clearTimeout(timer);
    timer = setTimeout(load, 180);
  }
  function load() {
    if (controller) controller.abort();
    controller = new AbortController();
    var url = new URL(root.dataset.url, window.location.href);
    if (search.value.trim()) url.searchParams.set("q", search.value.trim());
    if (category) url.searchParams.set("cat", category);
    var options = Object.keys(chosen);
    if (options.length) url.searchParams.set("opt", options.join(","));
    status.textContent = "Searching…";
    fetch(url, { credentials: "same-origin", signal: controller.signal, headers: { Accept: "application/json" } })
      .then(function (r) { return r.json().then(function (body) { return { ok: r.ok, body: body }; }); })
      .then(function (reply) {
        if (!reply.ok) { status.textContent = reply.body.error || "The search failed."; return; }
        var items = reply.body.classes;
        cards.textContent = "";
        items.forEach(function (item) { cards.appendChild(card(item)); });
        renderCart();
        status.textContent = items.length ? items.length + (items.length >= 60 ? "+ items: narrow the search" : " items") : "No item matches.";
      })
      .catch(function (e) { if (e.name !== "AbortError") status.textContent = "The search failed."; });
  }
  search.addEventListener("input", refresh);
  renderCart();
  load();
})();
