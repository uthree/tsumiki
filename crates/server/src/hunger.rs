//! Food energy, activity costs, gated regeneration, and starvation (M8).

use tsumiki_protocol::{MAX_HP, MAX_HUNGER};
use tsumiki_world::{HOTBAR_SIZE, food};

use crate::ClientState;

pub const EXHAUSTION_PER_HUNGER: f32 = 4.0;
pub const PASSIVE_EXHAUSTION_PER_SECOND: f32 = EXHAUSTION_PER_HUNGER / 120.0;
pub const MINING_EXHAUSTION: f32 = 0.04;
pub const MOVEMENT_EXHAUSTION_PER_BLOCK: f32 = 0.02;
pub const REGEN_INTERVAL_SECS: f32 = 2.0;
pub const STARVATION_INTERVAL_SECS: f32 = 4.0;

/// Spending energy never underflows hunger; excess cost at zero is discarded.
pub fn exhaust(client: &mut ClientState, amount: f32) {
    client.exhaustion += amount;
    while client.exhaustion >= EXHAUSTION_PER_HUNGER {
        client.exhaustion -= EXHAUSTION_PER_HUNGER;
        client.hunger = client.hunger.saturating_sub(1);
    }
}

pub fn eat(client: &mut ClientState, hotbar: u8) -> bool {
    if client.hp == 0
        || client.save.is_none()
        || client.hunger >= MAX_HUNGER
        || hotbar as usize >= HOTBAR_SIZE
    {
        return false;
    }
    let Some(stack) = client.main.slot(hotbar as usize) else {
        return false;
    };
    let Some(nutrition) = food::nutrition(stack.item) else {
        return false;
    };
    client.main.take_from(hotbar as usize, 1);
    client.hunger = (client.hunger + nutrition).min(MAX_HUNGER);
    client.starvation_accum = 0.0;
    true
}

/// Returns whether this tick killed the player. The caller owns the common
/// death transition, so starvation drops exactly the same items as other damage.
pub fn tick(client: &mut ClientState, dt: f32) -> bool {
    if client.hp == 0 || client.save.is_none() {
        client.hp_regen_accum = 0.0;
        client.starvation_accum = 0.0;
        return false;
    }
    exhaust(client, dt * PASSIVE_EXHAUSTION_PER_SECOND);
    if client.hunger >= 18 && client.hp < MAX_HP {
        client.hp_regen_accum += dt;
        while client.hp_regen_accum >= REGEN_INTERVAL_SECS
            && client.hp < MAX_HP
            && client.hunger >= 18
        {
            client.hp_regen_accum -= REGEN_INTERVAL_SECS;
            client.hp += 1;
            exhaust(client, 2.0);
        }
    } else {
        client.hp_regen_accum = 0.0;
    }
    if client.hunger == 0 {
        client.starvation_accum += dt;
        while client.starvation_accum >= STARVATION_INTERVAL_SECS && client.hp > 0 {
            client.starvation_accum -= STARVATION_INTERVAL_SECS;
            client.hp -= 1;
        }
    } else {
        client.starvation_accum = 0.0;
    }
    client.hp == 0
}
