//! The scenarios. Each is an `async fn(&mut Ctx) -> Done` that joins, does something a client does, and checks the
//! answer; `ALL` lists them for the runner. To add one, write the function in a module here, and add a line to `ALL`.

use crate::scenario::Scenario;

mod disconnect;
mod elevator;
mod inventory;
mod join;
mod mobiglas;
mod token;
mod weapon_pose;

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
    scenario!("mobiglas", "asks to open and close the mobiGlas in an action frame and checks the replicated state variables", mobiglas::run),
    scenario!("weapon-pose", "draws and holsters a weapon in action frames and checks the replicated pose variable", weapon_pose::run),
    scenario!("disconnect", "leaves with a disconnect notice and checks the shard's answer and its cleanup", disconnect::run),
];

/// The indexes of the player's variables the replicant sets (`services/pu-frontend/src/input.rs`, `index`).
pub struct PlayerVariables {
    pub pose_state: usize,
    pub additive_state: usize,
    pub mobiglas_state: usize,
    pub frame_end_offset: usize,
    pub frame_end_base: usize,
}

pub const PLAYER_VARIABLES: PlayerVariables = PlayerVariables { pose_state: 164, additive_state: 411, mobiglas_state: 471, frame_end_offset: 908, frame_end_base: 996 };

/// The client time of the first frame (a recorded one) and the step between frames.
const FRAME_TIME_BASE: u64 = 0x003f_bcc2_ea1c_9b57;
const FRAME_TIME_STEP: u64 = 0x1_0000;

/// Sends an action frame of the player whose time is that of frame number `tick`.
async fn send_frame(session: &mut crate::session::Session, player: u64, tick: u64, actions: &[crate::requests::Action]) -> Result<(), String> {
    let start = FRAME_TIME_BASE + tick * FRAME_TIME_STEP;
    let args = crate::requests::action_frame(player, start, actions, start + FRAME_TIME_STEP / 2);
    session.call(player, crate::requests::ACTION_FRAME_METHOD, &args).await
}

pub fn find(name: &str) -> Option<&'static Scenario> {
    ALL.iter().find(|scenario| scenario.name == name)
}
