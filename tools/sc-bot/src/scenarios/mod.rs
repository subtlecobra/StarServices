//! The scenarios. Each is an `async fn(&mut Ctx) -> Done` that joins, does something a client does, and checks the
//! answer; `ALL` lists them for the runner. To add one, write the function in a module here, and add a line to `ALL`.

use crate::scenario::Scenario;

mod disconnect;
mod elevator;
mod inventory;
mod join;
mod token;

macro_rules! scenario {
    ($name:expr, $about:expr, $run:path) => {
        Scenario { name: $name, about: $about, run: |ctx| Box::pin($run(ctx)) }
    };
}

pub const ALL: &[Scenario] = &[
    scenario!("join", "joins and checks the universe, the player, its gear, the hangar, its ATC, kiosks and call panels", join::run),
    scenario!("access-token", "asks for the access token of a worn item (message 21) and checks the answer (message 23)", token::run),
    scenario!("inventory-equip", "equips a stowed item to a port, checks the bind, takes it off, checks the unbind", inventory::equip),
    scenario!("inventory-move", "moves a stowed item between the item bank and the personal inventory and back", inventory::moves),
    scenario!("inventory-stack", "splits a stack and stacks the parts again", inventory::stack),
    scenario!("inventory-pocket", "stows a stowed item in the pocket (the personal storage) and moves it back", inventory::pocket),
    scenario!("inventory-equip-stack", "equips one item of a stack, checks the bind, takes it off", inventory::equip_from_stack),
    scenario!("elevator-call", "presses the call panel of the hangar's elevator and checks the answer and the carriage", elevator::run),
    scenario!("disconnect", "leaves with a disconnect notice and checks the shard's answer and its cleanup", disconnect::run),
];

pub fn find(name: &str) -> Option<&'static Scenario> {
    ALL.iter().find(|scenario| scenario.name == name)
}
