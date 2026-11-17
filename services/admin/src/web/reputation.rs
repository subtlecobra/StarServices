//! The Reputation tab: the player's reputations with the standing each score is in (sc-reputation's `ReputationInternal`),
//! the scope's icon from the game's mobiGlas, and a form that moves a score by a number of points. The icons are the
//! game's own vector files, kept in `assets/reputation` and served from here.

use super::*;
use sc_api::internal::{AdjustReputationRequest, DeleteReputationRequest, LadderStanding, ReputationEntry, ScopeSummary, SetReputationLockRequest};
use sc_runtime::player_urn;

const PAGE: &str = "/reputation";
/// The most points one change moves a score by.
const MAX_DELTA: i32 = 100_000;
/// The icon file shown for a scope the game draws no icon for: none.
const NO_ICON: &str = "";
/// The longest entity name accepted.
const MAX_ENTITY_BYTES: usize = 200;
/// The scope every faction's context lists first: the game's general affinity, not a reputation to give.
const HIDDEN_SCOPE: &str = "Affinity";
/// How many hexadecimal digits a color has in an icon's SVG.
const HEX_COLOR_DIGITS: usize = 6;
/// The weights of red, green and blue in a luminance, and what they add up to.
const LUMA_RED: u32 = 299;
const LUMA_GREEN: u32 = 587;
const LUMA_BLUE: u32 = 114;
const LUMA_SCALE: u32 = 1000;
/// The mean luminance from which an icon is light (and goes on a dark backing).
const LIGHT_ICON_LUMINANCE: u32 = 140;
/// The share of the scopes (in percent) two groups of factions need in common to be shown in one matrix.
const MERGE_SIMILARITY_PERCENT: usize = 50;
/// The points between the last of a level and the first of the next.
const RANK_GAP: i64 = 1;
/// A completed level.
const PERCENT: u32 = 100;
/// The anchor of the section, for the redirect after a change.
const SECTION: &str = "reputations";
/// The anchor of the editor, the query names of the selected cell, and the editor's text before a reputation exists.
const EDITOR_ANCHOR: &str = "rep-editor";
const ENTITY_QUERY: &str = "e";
const SCOPE_QUERY: &str = "s";
const EMPTY_STATE: &str = "no reputation yet";

include!(concat!(env!("OUT_DIR"), "/reputation_icons.rs"));
include!(concat!(env!("OUT_DIR"), "/factions.rs"));

/// The tone word of a light logo in the factions table.
const TONE_LIGHT: &str = "light";
/// Separates the scope names in the last column of the factions table.
const SCOPE_SEPARATOR: &str = ",";
/// What the entity of a faction's reputation starts with, before the faction's GEID.
const FACTION_URN: &str = "urn:sc:global:org:geid:";

/// A faction of the game data: the GEID that identifies it in a reputation, its name and its logo file, if it has one.
pub struct Faction {
    pub geid: u64,
    pub name: String,
    pub logo: Option<String>,
    /// Whether the logo is light, so that it is shown on a dark backing.
    pub light: bool,
    /// The scopes the game gives the faction a reputation in (their names in the reputation service).
    pub scopes: Vec<String>,
}

/// The factions the build knows, by name.
pub fn factions() -> &'static [Faction] {
    static KNOWN: std::sync::OnceLock<Vec<Faction>> = std::sync::OnceLock::new();
    KNOWN.get_or_init(|| {
        let mut known: Vec<Faction> = FACTIONS
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let geid = fields.next()?.parse().ok()?;
                let name = crate::localization::text(fields.next().unwrap_or_default());
                let file = fields.next();
                let light = fields.next() == Some(TONE_LIGHT);
                let scopes = fields.next().unwrap_or_default().split(SCOPE_SEPARATOR).filter(|scope| !scope.is_empty()).map(str::to_string).collect();
                let logo = file.filter(|file| !file.is_empty() && FACTION_LOGOS.iter().any(|(known, _)| known == file)).map(str::to_string);
                // A faction with no name in the game's texts is not one the game shows.
                (!name.is_empty()).then_some(Faction { geid, name, logo, light, scopes })
            })
            .collect();
        known.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        known
    })
}

/// The GEID an entity names: the faction urn, or the number alone.
fn entity_geid(entity: &str) -> Option<u64> {
    entity.strip_prefix(FACTION_URN).unwrap_or(entity).parse().ok()
}

