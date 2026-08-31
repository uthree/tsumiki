//! Client-detected survival damage (roadmap.md M4): fall damage and
//! drowning. Movement is client-authoritative (design.md decision), so the
//! client detects both locally and reports them via
//! `ClientToServer::ReportDamage`; the server clamps/validates the amount
//! and answers with `HealthUpdate`/`Died` (applied in `net.rs`).

use bevy::prelude::*;
use tsumiki_protocol::{ClientToServer, DamageCause, MAX_HP};
use tsumiki_world::physics::GRAVITY;

use crate::AppState;
use crate::camera::Player;
use crate::net::Transport;
use crate::pause;
use crate::state::{self, GameMode};

/// Seconds of underwater air before drowning damage starts.
pub const AIR_MAX: f32 = 10.0;
/// Drowning damage tick interval once air runs out.
const DROWN_TICK_SECS: f32 = 1.0;
/// Drowning damage per tick.
const DROWN_DAMAGE: u16 = 1;
/// A fall shorter than this deals no damage.
const SAFE_FALL_HEIGHT: f32 = 3.0;

/// Fall damage for a fall that lands at `impact_speed` blocks/s (the
/// player's downward speed magnitude on the exact frame it lands).
///
/// Pure: since gravity is constant and nothing else pushes the player
/// vertically mid-fall, `h = v^2 / (2*|g|)` recovers the total fall height
/// from the impact speed alone (`v^2 = 2gh` for a fall from rest). Damage is
/// `floor(h - 3.0)` clamped to `0..=MAX_HP`.
pub fn fall_damage(impact_speed: f32) -> u16 {
    let height = (impact_speed * impact_speed) / (2.0 * GRAVITY.abs());
    let raw = (height - SAFE_FALL_HEIGHT).floor();
    if raw <= 0.0 {
        0
    } else {
        (raw as u16).min(MAX_HP)
    }
}

/// Underwater air reserve, counted down while the player's eye is in water.
/// Read by [`crate::health`] to draw the air-bubble HUD row.
#[derive(Resource)]
pub struct Submersion {
    pub air_remaining: f32,
    /// Seconds accumulated toward the next drowning damage tick, only while
    /// `air_remaining` is at zero.
    drown_tick: f32,
}

impl Default for Submersion {
    fn default() -> Self {
        Self {
            air_remaining: AIR_MAX,
            drown_tick: 0.0,
        }
    }
}

fn reset_submersion(mut submersion: ResMut<Submersion>) {
    *submersion = Submersion::default();
}

/// Sends `ReportDamage { Fall }` the frame the player lands with enough
/// impact speed to hurt (survival only; see [`crate::camera::Player::landed_this_frame`]).
fn report_fall_damage(
    mode: Res<GameMode>,
    players: Query<&Player>,
    mut transport: ResMut<Transport>,
) {
    if !mode.is_survival() {
        return;
    }
    let Ok(player) = players.single() else {
        return;
    };
    let Some(impact_speed) = player.landed_this_frame else {
        return;
    };
    let dmg = fall_damage(impact_speed);
    if dmg > 0 {
        transport.send(ClientToServer::ReportDamage {
            amount: dmg,
            cause: DamageCause::Fall,
        });
    }
}

/// Drains [`Submersion::air_remaining`] while the player's eye is in water,
/// sending a `ReportDamage { Drown }` tick every second once it hits zero;
/// refills instantly on surfacing (survival only).
fn update_drowning(
    time: Res<Time>,
    mode: Res<GameMode>,
    mut submersion: ResMut<Submersion>,
    players: Query<&Player>,
    mut transport: ResMut<Transport>,
) {
    if !mode.is_survival() {
        return;
    }
    let Ok(player) = players.single() else {
        return;
    };
    if !player.spawned {
        return;
    }

    if player.eye_in_water {
        submersion.air_remaining = (submersion.air_remaining - time.delta_secs()).max(0.0);
        if submersion.air_remaining <= 0.0 {
            submersion.drown_tick += time.delta_secs();
            if submersion.drown_tick >= DROWN_TICK_SECS {
                submersion.drown_tick -= DROWN_TICK_SECS;
                transport.send(ClientToServer::ReportDamage {
                    amount: DROWN_DAMAGE,
                    cause: DamageCause::Drown,
                });
            }
        } else {
            submersion.drown_tick = 0.0;
        }
    } else {
        submersion.air_remaining = AIR_MAX;
        submersion.drown_tick = 0.0;
    }
}

/// Wires the fall-damage/drowning resources and detection systems into
/// `app`. Gated the same way as movement ([`pause::is_playing`],
/// [`state::is_alive`]) since both rely on this frame's movement outcome.
pub fn install(app: &mut App) {
    app.init_resource::<Submersion>()
        .add_systems(OnExit(AppState::InGame), reset_submersion)
        .add_systems(
            Update,
            (report_fall_damage, update_drowning)
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing)
                .run_if(state::is_alive),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impact_speed_for_height(h: f32) -> f32 {
        (2.0 * GRAVITY.abs() * h).sqrt()
    }

    #[test]
    fn fall_shorter_than_safe_height_deals_no_damage() {
        assert_eq!(fall_damage(impact_speed_for_height(2.0)), 0);
    }

    #[test]
    fn fall_exactly_at_safe_height_deals_no_damage() {
        assert_eq!(fall_damage(impact_speed_for_height(3.0)), 0);
    }

    #[test]
    fn fall_just_past_safe_height_deals_no_damage_until_a_full_block() {
        // h = 3.9 -> floor(0.9) = 0.
        assert_eq!(fall_damage(impact_speed_for_height(3.9)), 0);
    }

    #[test]
    fn fall_one_block_past_safe_height_deals_one_damage() {
        assert_eq!(fall_damage(impact_speed_for_height(4.0)), 1);
    }

    #[test]
    fn fall_damage_scales_with_extra_height() {
        assert_eq!(fall_damage(impact_speed_for_height(10.0)), 7);
    }

    #[test]
    fn fall_damage_is_clamped_to_max_hp() {
        assert_eq!(fall_damage(impact_speed_for_height(100.0)), MAX_HP);
    }

    #[test]
    fn zero_impact_speed_deals_no_damage() {
        assert_eq!(fall_damage(0.0), 0);
    }
}
