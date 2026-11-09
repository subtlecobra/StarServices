//! The Ships tab: the ships a player keeps in hangars (sc-entitygraph's `ShipsInternal`, see docs/SHIPS.md), the grant
//! of new ones, the vehicle entitlements and the rentals, on one page of collapsible sections. Server-rendered: the ship
//! search is a GET form and the cart of ships to grant travels in the query string, then in a hidden field of the grant
//! form. The owners are asked together, not one after the other (see `ships`).

use super::*;
use sc_api::internal::{AcquireShipRequest, PadSize, ReportShipRequest, ShipInsurance, ShipPolicy, ShipRecord, ShipState};

const PAGE: &str = "/ships";
/// The most ship lines in a cart, copies of one class, and ships in one grant.
const MAX_SHIP_LINES: usize = 10;
const MAX_SHIP_COPIES: u32 = 10;
const MAX_GRANT_SHIPS: u32 = 20;
const MAX_INSURANCE_DAYS: u32 = 36_500;
const SEARCH_LIMIT: u32 = 40;

pub fn pad_name(pad: i32) -> &'static str {
    match PadSize::try_from(pad) {
        Ok(PadSize::Tiny) => "Tiny",
        Ok(PadSize::Xsmall) => "XSmall",
        Ok(PadSize::Small) => "Small",
        Ok(PadSize::Medium) => "Medium",
        Ok(PadSize::Large) => "Large",
        Ok(PadSize::Xlarge) => "XLarge",
        _ => "unknown",
    }
}

fn state_name(state: i32) -> &'static str {
    match ShipState::try_from(state) {
        Ok(ShipState::Stored) => "stored",
        Ok(ShipState::Deployed) => "deployed",
        Ok(ShipState::Destroyed) => "destroyed",
        Ok(ShipState::Claiming) => "claiming",
        Ok(ShipState::Expired) => "expired",
        _ => "unknown",
    }
}

fn insurance_text(record: &ShipRecord, now: i64) -> String {
    let Some(insurance) = record.insurance.as_ref() else { return "none".into() };
    match ShipPolicy::try_from(insurance.policy) {
        Ok(ShipPolicy::Lifetime) => "lifetime".into(),
        Ok(ShipPolicy::Duration) if insurance.expires_at_ms > now => format!("until {}", time(insurance.expires_at_ms)),
        Ok(ShipPolicy::Duration) => format!("expired {}", time(insurance.expires_at_ms)),
        _ => "none".into(),
    }
}

/// `guid:copies` lines, comma separated; copies default to 1. A line of an unknown shape is dropped when `lenient`
/// (the cart of a link), else an error.
fn ship_lines(text: &str, lenient: bool) -> Result<Vec<(String, u32)>, String> {
    let mut lines: Vec<(String, u32)> = Vec::new();
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let mut fields = part.split(':');
        let guid = fields.next().unwrap_or("").to_lowercase();
        let copies = match fields.next() {
            None | Some("") => Some(1),
            Some(number) => number.parse::<u32>().ok().filter(|n| (1..=MAX_SHIP_COPIES).contains(n)),
        };
        match copies {
            Some(copies) if valid_class(&guid) && fields.next().is_none() => match lines.iter_mut().find(|(known, _)| *known == guid) {
                Some(line) => line.1 = (line.1 + copies).min(MAX_SHIP_COPIES),
                None => lines.push((guid, copies)),
            },
            _ if lenient => {}
            _ => return Err(format!("{part}: a class GUID, then the number of copies from 1 to {MAX_SHIP_COPIES}.")),
        }
    }
    lines.truncate(MAX_SHIP_LINES);
    Ok(lines)
}

fn cart_text(lines: &[(String, u32)]) -> String {
    lines.iter().map(|(guid, copies)| format!("{guid}:{copies}")).collect::<Vec<_>>().join(",")
}

