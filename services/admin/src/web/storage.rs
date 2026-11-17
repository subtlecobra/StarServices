//! "Character & storage": what the player wears and every inventory they own, as one tree. The character is at the top
//! with its worn gear under what it hangs from; below it the personal inventory, the item banks and the containers,
//! each nested under the item that holds it. Every branch is a `<details>`, collapsed until opened, except the character
//! and the personal inventory and the way to the inventory just worked on (`open` in the query, set by the redirect after a
//! change). Items are paged per inventory, so a crate of hundreds is never one page, and so are the top-level
//! inventories. An item's actions are a small menu on its row; Move, which needs the whole tree to pick from, is on the
//! item's own page (`/inventory/item?geid=`), so no row carries a list of every inventory.

use std::collections::HashSet;

use sc_api::internal::AdminWornItem;

use super::*;

/// Items shown per inventory page, and top-level inventories per page.
pub const ITEMS_PER_PAGE: usize = 25;
pub const INVENTORIES_PER_PAGE: usize = 25;
/// The deepest nesting drawn (a loop in the tree ends here too).
const MAX_NEST: usize = 12;

/// What the panel is drawn from.
pub struct Panel<'a> {
    pub app: &'a App,
    pub frame: &'a Frame<'a>,
    pub id: u32,
    pub tree: &'a [AdminInventory],
    pub equipment: Option<&'a AdminEquipmentResponse>,
    pub editable: bool,
    pub query: &'a HashMap<String, String>,
}

/// The page number (from 1) a query key holds.
fn page_number(query: &HashMap<String, String>, key: &str) -> usize {
    query.get(key).and_then(|text| text.parse::<usize>().ok()).filter(|page| *page >= 1).unwrap_or(1)
}

/// The glyph in front of an item, by what it is.
fn glyph(container: bool, kind: &str) -> &'static str {
    if container {
        "\u{25A3}"
    } else if kind.starts_with("Char_Armor") || kind.starts_with("Char_Clothing") || kind.starts_with("Char_") {
        "\u{25CF}"
    } else if kind.starts_with("Weapon") {
        "\u{2726}"
    } else {
        "\u{25C6}"
    }
}

fn worn_label(item: &AdminWornItem) -> &str {
    if item.display_name.is_empty() { &item.class_name } else { &item.display_name }
}