/// The faction an entity is, when the game data has it.
fn faction_of(entity: &str) -> Option<&'static Faction> {
    let geid = entity_geid(entity)?;
    factions().iter().find(|faction| faction.geid == geid)
}

/// The entity of a faction in a reputation.
fn faction_entity(faction: &Faction) -> String {
    format!("{FACTION_URN}{}", faction.geid)
}

/// `GET /static/faction/{file}`: one of the logos.
pub async fn faction_logo(Path(file): Path<String>) -> Response {
    match FACTION_LOGOS.iter().find(|(name, _)| *name == file) {
        Some((_, png)) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], *png).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The header icon file of each scope the game draws one for, by the scope's lowercase name. The files come from the game
/// data (`scripts/game-files.sh`); a scope whose file was not found at build time shows no icon.
const SCOPE_ICONS: [(&str, &str); 11] = [
    ("assassination", "icon_reputationrank_assassination_header.svg"),
    ("bountyhunter", "icon_reputationrank_bountyhunter_header.svg"),
    ("courier", "icon_reputationrank_courier_header.svg"),
    ("emergency", "icon_reputationrank_emergencyservices_header.svg"),
    ("hiredmuscle", "icon_reputationrank_hiredmuscle_header.svg"),
    ("investigationandretrieval", "icon_reputationrank_investigationandretrieval_header.svg"),
    ("security", "icon_reputationrank_security_header.svg"),
    ("smuggling", "icon_reputationrank_smuggling_header.svg"),
    ("theft", "icon_reputationrank_theft_header.svg"),
    ("technician", "icon_reputation_technician_header.svg"),
    ("racingship", "icon_spaceshipracing_header.svg"),
];

/// Whether the build found the game's icons.
pub fn has_icons() -> bool {
    !ICONS.is_empty()
}

/// The name of a scope as the player reads it: its localized display name, else the scope's own name.
fn scope_name(display_name: &str, scope: &str) -> String {
    let name = crate::localization::text(display_name);
    if name.is_empty() || name.starts_with("RepScope_") { scope.replace('_', " ") } else { name }
}

/// The embedded text of an icon file.
fn icon_text(file: &str) -> Option<&'static str> {
    ICONS.iter().find(|(name, _)| *name == file).map(|(_, svg)| *svg)
}

/// The file of the icon for a scope, by its display name or its scope name; empty when there is none or it was not built in.
fn scope_icon(display_name: &str, scope: &str) -> &'static str {
    let plain = |name: &str| name.to_ascii_lowercase().replace([' ', '_'], "");
    let (display, scope) = (plain(display_name), plain(scope));
    SCOPE_ICONS.iter().find(|(name, _)| *name == display || *name == scope).map(|(_, file)| *file).filter(|file| icon_text(file).is_some()).unwrap_or(NO_ICON)
}

/// `GET /static/reputation/{file}`: one of the icons.
pub async fn icon(Path(file): Path<String>) -> Response {
    match icon_text(&file) {
        Some(svg) => ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")], svg).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn adjust(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, SECTION, Err(text));
    let Ok(delta) = form.get("delta").trim().parse::<i32>() else { return fail("Points are a whole number.".into()) };
    if delta == 0 || delta.abs() > MAX_DELTA {
        return fail(format!("A change is 1 to {MAX_DELTA} points, up or down."));
    }
    let (entity, scope) = (form.get("entity").to_string(), form.get("scope").to_string());
    if entity.is_empty() || scope.is_empty() {
        return fail("Pick a reputation.".into());
    }
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let Some(geid) = player(&app, id).await else { return fail("No player for this account.".into()) };
    let entry = Entry {
        action: "reputation.adjust".into(),
        owner: "sc-reputation".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "entity": entity, "scope": scope, "delta": delta, "reason": reason }),
        ..Default::default()
    };
    let place = place_of(&entity, &scope);
    let request = AdjustReputationRequest { subject: player_urn(geid), entity, scope, delta, actor: format!("admin:{}", admin.handle), reason };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.adjust_reputation(request).await?;
        Ok((format!("The score is {} (was {}).", reply.score, reply.previous), (Some(json!({ "score": reply.previous })), Some(json!({ "score": reply.score })))))
    })
    .await;
    back_at(&app, id, &place, EDITOR_ANCHOR, result)
}