/// The class of a GUID, as sc-entitygraph has it.
async fn class_of(app: &App, guid: &str) -> Result<Option<sc_api::internal::AdminEntityClass>, Status> {
    let found = app.owners.search_classes(ClassSearch { query: guid.to_string(), limit: 1, ..Default::default() }).await?;
    Ok(found.into_iter().find(|class| class.guid.eq_ignore_ascii_case(guid)))
}

fn class_label(class: &sc_api::internal::AdminEntityClass) -> String {
    if class.display_name.is_empty() { class.class_name.clone() } else { class.display_name.clone() }
}

fn ship_rows(frame: &Frame, id: u32, ships: &[ShipView], names: &HashMap<String, String>, now: i64, actions: bool) -> Vec<Vec<String>> {
    ships
        .iter()
        .filter_map(|view| view.ship.as_ref())
        .map(|ship| {
            let class = names.get(&ship.entity_class_guid).map_or_else(|| format!("<code>{}</code>", escape(&ship.entity_class_guid)), |name| escape(name));
            let claim = ship.claim.as_ref().filter(|claim| claim.ready_at_ms > 0);
            let claim_text = claim.map_or_else(|| "-".into(), |claim| format!("ready {}, cost {} aUEC", time(claim.ready_at_ms), amount(claim.cost_auec as i64)));
            let pad = if ship.pad_size == PadSize::Unspecified as i32 {
                r#"<span class="warn">not set</span>"#.to_string()
            } else {
                pad_name(ship.pad_size).to_string()
            };
            let buttons = if actions && ship.state == ShipState::Deployed as i32 {
                let form = |to: &str, label: &str| {
                    frame.form(
                        &format!("/accounts/{id}{PAGE}/report"),
                        &format!(r#"<input type="hidden" name="geid" value="{}"><input type="hidden" name="to" value="{to}"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#, ship.geid),
                        label,
                    )
                };
                form("stored", "Put away") + &form("destroyed", "Mark destroyed")
            } else {
                String::new()
            };
            vec![
                class,
                if ship.name.is_empty() { String::new() } else { escape(&ship.name) },
                state_name(ship.state).to_string(),
                pad,
                escape(&place_name(ship.location_id)),
                escape(&insurance_text(ship, now)),
                escape(&claim_text),
                ship.geid.to_string(),
                time(ship.created_at_ms),
                buttons,
            ]
        })
        .collect()
}

/// The English names of classes by GUID, those the page needs asked together (and remembered: a class's name does not
/// change). A class that is unknown or cannot be asked is not in the map.
async fn class_names(app: &App, guids: &[String]) -> Vec<Result<Option<String>, Status>> {
    let lookups = guids.iter().map(|guid| async move {
        let key = guid.to_lowercase();
        if let Some(name) = app.class_names.lock().unwrap().get(&key) {
            return Ok(Some(name.clone()));
        }
        let found = class_of(app, &key).await?.map(|class| class_label(&class));
        if let Some(name) = &found {
            let mut known = app.class_names.lock().unwrap();
            if known.len() > 5_000 {
                known.clear();
            }
            known.insert(key, name.clone());
        }
        Ok(found)
    });
    futures_util::future::join_all(lookups).await
}

/// The player's home location document's place, if it has one.
async fn home_place(app: &App, geid: u64) -> Option<u32> {
    let documents = app.owners.documents(geid, vec![HOME.into()]).await.ok()?;
    match find(&documents, HOME)?.document.as_ref()?.body.as_ref()?.class.as_ref()? {
        Class::PlayerHomeLocation(home) => Some(home.home_location_id),
        _ => None,
    }
}

/// The page. Everything it shows comes from calls that do not depend on each other, so they are made together: the
/// page takes as long as the slowest owner, not the sum (the hangar list, the rentals, both entitlement queries, the
/// presence checks, the home location and every class name used to be asked in turn).
pub async fn ships(State(app): State<Shared>, Extension(admin): Extension<Admin>, Path(id): Path<u32>, RawQuery(raw): RawQuery) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let query = query(raw);
    let mut body = flash(&query) + &account_header(&app, &target, PAGE);
    let Some(geid) = target.geid else { return html(frame.page("Ships", &(body + &no_player()))) };
    let cart = ship_lines(query.get("cart").map_or("", String::as_str), true).unwrap_or_default();
    let vehicles = EntitlementFilter { item_type: Some(VEHICLE_ITEM_TYPE.into()), include_revoked: Some(true), ..Default::default() };
    let rented = EntitlementFilter { source_type: vec![EntitlementSourceType::Rental as i32], include_revoked: Some(true), ..Default::default() };
    let (now, listing, rentals, owned, rented, home) = tokio::join!(
        playing(&app, id, geid),
        app.owners.list_ships(geid),
        app.owners.rentals(geid),
        app.owners.entitlements(id, geid, vehicles),
        app.owners.entitlements(id, geid, rented),
        home_place(&app, geid),
    );
    let _ = write!(body, "<p>Player: {}</p>", playing_text(&now));
    let locked = inventory_lock(&now);
    match locked {
        Some(text) => {
            let _ = write!(body, r#"<p class="warn">{}</p>"#, escape(text));
        }
        None => body.push_str(r#"<p class="muted">Changes go to sc-entitygraph, which writes the durable record first (sc-entity-document) and then the ship's entity.</p>"#),
    }

    // The hangar ships, with the class names.
    let mut guids: Vec<String> = Vec::new();
    for guid in listing.as_ref().map(|listing| listing.ships.iter().filter_map(|view| view.ship.as_ref()).map(|ship| ship.entity_class_guid.clone()).collect::<Vec<_>>()).unwrap_or_default().into_iter().chain(cart.iter().map(|(guid, _)| guid.clone())) {
        if !guids.contains(&guid) {
            guids.push(guid);
        }
    }
    let names: HashMap<String, String> = guids.iter().cloned().zip(class_names(&app, &guids).await).filter_map(|(guid, name)| Some((guid, name.ok()??))).collect();
    match &listing {
        Ok(listing) => {
            let mut section = table(
                &["Class", "Name", "State", "Pad size", "Hangar at", "Insurance", "Claim", "Ship", "Acquired", ""],
                &ship_rows(&frame, id, &listing.ships, &names, now_ms(), locked.is_none()),
            );
            let _ = write!(
                section,
                r#"<p class="muted">The list repaired {} ship{} in the entity graph from the records and found {} orphan{} (in the graph with no record, not listed). A ship whose pad size is "not set" is listed for no hangar: its class has no size in the seed. Put away and Mark destroyed apply to a deployed ship left behind by a crashed shard; a destroyed ship is claimed in game.</p>"#,
                listing.repaired,
                if listing.repaired == 1 { "" } else { "s" },
                listing.orphans,
                if listing.orphans == 1 { "" } else { "s" },
            );
            body.push_str(&section_open("hangar", "Ships in hangars", Some(listing.ships.len()), true, &section));
        }
        Err(status) => body.push_str(&section_open("hangar", "Ships in hangars", None, true, &failed("sc-entitygraph", status))),
    }

    if locked.is_none() {
        let searching = ["q", "cart", "all"].iter().any(|key| query.contains_key(*key));
        let inner = grant_section(&app, &frame, id, &query, &cart, &names, home).await;
        body.push_str(&section_open("grant", "Grant ships", (!cart.is_empty()).then_some(cart.iter().map(|(_, copies)| *copies as usize).sum()), searching, &inner));
    }

    // What the account owns and rents: the entitlements behind the ships.
    match (owned, rented) {
        (Ok(mut owned), Ok(rented)) => {
            for entitlement in rented {
                if !owned.iter().any(|known| known.urn == entitlement.urn) {
                    owned.push(entitlement);
                }
            }
            let inner = entitlements_table(&app, &frame, id, &owned).await;
            body.push_str(&section_open("entitlements", "Ship entitlements", Some(owned.len()), false, &inner));
        }
        (Err(status), _) | (_, Err(status)) => body.push_str(&section_open("entitlements", "Ship entitlements", None, false, &failed("sc-entitlement", &status))),
    }
    match &rentals {
        Ok(rentals) => {
            let mut inner = table(&["Class", "Status", "Began", "Expires", "Days", "Renewals", "Rental", ""], &rental_rows(&frame, id, rentals));
            inner.push_str(r#"<p class="muted">Ending a rental makes it expire now: sc-rental revokes its entitlement, then finishes it. Extending is a renewal without the charge.</p>"#);
            body.push_str(&section_open("rentals", "Rentals", Some(rentals.len()), !rentals.is_empty(), &inner));
        }
        Err(status) => body.push_str(&section_open("rentals", "Rentals", None, true, &failed("sc-rental", status))),
    }
    html(frame.page("Ships", &body))
}

/// The vehicle entitlements and every rental, each with the vehicle state saved for its entity; the states are asked
/// together, for the first `VEHICLE_STATES_SHOWN`.
async fn entitlements_table(app: &App, frame: &Frame<'_>, id: u32, owned: &[sc_proto::services::entitlement::v2::Entitlement]) -> String {
    let mut rows = entitlement_rows(frame, id, owned, false);
    let wanted: Vec<Option<u64>> = owned.iter().enumerate().map(|(at, entitlement)| entitlement.entity_geid.filter(|_| at < VEHICLE_STATES_SHOWN)).collect();
    let states = futures_util::future::join_all(wanted.iter().map(|entity| async move {
        match entity {
            Some(entity) => Some(app.owners.documents(*entity, vec!["vehicle_state".into()]).await),
            None => None,
        }
    }))
    .await;
    for ((row, entitlement), state) in rows.iter_mut().zip(owned).zip(states) {
        row.push(match state {
            Some(Ok(documents)) if documents.is_empty() => "no saved state".into(),
            Some(Ok(documents)) => documents.iter().map(document_summary).collect::<Vec<_>>().join("<br>"),
            Some(Err(status)) => format!("sc-entity-document: {}", escape(status.message())),
            None if entitlement.entity_geid.is_some() => "-".into(),
            None => "no entity yet".into(),
        });
    }
    let mut headers = ENTITLEMENT_HEADERS.to_vec();
    headers.push("Saved state");
    let mut out = table(&headers, &rows);
    out.push_str(r#"<p class="muted">The vehicle entitlements and every rental. Where a ship is in the world is the DGS's while it is spawned, and is not stored yet.</p>"#);
    out
}

/// The old Hangar tab's address, which now is the Ships tab.
pub async fn hangar_moved(State(app): State<Shared>, Path(id): Path<u32>, RawQuery(raw): RawQuery) -> Response {
    let query = raw.map(|raw| format!("?{raw}")).unwrap_or_default();
    Redirect::to(&format!("{}/accounts/{id}{PAGE}{query}", app.prefix)).into_response()
}

async fn grant_section(
    app: &App,
    frame: &Frame<'_>,
    id: u32,
    query: &HashMap<String, String>,
    cart: &[(String, u32)],
    names: &HashMap<String, String>,
    home: Option<u32>,
) -> String {
    let mut out = String::new();
    let cart_value = cart_text(cart);
    let text = query.get("q").map_or("", |text| text.trim());
    let everything = query.contains_key("all");
    let _ = write!(
        out,
        r#"<form method="get" action="{prefix}/accounts/{id}{PAGE}#grant" class="inline"><input name="q" value="{q}" maxlength="{MAX_CLASS_QUERY}" size="30" placeholder="Ship name, class name or GUID"><input type="hidden" name="cart" value="{cart}"><label><input type="checkbox" name="all" value="1"{checked}> any class, not only vehicles</label><button type="submit">Find ships</button></form>"#,
        prefix = app.prefix,
        q = escape(text),
        cart = escape(&cart_value),
        checked = if everything { " checked" } else { "" },
    );
    if !text.is_empty() || !everything && query.contains_key("q") {
        let search = ClassSearch {
            query: text.to_string(),
            limit: SEARCH_LIMIT,
            any_of: if everything { Vec::new() } else { vec![(VEHICLE_ITEM_TYPE.to_string(), Vec::new())] },
            named_only: true,
            ..Default::default()
        };
        match app.owners.search_classes(search).await {
            Ok(classes) => {
                let rows: Vec<Vec<String>> = classes
                    .iter()
                    .map(|class| {
                        let mut next = cart.to_vec();
                        let guid = class.guid.to_lowercase();
                        match next.iter().position(|(known, _)| *known == guid) {
                            Some(at) => next[at].1 = (next[at].1 + 1).min(MAX_SHIP_COPIES),
                            None if next.len() < MAX_SHIP_LINES => next.push((guid, 1)),
                            None => {}
                        }
                        vec![
                            escape(&class_label(class)),
                            escape(&class.class_name),
                            format!("<code>{}</code>", escape(&class.guid)),
                            escape(&format!("{} {}", class.item_type, class.item_sub_type)),
                            format!(
                                r##"<a href="{}/accounts/{id}{PAGE}?q={}&cart={}{}#grant">Add</a>"##,
                                app.prefix,
                                query_value(text),
                                query_value(&cart_text(&next)),
                                if everything { "&all=1" } else { "" }
                            ),
                        ]
                    })
                    .collect();
                out.push_str(&table(&["Name", "Class", "GUID", "Type", ""], &rows));
                if classes.len() >= SEARCH_LIMIT as usize {
                    let _ = write!(out, r#"<p class="muted">Only the first {SEARCH_LIMIT}: narrow the search.</p>"#);
                }
                if classes.is_empty() && !everything {
                    out.push_str(r#"<p class="muted">No vehicle class matches. The vehicle filter assumes the item type <code>NOITEM_Vehicle</code>; tick "any class" to search every class.</p>"#);
                }
            }
            Err(status) => out.push_str(&failed("sc-entitygraph", &status)),
        }
    }

    // The cart.
    if cart.is_empty() {
        out.push_str(r#"<p class="muted">Nothing to grant yet: find a ship and press Add.</p>"#);
    } else {
        out.push_str("<h3>To grant</h3><ul>");
        for (index, (guid, copies)) in cart.iter().enumerate() {
            let name = match names.get(guid) {
                Some(name) => escape(name),
                None => format!("<span class=\"warn\">unknown class <code>{}</code></span>", escape(guid)),
            };
            let mut rest = cart.to_vec();
            rest.remove(index);
            let _ = write!(
                out,
                r##"<li>{copies} x {name} <a href="{}/accounts/{id}{PAGE}?q={}&cart={}{}#grant">Remove</a></li>"##,
                app.prefix,
                query_value(text),
                query_value(&cart_text(&rest)),
                if everything { "&all=1" } else { "" },
            );
        }
        out.push_str("</ul>");
    }

    // The form.
    let places = places();
    if places.is_empty() {
        out.push_str(r#"<p class="error">The hangar locations of the game build are not loaded yet (sc-gamedata); try again shortly.</p>"#);
    }
    let options = places.iter().fold(String::new(), |mut out, place| {
        let _ = write!(out, r#"<option value="{}"{}>{} ({})</option>"#, place.id, if Some(place.id) == home { " selected" } else { "" }, escape(&place.name), escape(&place.system));
        out
    });
    let pads: String = [PadSize::Tiny, PadSize::Xsmall, PadSize::Small, PadSize::Medium, PadSize::Large, PadSize::Xlarge]
        .iter()
        .map(|pad| format!(r#"<option value="{}">{}</option>"#, *pad as i32, pad_name(*pad as i32)))
        .collect();
    let fields = format!(
        r#"<input type="hidden" name="ships" value="{cart}"><div class="wide"><span class="muted">Home hangar</span><select name="location" required>{options}</select></div><div class="wide"><span class="muted">Insurance</span><select name="insurance"><option value="none">none</option><option value="duration">for a number of days</option><option value="lifetime">lifetime (LTI)</option></select> <input name="days" type="number" min="1" max="{MAX_INSURANCE_DAYS}" value="365" aria-label="Insurance days"> days (for the duration policy)</div><div class="wide"><span class="muted">Pad size</span><select name="pad"><option value="">from the class's seeded size</option>{pads}</select></div><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason, for the whole grant">"#,
        cart = escape(&cart_value),
    );
    out.push_str(&frame.form_class("stacked", &format!("/accounts/{id}{PAGE}/grant"), &fields, "Grant the ships"));
    out.push_str(r#"<p class="muted">Each ship is stored in the chosen hangar's bank and is the player's at once; granting again with a new form makes new ships. The pad size comes from the class's seeded size (<code>vehicle.maxBoundingBox</code>); a cluster not re-seeded yet has none, and then the pad size must be picked here or the ship is listed for no hangar (the result says so). The claim wait times come from the seed too (<code>vehicle.insurance</code>) and are 0 until it is re-seeded. The vehicle search assumes the item type <code>NOITEM_Vehicle</code>.</p>"#);
    out
}

pub async fn grant(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, "grant", Err(text));
    let lines = match ship_lines(form.get("ships"), false) {
        Ok(lines) if lines.is_empty() => return fail("Add some ships first.".into()),
        Ok(lines) => lines,
        Err(text) => return fail(text),
    };
    if lines.len() > MAX_SHIP_LINES || lines.iter().map(|(_, copies)| copies).sum::<u32>() > MAX_GRANT_SHIPS {
        return fail(format!("At most {MAX_SHIP_LINES} classes and {MAX_GRANT_SHIPS} ships at once."));
    }
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let location: u32 = form.get("location").parse().unwrap_or(0);
    if !places().iter().any(|place| place.id == location) {
        return fail("Pick a home hangar from the list.".into());
    }
    let pad = match form.get("pad") {
        "" => PadSize::Unspecified,
        text => match text.parse::<i32>().ok().and_then(|pad| PadSize::try_from(pad).ok()).filter(|pad| *pad != PadSize::Unspecified) {
            Some(pad) => pad,
            None => return fail("Pick a pad size from the list.".into()),
        },
    };
    let (policy, expires_at_ms) = match form.get("insurance") {
        "" | "none" => (ShipPolicy::Unspecified, 0),
        "lifetime" => (ShipPolicy::Lifetime, 0),
        "duration" => match form.get("days").parse::<u32>() {
            Ok(days) if (1..=MAX_INSURANCE_DAYS).contains(&days) => (ShipPolicy::Duration, now_ms() + i64::from(days) * DAY_MS),
            _ => return fail(format!("An insurance of 1 to {MAX_INSURANCE_DAYS} days.")),
        },
        _ => return fail("Pick an insurance from the list.".into()),
    };
    // Every class must exist before anything is granted.
    let guids: Vec<String> = lines.iter().map(|(guid, _)| guid.clone()).collect();
    let mut named = Vec::new();
    for (guid, found) in guids.iter().zip(class_names(&app, &guids).await) {
        match found {
            Ok(Some(name)) => named.push(name),
            Ok(None) => return fail(format!("No class {guid}: run sc-admin seed.")),
            Err(status) => return fail(format!("sc-entitygraph: {}", status.message())),
        }
    }
    let geid = match offline_player(&app, id).await {
        Ok(geid) => geid,
        Err(text) => return fail(text),
    };
    let listed: Vec<Value> = lines.iter().zip(&named).map(|((guid, copies), name)| json!({ "class": guid, "name": name, "copies": copies })).collect();
    let entry = Entry {
        action: "ship.grant".into(),
        owner: "sc-entitygraph".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({
            "ships": listed,
            "location": location,
            "insurance": form.get("insurance"),
            "expires_at_ms": expires_at_ms,
            "pad_size": pad_name(pad as i32),
            "reason": reason,
        }),
        ..Default::default()
    };
    let base = key(&form);
    let result = act(&app, &admin, &form, entry, async {
        let total: u32 = lines.iter().map(|(_, copies)| copies).sum();
        let (mut done, mut granted, mut unsized_ships) = (0u32, Vec::new(), 0u32);
        for ((guid, copies), name) in lines.iter().zip(&named) {
            for _ in 0..*copies {
                let request = AcquireShipRequest {
                    command_id: format!("{base}:{done}"),
                    owner_id: geid,
                    entity_class_guid: guid.clone(),
                    location_id: location,
                    insurance: Some(ShipInsurance { policy: policy as i32, expires_at_ms, ..Default::default() }),
                    pad_size: pad as i32,
                    ..Default::default()
                };
                match app.owners.acquire_ship(request).await {
                    Ok(reply) => {
                        let ship = reply.ship.and_then(|view| view.ship).unwrap_or_default();
                        if ship.pad_size == PadSize::Unspecified as i32 {
                            unsized_ships += 1;
                        }
                        granted.push(json!({ "geid": ship.geid, "class": guid, "name": name, "pad_size": pad_name(ship.pad_size), "replayed": reply.replayed, "graph_synced": reply.graph_synced }));
                    }
                    Err(status) => return Err(Status::new(status.code(), format!("granted {done} of {total}, then {name}: {}", status.message()))),
                }
                done += 1;
            }
        }
        let summary = lines.iter().zip(&named).map(|((_, copies), name)| format!("{copies}x {name}")).collect::<Vec<_>>().join(", ");
        let mut text = format!("Granted {done} ship{} at {}: {summary}.", if done == 1 { "" } else { "s" }, place_name(location));
        if unsized_ships > 0 {
            let _ = write!(text, " {unsized_ships} of them have no pad size (the class is not seeded with a size): they are listed for no hangar until one is set.");
        }
        Ok((text, (None, Some(json!({ "location": location, "ships": granted })))))
    })
    .await;
    back_at(&app, id, PAGE, "hangar", result)
}

pub async fn report(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, "hangar", Err(text));
    let Ok(ship) = form.get("geid").parse::<u64>() else { return fail("Pick a ship.".into()) };
    let state = match form.get("to") {
        "stored" => ShipState::Stored,
        "destroyed" => ShipState::Destroyed,
        _ => return fail("A deployed ship is put away or marked destroyed.".into()),
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let geid = match offline_player(&app, id).await {
        Ok(geid) => geid,
        Err(text) => return fail(text),
    };
    // The ship as it is now: it must be the player's, and it is put away where its record says it belongs.
    let before = match app.owners.list_ships(geid).await {
        Ok(listing) => listing.ships.into_iter().filter_map(|view| view.ship).find(|found| found.geid == ship),
        Err(status) => return fail(format!("sc-entitygraph: {}", status.message())),
    };
    let Some(before) = before else { return fail(format!("The player has no ship {ship}.")) };
    let entry = Entry {
        action: if state == ShipState::Stored { "ship.store" } else { "ship.destroy" }.into(),
        owner: "sc-entitygraph".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "ship": ship, "to": state_name(state as i32), "reason": reason }),
        ..Default::default()
    };
    let request = ReportShipRequest { command_id: key(&form), owner_id: geid, geid: ship, state: state as i32, location_id: before.location_id, now_ms: 0 };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.report_ship(request).await?;
        let now = reply.ship.and_then(|view| view.ship).unwrap_or_default();
        let text = format!("Ship {ship} is {}.{}", state_name(now.state), if reply.replayed { " (already applied)" } else { "" });
        Ok((text, (Some(json!({ "state": state_name(before.state) })), Some(json!({ "state": state_name(now.state), "location": now.location_id })))))
    })
    .await;
    back_at(&app, id, PAGE, "hangar", result)
}