/// A numbered pager: `Previous 1 2 3 Next`, with the first, last and the pages around the current one.
fn pager(panel: &Panel, key: &str, page: usize, per: usize, total: usize, anchor: &str) -> String {
    if total <= per {
        return String::new();
    }
    let pages = total.div_ceil(per);
    let page = page.min(pages);
    let (from, to) = ((page - 1) * per + 1, (page * per).min(total));
    let link = |to_page: usize, text: &str| -> String {
        format!(r#"<a href="{}">{}</a>"#, escape(&panel.url(&[(key, Some(to_page.to_string()))], anchor)), escape(text))
    };
    let mut out = format!(r#"<nav class="pager" aria-label="Pages">Showing {from}-{to} of {total} "#);
    if page > 1 {
        out.push_str(&link(page - 1, "Previous"));
        out.push(' ');
    }
    let mut last = 0;
    for number in 1..=pages {
        if number != 1 && number != pages && number.abs_diff(page) > 2 {
            continue;
        }
        if number > last + 1 {
            out.push_str("<span>…</span> ");
        }
        if number == page {
            let _ = write!(out, r#"<strong aria-current="page">{number}</strong> "#);
        } else {
            out.push_str(&link(number, &number.to_string()));
            out.push(' ');
        }
        last = number;
    }
    if page < pages {
        out.push_str(&link(page + 1, "Next"));
    }
    out.push_str("</nav>");
    out
}

impl Panel<'_> {
    /// The address of this page with `changes` applied to its query (a `None` removes the key), at `anchor`. The notice of
    /// an action is not carried along.
    fn url(&self, changes: &[(&str, Option<String>)], anchor: &str) -> String {
        let mut pairs: Vec<(String, String)> =
            self.query.iter().filter(|(key, _)| !matches!(key.as_str(), "notice" | "error")).map(|(key, value)| (key.clone(), value.clone())).collect();
        for (key, value) in changes {
            pairs.retain(|(known, _)| known != key);
            if let Some(value) = value {
                pairs.push(((*key).to_string(), value.clone()));
            }
        }
        pairs.sort();
        let mut out = format!("{}/accounts/{}{INVENTORY_PAGE}", self.app.prefix, self.id);
        for (at, (key, value)) in pairs.iter().enumerate() {
            let _ = write!(out, "{}{}={}", if at == 0 { '?' } else { '&' }, query_value(key), query_value(value));
        }
        if !anchor.is_empty() {
            let _ = write!(out, "#{anchor}");
        }
        out
    }

    fn personal(&self) -> Option<String> {
        self.tree_player().map(|player| format!("{player}:PlayerInventory:0"))
    }

    fn tree_player(&self) -> Option<u64> {
        tree_player(self.tree)
    }

    fn inventory(&self, id: &str) -> Option<&AdminInventory> {
        self.tree.iter().find(|inventory| inventory_id(inventory) == id)
    }

    /// The inventories to show open: those the query names and every one above them.
    fn open_set(&self) -> HashSet<String> {
        let mut open = HashSet::new();
        for id in self.query.get("open").map_or("", String::as_str).split(',').filter(|id| !id.is_empty()) {
            let mut at = self.inventory(id);
            while let Some(inventory) = at {
                if !open.insert(inventory_id(inventory).to_string()) {
                    break;
                }
                at = self.inventory(&inventory.parent_inventory_id);
            }
        }
        open
    }

    /// The whole panel.
    pub fn render(&self) -> String {
        let open = self.open_set();
        let mut out = String::from(r#"<div class="tree-panel">"#);
        out.push_str(&self.character());

        let known = |parent: &str| !parent.is_empty() && self.inventory(parent).is_some();
        let mut tops: Vec<&AdminInventory> = self.tree.iter().filter(|inventory| !known(&inventory.parent_inventory_id)).collect();
        let personal = self.personal();
        tops.sort_by_key(|inventory| Some(inventory_id(inventory).to_string()) != personal);
        let page = page_number(self.query, "ip").min(tops.len().div_ceil(INVENTORIES_PER_PAGE).max(1));
        let shown = tops.iter().skip((page - 1) * INVENTORIES_PER_PAGE).take(INVENTORIES_PER_PAGE);
        let mut seen = HashSet::new();
        let _ = write!(out, r#"<div class="storage"><h3 class="storage-title">Storage <span class="count">{}</span></h3>"#, tops.len());
        out.push_str(&pager(self, "ip", page, INVENTORIES_PER_PAGE, tops.len(), "storage"));
        for inventory in shown {
            let default_open = Some(inventory_id(inventory).to_string()) == personal;
            out.push_str(&self.inventory_node(inventory, 0, default_open, &open, &mut seen));
        }
        if tops.is_empty() {
            out.push_str(r#"<p class="muted">No inventory yet: sc-entitygraph makes them on first need.</p>"#);
        }
        out.push_str(&pager(self, "ip", page, INVENTORIES_PER_PAGE, tops.len(), "storage"));
        out.push_str("</div></div>");
        out
    }

    /// The character: worn gear under what it hangs from.
    fn character(&self) -> String {
        let Some(equipment) = self.equipment else {
            return r#"<details class="node character" id="character" open><summary><span class="ico" aria-hidden="true">&#9679;</span> <span class="name">Character</span> <span class="meta">what is worn could not be read</span></summary></details>"#.to_string();
        };
        let worn = &equipment.worn;
        let present: HashSet<u64> = worn.iter().map(|item| item.geid).collect();
        let roots: Vec<&AdminWornItem> = worn.iter().filter(|item| !present.contains(&item.parent_geid) || item.parent_geid == item.geid).collect();
        let mut out = format!(
            r#"<details class="node character" id="character" open><summary><span class="ico" aria-hidden="true">&#9679;</span> <span class="name">Character</span> <span class="meta">{} worn item{}</span></summary><div class="kids">"#,
            worn.len(),
            if worn.len() == 1 { "" } else { "s" }
        );
        if worn.is_empty() {
            out.push_str(r#"<p class="muted">Nothing is worn yet: the loadout is made when the player first joins a shard.</p>"#);
        }
        for root in roots {
            out.push_str(&self.worn_node(equipment, root, 0));
        }
        out.push_str("</div></details>");
        out
    }

    fn worn_node(&self, equipment: &AdminEquipmentResponse, item: &AdminWornItem, nest: usize) -> String {
        let children: Vec<&AdminWornItem> = if nest < MAX_NEST { equipment.worn.iter().filter(|child| child.parent_geid == item.geid && child.geid != item.geid).collect() } else { Vec::new() };
        let line = format!(
            r#"<span class="ico" aria-hidden="true">{}</span> <span class="name">{}</span> <span class="meta">{} · {} {}</span>"#,
            glyph(false, &item.item_type),
            escape(worn_label(item)),
            escape(&item.port),
            escape(&item.item_type),
            escape(&item.item_sub_type)
        );
        let actions = if self.editable && item.removable { self.take_off(item) } else { String::new() };
        if children.is_empty() {
            return format!(r#"<div class="item">{line}{actions}</div>"#);
        }
        let kids: String = children.iter().map(|child| self.worn_node(equipment, child, nest + 1)).collect();
        format!(r#"<details class="node" open><summary>{line}</summary>{actions}<div class="kids">{kids}</div></details>"#)
    }

    /// Takes a worn item off into the personal inventory; another place is picked on the item's page.
    fn take_off(&self, item: &AdminWornItem) -> String {
        let Some(personal) = self.personal() else {
            return format!(r#" <a class="small" href="{}">Take off…</a>"#, escape(&self.item_link(item.geid)));
        };
        let form = self.frame.form_class(
            "stacked menu-form",
            &format!("/accounts/{}/inventory/unequip", self.id),
            &format!(
                r#"<input type="hidden" name="geid" value="{}"><input type="hidden" name="to" value="{}"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" aria-label="Reason">"#,
                item.geid,
                escape(&personal)
            ),
            "Take off",
        );
        format!(
            r#"<details class="actions"><summary>Actions</summary><div class="menu">{form}<a class="small" href="{}">Put it somewhere else…</a></div></details>"#,
            escape(&self.item_link(item.geid))
        )
    }

    fn item_link(&self, geid: u64) -> String {
        format!("{}/accounts/{}{INVENTORY_PAGE}/item?geid={geid}", self.app.prefix, self.id)
    }

    /// The occupancy bar and words of an inventory.
    fn capacity(&self, properties: &InventoryNodeProperties) -> String {
        use sc_proto::services::entitygraph::v1::inventory_configuration::Configuration;
        let text = capacity_text(properties);
        match properties.configuration.as_ref().and_then(|configuration| configuration.configuration) {
            Some(Configuration::Physical(physical)) if physical.capacity > 0 => format!(
                r#"<meter min="0" max="{cap}" value="{occ}" aria-label="Occupancy"></meter> <span class="meta">{text}</span>"#,
                cap = physical.capacity,
                occ = physical.occupancy.min(physical.capacity),
            ),
            _ => format!(r#"<span class="meta">{}</span>"#, escape(&text)),
        }
    }

    fn inventory_node(&self, inventory: &AdminInventory, nest: usize, default_open: bool, open: &HashSet<String>, seen: &mut HashSet<String>) -> String {
        let Some(properties) = inventory.inventory.as_ref() else { return String::new() };
        if nest > MAX_NEST || !seen.insert(properties.id.clone()) {
            return String::new();
        }
        let id = properties.id.as_str();
        let is_open = default_open || open.contains(id);
        let total = inventory.items.len();
        let page = page_number(self.query, &format!("pg:{id}")).min(total.div_ceil(ITEMS_PER_PAGE).max(1));
        let editable = self.editable && !inventory.live;

        let mut body = String::new();
        if inventory.live {
            body.push_str(r#"<p class="warn">Held by something live on a shard: the DGS has it, so it is not changed here.</p>"#);
        }
        let key = format!("pg:{id}");
        let anchor = format!("inv-{}", query_value(id));
        body.push_str(&pager(self, &key, page, ITEMS_PER_PAGE, total, &anchor));
        // Item banks are listed whole; items beyond the first `ITEMS_PER_PAGE` come with the pager.
        let mut placed: HashSet<&str> = inventory.items.iter().flat_map(|item| item.held_inventory_ids.iter().map(String::as_str)).collect();
        for item in inventory.items.iter().skip((page - 1) * ITEMS_PER_PAGE).take(ITEMS_PER_PAGE) {
            body.push_str(&self.item_entry(inventory, item, nest, open, seen, editable));
        }
        if total == 0 {
            body.push_str(r#"<p class="muted">Empty.</p>"#);
        }
        if inventory.item_count as usize > total {
            let _ = write!(body, r#"<p class="muted">Only the first {total} of {} items are loaded.</p>"#, inventory.item_count);
        }
        body.push_str(&pager(self, &key, page, ITEMS_PER_PAGE, total, &anchor));
        // Inventories under this one that no listed item holds (their holder is beyond the loaded items, or is not an item).
        placed.insert("");
        let strays: String = self
            .tree
            .iter()
            .filter(|other| other.parent_inventory_id == id && !placed.contains(inventory_id(other)))
            .map(|other| self.inventory_node(other, nest + 1, false, open, seen))
            .collect();
        if !strays.is_empty() {
            let _ = write!(body, r#"<div class="kids strays"><p class="muted">Also inside:</p>{strays}</div>"#);
        }

        let marks = format!("{}{}", if inventory.live { r#" <span class="tag warn-tag">live</span>"# } else { "" }, if properties.shared { r#" <span class="tag">shared</span>"# } else { "" });
        let count = inventory.item_count;
        format!(
            r#"<details class="node inventory" id="inv-{id_attr}"{open}><summary title="{path}"><span class="ico" aria-hidden="true">{ico}</span> <span class="name">{label}</span>{marks} {capacity} <span class="meta">· {count} item{s}</span></summary><div class="kids">{body}</div></details>"#,
            id_attr = escape(id),
            open = if is_open { " open" } else { "" },
            path = escape(&inventory_path(self.tree, inventory)),
            ico = glyph(properties.context == "Container", ""),
            label = escape(&inventory_label(self.tree, inventory)),
            capacity = self.capacity(properties),
            s = if count == 1 { "" } else { "s" },
        )
    }

    /// One item: its line, its actions, and the inventories it holds nested under it.
    fn item_entry(&self, inventory: &AdminInventory, item: &AdminInventoryItem, nest: usize, open: &HashSet<String>, seen: &mut HashSet<String>, editable: bool) -> String {
        let held: Vec<&AdminInventory> = item.held_inventory_ids.iter().filter_map(|held| self.inventory(held)).collect();
        let name = if item.display_name.is_empty() { item.class_name.as_str() } else { item.display_name.as_str() };
        let label = if item.class_name.is_empty() {
            format!(r#"<span class="muted">class {:#010x}, not seeded</span>"#, item.class_guid_crc)
        } else {
            escape(name)
        };
        let stack = if item.stack_size > 1 { format!(r#" <span class="qty">&times;{}</span>"#, item.stack_size) } else { String::new() };
        let class = if !item.display_name.is_empty() && !item.class_name.is_empty() { format!(" · {}", escape(&item.class_name)) } else { String::new() };
        let line = format!(
            r#"<span class="ico" aria-hidden="true">{}</span> <span class="name" title="{}">{label}</span>{stack} <span class="meta">#{}{class}</span>"#,
            glyph(!held.is_empty(), ""),
            escape(&item.class_guid),
            item.geid,
        );
        let actions = if item.on_shard {
            format!(r#" <span class="warn">on shard <code>{}</code></span>"#, escape(&item.shard_id))
        } else if editable {
            self.item_actions(inventory, item)
        } else {
            String::new()
        };
        if held.is_empty() {
            return format!(r#"<div class="item" id="item-{}">{line}{actions}</div>"#, item.geid);
        }
        let opened = held.iter().any(|held| open.contains(inventory_id(held)));
        let inside: String = held.iter().map(|held| self.inventory_node(held, nest + 1, false, open, seen)).collect();
        format!(r#"<details class="node item-node" id="item-{}"{}><summary>{line}</summary>{actions}<div class="kids">{inside}</div></details>"#, item.geid, if opened { " open" } else { "" })
    }

    /// Equip (when it fits a free port), Move (on the item's page) and Remove, as a small menu.
    fn item_actions(&self, inventory: &AdminInventory, item: &AdminInventoryItem) -> String {
        let mut menu = String::new();
        if let Some(equip) = self.equip_form(item) {
            menu.push_str(&equip);
        }
        let _ = write!(menu, r#"<a class="small" href="{}">Move…</a>"#, escape(&self.item_link(item.geid)));
        menu.push_str(&self.frame.form_class(
            "stacked menu-form",
            &format!("/accounts/{}/inventory/remove", self.id),
            &format!(
                r#"<input type="hidden" name="geid" value="{}"><input type="hidden" name="from" value="{}"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" aria-label="Reason">"#,
                item.geid,
                escape(inventory_id(inventory))
            ),
            "Remove",
        ));
        format!(r#"<details class="actions"><summary>Actions</summary><div class="menu">{menu}</div></details>"#)
    }

    /// The free ports of what the player wears that `item` fits, as a form to equip it; none when there are none.
    pub fn equip_form(&self, item: &AdminInventoryItem) -> Option<String> {
        let equipment = self.equipment?;
        let options: String = equipment
            .ports
            .iter()
            .filter(|port| port.occupant_geid == 0 && port.candidates.contains(&item.geid))
            .fold(String::new(), |mut out, port| {
                let _ = write!(out, r#"<option value="{}:{}">{} › {}</option>"#, port.parent_geid, escape(&port.port), escape(&port.parent_name), escape(&port.port));
                out
            });
        if options.is_empty() {
            return None;
        }
        Some(self.frame.form_class(
            "stacked menu-form",
            &format!("/accounts/{}/inventory/equip", self.id),
            &format!(
                r#"<input type="hidden" name="geid" value="{}"><div class="wide"><span class="muted">Equip on</span><select name="port">{options}</select></div><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" aria-label="Reason">"#,
                item.geid
            ),
            "Equip",
        ))
    }
}

/// An item's own page: what it is and where, and the forms that need room: Move (to any inventory of the tree, with the
/// picker), Equip, Take off and Remove.
pub async fn item_page(State(app): State<Shared>, Extension(admin): Extension<Admin>, Path(id): Path<u32>, RawQuery(raw): RawQuery) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let query = query(raw);
    let mut body = flash(&query) + &account_header(&app, &target, INVENTORY_PAGE);
    let Some(geid) = target.geid else { return html(frame.page("Item", &(body + &no_player()))) };
    let Some(item) = query.get("geid").and_then(|text| text.parse::<u64>().ok()) else {
        return html(frame.page("Item", &(body + &notice("Which item?", true))));
    };
    let (now, tree, equipment) = tokio::join!(playing(&app, id, geid), app.owners.inventory_tree(geid), app.owners.equipment(geid));
    let tree = match tree {
        Ok(tree) => tree,
        Err(status) => return html(frame.page("Item", &(body + &failed("sc-entitygraph", &status)))),
    };
    let locked = inventory_lock(&now);
    let panel = Panel { app: &app, frame: &frame, id, tree: &tree, equipment: equipment.as_ref().ok(), editable: locked.is_none(), query: &query };
    let back = |open: &str| format!(r#"<p><a href="{}/accounts/{id}{INVENTORY_PAGE}?open={}#inv-{}">&larr; Back to character &amp; storage</a></p>"#, app.prefix, query_value(open), query_value(open));
    let stowed = tree.iter().flat_map(|inventory| &inventory.items).find(|found| found.geid == item);
    let worn = equipment.as_ref().ok().and_then(|equipment| equipment.worn.iter().find(|found| found.geid == item));
    let _ = write!(body, "<p>Player: {}</p>", playing_text(&now));
    if let Some(text) = locked {
        let _ = write!(body, r#"<p class="warn">{}</p>"#, escape(text));
    }
    if let Some(found) = stowed {
        let inventory = tree.iter().find(|inventory| inventory_id(inventory) == found.inventory_id);
        let name = if found.display_name.is_empty() { found.class_name.as_str() } else { found.display_name.as_str() };
        let _ = write!(
            body,
            r#"<h2>{}</h2><p>Item <code>{}</code> · class <code>{}</code> · stack {} · in <a href="{}/accounts/{id}{INVENTORY_PAGE}?open={}#inv-{}">{}</a></p>"#,
            escape(name),
            found.geid,
            escape(&found.class_guid),
            found.stack_size,
            app.prefix,
            query_value(&found.inventory_id),
            query_value(&found.inventory_id),
            escape(&inventory.map_or_else(|| found.inventory_id.clone(), |inventory| inventory_path(&tree, inventory))),
        );
        if locked.is_none() && !found.on_shard && inventory.is_some_and(|inventory| !inventory.live) {
            let hidden = format!(r#"<input type="hidden" name="geid" value="{}"><input type="hidden" name="from" value="{}">"#, found.geid, escape(&found.inventory_id));
            let options = target_options(&tree, found);
            body.push_str(r#"<h3>Move</h3>"#);
            body.push_str(&frame.form_class(
                "stacked",
                &format!("/accounts/{id}/inventory/move"),
                &format!(
                    r#"{hidden}<div class="wide"><span class="muted">Move to</span><select name="to" data-tree>{options}</select></div><input name="quantity" type="number" min="0" max="{}" placeholder="All" class="narrow" aria-label="Quantity"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" aria-label="Reason">"#,
                    found.stack_size.max(1)
                ),
                "Move",
            ));
            if let Some(equip) = panel.equip_form(found) {
                body.push_str("<h3>Equip</h3>");
                body.push_str(&equip);
            }
            body.push_str("<h3>Remove</h3>");
            body.push_str(&frame.form(
                &format!("/accounts/{id}/inventory/remove"),
                &format!(r#"{hidden}<input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#),
                "Remove",
            ));
            body.push_str(r#"<p class="muted">Moving part of a stack splits it into a new item; removing an item erases everything it holds.</p>"#);
        } else if locked.is_none() {
            body.push_str(r#"<p class="muted">This item is held by something live on a shard, so it is not changed here.</p>"#);
        }
        body.push_str(&back(&found.inventory_id));
    } else if let Some(found) = worn {
        let _ = write!(body, "<h2>{}</h2><p>Worn: item <code>{}</code> · {} {} · on port {}</p>", escape(worn_label(found)), found.geid, escape(&found.item_type), escape(&found.item_sub_type), escape(&found.port));
        if locked.is_none() && found.removable {
            let keep = |inventory: &AdminInventory| !inventory.live;
            body.push_str("<h3>Take off</h3>");
            body.push_str(&frame.form_class(
                "stacked",
                &format!("/accounts/{id}/inventory/unequip"),
                &format!(
                    r#"<input type="hidden" name="geid" value="{}"><div class="wide"><span class="muted">Put it in</span><select name="to" data-tree>{}</select></div><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" aria-label="Reason">"#,
                    found.geid,
                    inventory_options(&tree, &keep, panel.personal().as_deref(), true)
                ),
                "Take off",
            ));
        }
        body.push_str(&format!(r#"<p><a href="{}/accounts/{id}{INVENTORY_PAGE}#character">&larr; Back to character &amp; storage</a></p>"#, app.prefix));
    } else {
        body.push_str(&notice(&format!("The player has no item {item} now."), true));
        let _ = write!(body, r#"<p><a href="{}/accounts/{id}{INVENTORY_PAGE}">&larr; Back to character &amp; storage</a></p>"#, app.prefix);
    }
    let _ = write!(body, r#"<script src="{}/static/tree.js" defer></script>"#, app.prefix);
    html(frame.page("Item", &body))
}