/// Parses an optional whole number: an empty field is none.
fn optional(form: &Form, name: &str) -> Result<Option<i32>, String> {
    match form.get(name).trim() {
        "" => Ok(None),
        text => text.parse::<i32>().map(Some).map_err(|_| "A number is a whole number.".to_string()),
    }
}

/// Starts a reputation in a scope of the game; the points, when given, are applied on top of the initial value.
pub async fn add(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, SECTION, Err(text));
    // A faction picked from the badges, else the entity written out.
    let picked = form.get("faction").trim();
    let entity = if picked.is_empty() { form.get("entity").trim().to_string() } else { picked.to_string() };
    let scope = form.get("scope").to_string();
    if entity.is_empty() || entity.len() > MAX_ENTITY_BYTES {
        return fail(format!("Pick a faction, or write who it is with (1 to {MAX_ENTITY_BYTES} characters)."));
    }
    let delta = match optional(&form, "delta") {
        Ok(delta) => delta.unwrap_or(0),
        Err(text) => return fail(text),
    };
    if delta.abs() > MAX_DELTA {
        return fail(format!("Extra points are at most {MAX_DELTA}."));
    }
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    // The score the reputation is created at (from its rank and progress), else the scope's initial one plus the extra points.
    let score = match optional(&form, "score") {
        Ok(score) => score,
        Err(text) => return fail(text),
    };
    let delta = match app.owners.reputation_scopes().await {
        Ok(scopes) => match scopes.iter().find(|known| known.scope == scope) {
            Some(known) => match score {
                Some(score) => score.saturating_sub(known.initial_reputation),
                None => delta,
            },
            None => return fail("That scope is not one of the game's.".into()),
        },
        Err(status) => return fail(format!("sc-reputation: {}", status.message())),
    };
    let Some(geid) = player(&app, id).await else { return fail("No player for this account.".into()) };
    let entry = Entry {
        action: "reputation.add".into(),
        owner: "sc-reputation".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "entity": entity, "scope": scope, "delta": delta, "reason": reason }),
        ..Default::default()
    };
    let place = place_of(&entity, &scope);
    let request = AdjustReputationRequest { subject: player_urn(geid), entity, scope, delta, actor: format!("admin:{}", admin.handle), reason };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.adjust_reputation(request).await?;
        Ok((format!("The reputation is at {}.", reply.score), (Some(json!({ "score": reply.previous })), Some(json!({ "score": reply.score })))))
    })
    .await;
    back_at(&app, id, &place, EDITOR_ANCHOR, result)
}

/// Sets a score to a value: the change is the difference from the score now, so the ceiling and the lock still hold.
pub async fn set(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, SECTION, Err(text));
    let Ok(target) = form.get("score").trim().parse::<i32>() else { return fail("The score is a whole number.".into()) };
    let (entity, scope) = (form.get("entity").to_string(), form.get("scope").to_string());
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let Some(geid) = player(&app, id).await else { return fail("No player for this account.".into()) };
    let subject = player_urn(geid);
    let now = match app.owners.reputations(&subject).await {
        Ok(entries) => entries.into_iter().find(|found| found.entity == entity && found.scope == scope).map(|found| found.score),
        Err(status) => return fail(format!("sc-reputation: {}", status.message())),
    };
    let Some(now) = now else { return fail("The player has no such reputation.".into()) };
    let delta = i64::from(target) - i64::from(now);
    let Ok(delta) = i32::try_from(delta) else { return fail("That score is too far from the current one.".into()) };
    let entry = Entry {
        action: "reputation.set".into(),
        owner: "sc-reputation".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "entity": entity, "scope": scope, "to": target, "from": now, "reason": reason }),
        ..Default::default()
    };
    let place = place_of(&entity, &scope);
    let request = AdjustReputationRequest { subject, entity, scope, delta, actor: format!("admin:{}", admin.handle), reason };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.adjust_reputation(request).await?;
        Ok((format!("The score is {} (was {}).", reply.score, reply.previous), (Some(json!({ "score": reply.previous })), Some(json!({ "score": reply.score })))))
    })
    .await;
    back_at(&app, id, &place, EDITOR_ANCHOR, result)
}

