//! Underwater screen tint (roadmap.md M4): a translucent blue full-screen
//! overlay shown whenever the player's eye is inside a water block.
//! Independent of game mode — anyone swimming sees it, survival or
//! creative.

use bevy::prelude::*;

use crate::AppState;
use crate::camera::Player;

const TINT_COLOR: Color = Color::srgba(0.15, 0.35, 0.65, 0.28);

#[derive(Component)]
struct UnderwaterOverlay;

pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_overlay)
        .add_systems(OnExit(AppState::InGame), teardown_overlay)
        .add_systems(Update, update_overlay.run_if(in_state(AppState::InGame)));
}

fn spawn_overlay(mut commands: Commands) {
    commands.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            position_type: PositionType::Absolute,
            ..default()
        },
        BackgroundColor(TINT_COLOR),
        Visibility::Hidden,
        UnderwaterOverlay,
    ));
}

fn teardown_overlay(mut commands: Commands, overlays: Query<Entity, With<UnderwaterOverlay>>) {
    for entity in &overlays {
        commands.entity(entity).despawn();
    }
}

fn update_overlay(
    players: Query<&Player>,
    mut overlays: Query<&mut Visibility, With<UnderwaterOverlay>>,
) {
    let submerged = players.single().map(|p| p.eye_in_water).unwrap_or(false);
    for mut vis in &mut overlays {
        *vis = if submerged {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}