/// Locks a score inside a range; both bounds empty removes the lock.
pub async fn lock(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, SECTION, Err(text));
    let (lock_min, lock_max) = match (optional(&form, "lock_min"), optional(&form, "lock_max")) {
        (Ok(low), Ok(high)) => (low, high),
        (Err(text), _) | (_, Err(text)) => return fail(text),
    };
    if let (Some(low), Some(high)) = (lock_min, lock_max) {
        if low > high {
            return fail("The lock's minimum is above its maximum.".into());
        }
    }
    let (entity, scope) = (form.get("entity").to_string(), form.get("scope").to_string());
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let Some(geid) = player(&app, id).await else { return fail("No player for this account.".into()) };
    let entry = Entry {
        action: "reputation.lock".into(),
        owner: "sc-reputation".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "entity": entity, "scope": scope, "lock_min": lock_min, "lock_max": lock_max, "reason": reason }),
        ..Default::default()
    };
    let place = place_of(&entity, &scope);
    let request = SetReputationLockRequest { subject: player_urn(geid), entity, scope, lock_min, lock_max, actor: format!("admin:{}", admin.handle), reason };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.lock_reputation(request).await?;
        let text = if lock_min.is_none() && lock_max.is_none() { format!("The lock is removed; the score is {}.", reply.score) } else { format!("The score is locked; it is {}.", reply.score) };
        Ok((text, (None, Some(json!({ "lock_min": lock_min, "lock_max": lock_max, "score": reply.score })))))
    })
    .await;
    back_at(&app, id, &place, EDITOR_ANCHOR, result)
}

/// Removes a reputation and its history. It needs the confirmation box.
pub async fn delete(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>, Path(id): Path<u32>) -> Response {
    let fail = |text: String| back_at(&app, id, PAGE, SECTION, Err(text));
    if form.get("confirm") != "yes" {
        return fail("Tick the box to delete the reputation and its history.".into());
    }
    let (entity, scope) = (form.get("entity").to_string(), form.get("scope").to_string());
    if entity.is_empty() || scope.is_empty() {
        return fail("Pick a reputation.".into());
    }
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return fail(text),
    };
    let Some(geid) = player(&app, id).await else { return fail("No player for this account.".into()) };
    let entry = Entry {
        action: "reputation.delete".into(),
        owner: "sc-reputation".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "entity": entity, "scope": scope, "reason": reason }),
        ..Default::default()
    };
    let place = place_of(&entity, &scope);
    let request = DeleteReputationRequest { subject: player_urn(geid), entity, scope, actor: format!("admin:{}", admin.handle), reason };
    let result = act(&app, &admin, &form, entry, async {
        let reply = app.owners.delete_reputation(request).await?;
        Ok((format!("The reputation is deleted (its score was {}).", reply.score), (Some(json!({ "score": reply.score })), None)))
    })
    .await;
    back_at(&app, id, &place, EDITOR_ANCHOR, result)
}

/// A tile that gives an icon a backing to be seen on, whatever the page's colors: dark for a light icon, light for a dark one.
fn tile(inner: &str, on_dark: bool, size: u32) -> String {
    format!(r#"<span class="logo-tile {}" style="width:{size}px;height:{size}px">{inner}</span>"#, if on_dark { "on-dark" } else { "on-light" })
}

/// A faction's logo on its tile or, for one with no logo, its first letter.
fn faction_image(prefix: &str, faction: Option<&Faction>, name: &str, size: u32) -> String {
    let inner_size = size.saturating_sub(8);
    match faction.and_then(|faction| faction.logo.as_deref().map(|file| (file, faction.light))) {
        Some((file, light)) => tile(&format!(r#"<img src="{prefix}/static/faction/{file}" alt="" width="{inner_size}" height="{inner_size}">"#), light, size),
        None => tile(&format!(r#"<span class="logo-letter">{}</span>"#, escape(&name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default())), true, size),
    }
}

/// Whether an icon is light: the mean luminance of the colors its SVG fills with, so that it gets a dark backing.
fn icon_is_light(icon: &str) -> bool {
    let Some(svg) = icon_text(icon) else { return true };
    let (mut total, mut count) = (0u32, 0u32);
    for part in svg.split('#').skip(1) {
        let hex: String = part.chars().take(HEX_COLOR_DIGITS).collect();
        if hex.len() < HEX_COLOR_DIGITS || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let channel = |at: usize| u32::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
        total += (channel(0) * LUMA_RED + channel(2) * LUMA_GREEN + channel(4) * LUMA_BLUE) / LUMA_SCALE;
        count += 1;
    }
    count == 0 || total / count >= LIGHT_ICON_LUMINANCE
}

/// A scope's icon on a backing that contrasts with it, or an empty tile.
fn scope_image(prefix: &str, icon: &str, size: u32) -> String {
    let inner_size = size.saturating_sub(8);
    if icon.is_empty() {
        return tile("", true, size);
    }
    tile(&format!(r#"<img src="{prefix}/static/reputation/{icon}" alt="" width="{inner_size}" height="{inner_size}">"#), icon_is_light(icon), size)
}

/// Where a score stands on its ladder: the index of its level, and how far it is to the next as a percentage. A level
/// spans its minimum to one point short of the next level's; the top level is always 100.
fn progress(ladder: &[LadderStanding], score: i32) -> Option<(usize, u32)> {
    let level = ladder.iter().rposition(|standing| standing.min <= score).unwrap_or(0);
    let current = ladder.get(level)?;
    let Some(next) = ladder.get(level + 1) else { return Some((level, PERCENT)) };
    let span = (i64::from(next.min) - i64::from(current.min) - RANK_GAP).max(1);
    let done = (i64::from(score) - i64::from(current.min)).clamp(0, span);
    Some((level, ((done * i64::from(PERCENT) * 2 + span) / (span * 2)) as u32))
}

/// The text under a score's level: the level's name and how far it is to the next (the script on the page rewrites it as the
/// rank badges are dragged, in the same words).
fn level_text(ladder: &[LadderStanding], score: i32) -> String {
    match progress(ladder, score) {
        Some((at, percent)) => {
            let name = crate::localization::text(&ladder[at].name);
            match ladder.get(at + 1) {
                Some(next) => format!("{name} \u{b7} {percent}% to {}", crate::localization::text(&next.name)),
                None => format!("{name} \u{b7} {percent}%"),
            }
        }
        None => String::new(),
    }
}

/// Percent-encodes a value for a query string.
fn url_component(text: &str) -> String {
    form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// The page address that selects a cell of the matrix: what the redirect after a change returns to.
fn place_of(entity: &str, scope: &str) -> String {
    format!("{PAGE}?{ENTITY_QUERY}={}&{SCOPE_QUERY}={}", url_component(entity), url_component(scope))
}

/// Whether the matrix offers the cell: the game gives the faction the scope, or the player already has the reputation.
fn offers(entity: &str, scope: &ScopeSummary, has_entry: bool) -> bool {
    has_entry || faction_of(entity).is_some_and(|faction| faction.scopes.contains(&scope.scope))
}

/// The name an entity is shown with: the faction's, or the entity itself.
fn entity_name(entity: &str) -> String {
    faction_of(entity).map_or_else(|| entity.to_string(), |faction| faction.name.clone())
}

/// One cell of the matrix: a link that selects it (the page script opens it without leaving the page). With a reputation
/// it shows the rank and a bar of the progress in it; without, a plus; a scope the game does not give the faction is dotted.
fn cell(app: &App, id: u32, entity: &str, scope: &ScopeSummary, entry: Option<&ReputationEntry>, selected: bool) -> String {
    if !offers(entity, scope, entry.is_some()) {
        return r#"<span class="rep-cell rep-na" aria-hidden="true">&ndash;</span>"#.into();
    }
    // The open cell links to the page with nothing selected: pressing it again closes its editor.
    let href = if selected { format!("{}/accounts/{id}{PAGE}", app.prefix) } else { format!("{}/accounts/{id}{}#{EDITOR_ANCHOR}", app.prefix, escape(&place_of(entity, &scope.scope))) };
    let on = if selected { " on" } else { "" };
    let label = format!("{}, {}", entity_name(entity), scope_name(&scope.display_name, &scope.scope));
    match entry {
        Some(entry) => {
            let (level, percent) = progress(&scope.ladder, entry.score).unwrap_or((0, 0));
            let rank = scope.ladder.get(level).map_or_else(|| entry.standing.clone(), |standing| crate::localization::text(&standing.name));
            format!(r#"<a class="rep-cell{on}" href="{href}" aria-label="{}: {}"><b>{}</b><span class="rep-mini"><i style="width:{percent}%"></i></span></a>"#, escape(&label), escape(&rank), escape(&rank))
        }
        None => format!(r#"<a class="rep-cell rep-empty{on}" href="{href}" aria-label="{}: no reputation yet">+</a>"#, escape(&label)),
    }
}

/// The matrix: a row for each faction (the ones the player has reputations with first), a column for each scope.
fn matrix(app: &App, id: u32, entities: &[String], columns: &[&ScopeSummary], entries: &[ReputationEntry], selected: Option<(&str, &str)>) -> String {
    let head: String = columns
        .iter()
        .map(|scope| format!(r#"<span class="rep-mcol">{}{}</span>"#, scope_image(&app.prefix, scope_icon(&scope.display_name, &scope.scope), 24), escape(&scope_name(&scope.display_name, &scope.scope))))
        .collect();
    let rows: String = entities
        .iter()
        .map(|entity| {
            let faction = faction_of(entity);
            let name = entity_name(entity);
            let cells: String = columns
                .iter()
                .map(|scope| {
                    let entry = entries.iter().find(|entry| entry.entity == *entity && entry.scope == scope.scope);
                    cell(app, id, entity, scope, entry, selected == Some((entity.as_str(), scope.scope.as_str())))
                })
                .collect();
            format!(r#"<div class="rep-mrow"><span class="rep-mname">{}{}</span>{cells}</div>"#, faction_image(&app.prefix, faction, &name, 28), escape(&name))
        })
        .collect();
    format!(r#"<div class="rep-matrix" style="--rep-cols:{}"><div class="rep-mrow rep-mhead"><span></span>{head}</div>{rows}</div>"#, columns.len())
}

/// The editor of the selected cell: the ranks of the scope as badges that also set the progress (dragged across, in the
/// page script), the exact score to type, and Save or Create. Delete, adding points and locking come with it.
fn editor(app: &App, frame: &Frame<'_>, id: u32, entity: &str, scope: &ScopeSummary, entry: Option<&ReputationEntry>) -> String {
    let ladder = scope.ladder.as_slice();
    let name = entity_name(entity);
    let faction = faction_of(entity);
    let scope_title = scope_name(&scope.display_name, &scope.scope);
    let state = match entry {
        Some(entry) => level_text(ladder, entry.score),
        None => EMPTY_STATE.to_string(),
    };
    let crumb = format!(
        r#"<div class="rep-crumb">{}<h2>{}</h2><span class="muted">&rsaquo;</span>{}<h2>{}</h2><span class="rep-state" id="rep-state" data-empty="{EMPTY_STATE}">{}</span></div>"#,
        faction_image(&app.prefix, faction, &name, 36),
        escape(&name),
        scope_image(&app.prefix, scope_icon(&scope.display_name, &scope.scope), 28),
        escape(&scope_title),
        escape(&state)
    );
    let at = entry.and_then(|entry| progress(ladder, entry.score));
    let ranks: String = ladder
        .iter()
        .enumerate()
        .map(|(index, standing)| {
            let (current, fill) = match at {
                Some((level, percent)) => (index == level, if index < level { PERCENT } else if index == level { percent } else { 0 }),
                None => (false, 0),
            };
            let next = ladder.get(index + 1).map_or(String::new(), |next| next.min.to_string());
            let class = format!("rep-rank{}{}", if current { " cur" } else { "" }, if fill == PERCENT { " full" } else { "" });
            format!(
                r#"<div class="{class}" role="slider" tabindex="{}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="{fill}" aria-label="{}" data-min="{}" data-next="{next}"><span class="rep-fill" style="width:{fill}%"></span><span class="rep-rbody"><b>{}</b><span class="rep-rmin">{}+</span><span class="rep-pc">{}</span></span></div>"#,
                if current || (at.is_none() && index == 0) { 0 } else { -1 },
                escape(&crate::localization::text(&standing.name)),
                standing.min,
                escape(&crate::localization::text(&standing.name)),
                standing.min,
                if current { if next.is_empty() { "complete".to_string() } else { format!("{fill}%") } } else { String::new() }
            )
        })
        .collect();
    let hint = if entry.is_some() {
        "Drag across a rank badge to set the progress inside it. Touch another badge to change rank. Left and right keys move the progress, up and down the rank."
    } else {
        "No reputation here yet. Touch a rank badge to start it there, then press Create."
    };
    let hidden = format!(r#"<input type="hidden" name="entity" value="{}"><input type="hidden" name="scope" value="{}">"#, escape(entity), escape(&scope.scope));
    let reason = format!(r#"<input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason" class="rep-reason" aria-label="Reason">"#);
    let score = entry.map_or(String::new(), |entry| entry.score.to_string());
    let (action, button) = if entry.is_some() { ("set", "Save") } else { ("add", "Create") };
    let save = frame.form_class(
        "rep-set",
        &format!("/accounts/{id}{PAGE}/{action}"),
        &format!(r#"{hidden}<label class="muted">Score <input type="number" name="score" class="rep-score" required value="{score}"></label>{reason}"#),
        button,
    );
    let Some(entry) = entry else {
        return format!(r#"{crumb}<div class="rep-ranks">{ranks}</div><p class="muted rep-hint">{hint}</p>{save}"#);
    };
    let bound = |value: Option<i32>| value.map_or("-".to_string(), |value| value.to_string());
    let locked = if entry.lock_min.is_none() && entry.lock_max.is_none() { String::new() } else { format!(r#"<p class="muted">Locked {} to {}.</p>"#, bound(entry.lock_min), bound(entry.lock_max)) };
    let number = |name: &str, label: &str| format!(r#"<input name="{name}" type="number" placeholder="{label}">"#);
    let remove = frame.form_class(
        "rep-delete",
        &format!("/accounts/{id}{PAGE}/delete"),
        &format!(r#"{hidden}<label><input type="checkbox" name="confirm" value="yes" required> Delete this reputation and its history</label>{reason}"#),
        "Delete",
    );
    let change = frame.form(
        &format!("/accounts/{id}{PAGE}/adjust"),
        &format!(r#"{hidden}<input name="delta" type="number" required min="-{MAX_DELTA}" max="{MAX_DELTA}" placeholder="Points (+/-)">{reason}"#),
        "Add points",
    );
    let locking = frame.form(&format!("/accounts/{id}{PAGE}/lock"), &format!("{hidden}{}{}{reason}", number("lock_min", "Lock min"), number("lock_max", "Lock max")), "Lock");
    format!(
        r#"{crumb}<div class="rep-ranks">{ranks}</div><p class="muted rep-hint">{hint}</p>{save}{remove}{locked}<details class="rep-more"><summary>More</summary>{change}{locking}</details>"#
    )
}

pub async fn reputation(State(app): State<Shared>, Extension(admin): Extension<Admin>, Path(id): Path<u32>, RawQuery(raw): RawQuery) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let query = query(raw);
    let mut body = flash(&query) + &account_header(&app, &target, PAGE);
    let Some(geid) = target.geid else { return html(frame.page("Reputation", &(body + &no_player()))) };
    let subject = player_urn(geid);
    let (entries, scopes) = tokio::join!(app.owners.reputations(&subject), app.owners.reputation_scopes());
    let entries = match entries {
        Ok(entries) => entries,
        Err(status) => return html(frame.page("Reputation", &(body + &failed("sc-reputation", &status)))),
    };
    let scopes = scopes.unwrap_or_default();
    // Every faction of the game is a row, and so is any other entity the player has a reputation with.
    let mut entities: Vec<String> = Vec::new();
    for entity in entries.iter().map(|entry| entry.entity.clone()).chain(factions().iter().map(faction_entity)) {
        if !entities.contains(&entity) {
            entities.push(entity);
        }
    }
    // The ones the player has reputations with come first, then by name.
    entities.sort_by_key(|entity| (!entries.iter().any(|entry| entry.entity == *entity), entity_name(entity).to_lowercase()));
    // The scopes a row offers; the rows that offer the same (or nearly) share a matrix, so a matrix has few columns.
    let candidates: Vec<&ScopeSummary> = scopes.iter().filter(|scope| scope.scope != HIDDEN_SCOPE).collect();
    let mut groups: Vec<(std::collections::BTreeSet<usize>, Vec<String>)> = Vec::new();
    for entity in &entities {
        let set: std::collections::BTreeSet<usize> = (0..candidates.len())
            .filter(|at| offers(entity, candidates[*at], entries.iter().any(|entry| entry.entity == *entity && entry.scope == candidates[*at].scope)))
            .collect();
        if set.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|group| group.0 == set) {
            Some(group) => group.1.push(entity.clone()),
            None => groups.push((set, vec![entity.clone()])),
        }
    }
    // Groups that are alike enough become one, the most alike first.
    loop {
        let mut best: Option<(usize, usize, usize)> = None;
        for first in 0..groups.len() {
            for second in first + 1..groups.len() {
                let shared = groups[first].0.intersection(&groups[second].0).count();
                let all = groups[first].0.union(&groups[second].0).count();
                let similarity = shared * PERCENT as usize / all.max(1);
                if similarity >= MERGE_SIMILARITY_PERCENT && best.is_none_or(|(most, _, _)| similarity > most) {
                    best = Some((similarity, first, second));
                }
            }
        }
        let Some((_, first, second)) = best else { break };
        let other = groups.remove(second);
        groups[first].0.extend(other.0);
        groups[first].1.extend(other.1);
    }
    let has_entries = |group: &(std::collections::BTreeSet<usize>, Vec<String>)| group.1.iter().any(|entity| entries.iter().any(|entry| entry.entity == *entity));
    groups.sort_by_key(|group| (!has_entries(group), std::cmp::Reverse(group.1.len()), group.0.iter().next().copied()));
    // The cell asked for, if any: nothing is open until one is.
    let asked = query.get(ENTITY_QUERY).zip(query.get(SCOPE_QUERY)).map(|(entity, scope)| (entity.as_str(), scope.as_str()));
    let mut sections = String::new();
    for (set, members) in &groups {
        let mut columns: Vec<&ScopeSummary> = set.iter().map(|at| candidates[*at]).collect();
        columns.sort_by_key(|scope| scope_name(&scope.display_name, &scope.scope).to_lowercase());
        let mut rows = members.clone();
        rows.sort_by_key(|entity| (!entries.iter().any(|entry| entry.entity == *entity), entity_name(entity).to_lowercase()));
        let chosen = asked.filter(|(entity, scope)| rows.iter().any(|known| known == entity) && columns.iter().any(|column| column.scope == *scope));
        let title = columns.iter().map(|scope| scope_name(&scope.display_name, &scope.scope)).collect::<Vec<_>>().join(" \u{b7} ");
        let (editor_html, slot_id) = match chosen.and_then(|(entity, scope)| columns.iter().find(|column| column.scope == scope).map(|column| (entity, *column))) {
            Some((entity, column)) => {
                let entry = entries.iter().find(|entry| entry.entity == entity && entry.scope == column.scope);
                (editor(&app, &frame, id, entity, column, entry), format!(r#" id="{EDITOR_ANCHOR}""#))
            }
            None => (String::new(), String::new()),
        };
        sections.push_str(&format!(
            r#"<section class="rep-panel rep-cluster"><h2 class="rep-ctitle">{}</h2><div class="rep-scroll">{}</div><div class="rep-editor"{slot_id}>{editor_html}</div></section>"#,
            escape(&title),
            matrix(&app, id, &rows, &columns, &entries, chosen)
        ));
    }
    if sections.is_empty() {
        sections.push_str(r#"<p class="muted">No scope to show.</p>"#);
    }
    let legend = r#"<div class="rep-legend muted"><span>The rank and its progress in the cell</span><span>+ no reputation yet</span><span>dotted: the game gives that faction no such scope</span><span>Press an open cell again to close it</span></div>"#;
    body.push_str(&format!(r#"<div id="{SECTION}">{sections}{legend}</div>"#));
    html(frame.page("Reputation", &body))
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn ladder() -> Vec<LadderStanding> {
        [(-100, "Low"), (0, "Mid"), (100, "Top")].iter().map(|(min, name)| LadderStanding { name: (*name).into(), min: *min }).collect()
    }

    #[test]
    fn a_score_is_a_percentage_of_the_way_to_the_next_level_and_the_top_is_complete() {
        let ladder = ladder();
        assert_eq!(progress(&ladder, 0), Some((1, 0)));
        assert_eq!(progress(&ladder, 25), Some((1, 25)));
        assert_eq!(progress(&ladder, -50), Some((0, 51)));
        assert_eq!(progress(&ladder, 99), Some((1, 100)), "the last point of a level is its whole progress");
        assert_eq!(progress(&ladder, 100), Some((2, 100)));
        assert_eq!(progress(&ladder, 5_000), Some((2, 100)));
        assert_eq!(progress(&ladder, -900), Some((0, 0)), "below the first level");
        assert_eq!(progress(&[], 5), None);
    }
}
