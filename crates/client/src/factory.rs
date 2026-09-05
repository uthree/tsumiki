//! Factory controls and belt animation. Visible belt cargo is derived from
//! server flow rates and never participates in inventory or collision.

use std::collections::HashMap;

use bevy::math::Affine2;
use bevy::prelude::*;
use tsumiki_protocol::{BeltFlow, ClientToServer, ContainerKind, FactoryAction, FactoryView};

use crate::entity_light::EntityLightTint;
use crate::item_icons::{self, ItemIcons};
use crate::net::Transport;
use crate::pause::PauseState;
use crate::state::{ContainerState, GameState, ItemReg};
use crate::view::Registry;
use crate::{AppState, UiFont, ui};

#[derive(Resource, Default)]
pub(crate) struct FactoryClient {
    pub view: Option<FactoryView>,
    pub flows: Vec<BeltFlow>,
}

#[derive(Component)]
struct StatusText;

#[derive(Component, Clone, Copy)]
struct ActionButton(FactoryAction);

#[derive(Component)]
struct Cargo(BeltFlow);

#[derive(Resource, Default)]
struct CargoEntities(HashMap<IVec3, (Entity, Handle<StandardMaterial>)>);

#[derive(Resource)]
struct CargoMesh(Handle<Mesh>);

pub fn install(app: &mut App) {
    app.init_resource::<FactoryClient>()
        .init_resource::<CargoEntities>()
        .add_systems(Startup, setup)
        .add_systems(OnExit(AppState::InGame), teardown)
        .add_systems(
            Update,
            (update_status, handle_actions, sync_cargo, animate_cargo)
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn setup(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    commands.insert_resource(CargoMesh(meshes.add(Rectangle::new(0.35, 0.35))));
}

pub(crate) fn spawn_panel(parent: &mut ChildSpawnerCommands<'_>, font: &UiFont) {
    parent.spawn((
        Text::new("Reading machine..."),
        font.text(16.0),
        TextColor(ui::PANEL_TEXT_COLOR),
        StatusText,
    ));
    parent.spawn((
        Text::new("Deposit the held stack; Withdraw collects output.\nRecipe changes reset unfinished work."),
        font.text(16.0),
        TextColor(Color::srgb(0.75, 0.73, 0.68)),
    ));
    for buttons in [
        vec![
            (FactoryAction::Deposit, "Deposit"),
            (FactoryAction::Withdraw, "Withdraw"),
        ],
        vec![
            (FactoryAction::Rotate, "Rotate"),
            (FactoryAction::CycleItem, "Recipe / item"),
            (FactoryAction::Toggle, "Run / stop"),
        ],
    ] {
        parent
            .spawn(Node {
                width: Val::Percent(100.0),
                column_gap: Val::Px(6.0),
                ..default()
            })
            .with_children(|row| {
                for (action, label) in buttons {
                    row.spawn((
                        Button,
                        ActionButton(action),
                        Node {
                            flex_grow: 1.0,
                            height: Val::Px(34.0),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.28, 0.40, 0.43)),
                        ui::ButtonBase(Color::srgb(0.28, 0.40, 0.43)),
                    ))
                    .with_children(|button| {
                        button.spawn((
                            Text::new(label),
                            font.text(16.0),
                            TextColor(ui::PANEL_TEXT_COLOR),
                        ));
                    });
                }
            });
    }
}

fn status_text(
    view: &FactoryView,
    registry: &tsumiki_world::ItemRegistry,
    blocks: &tsumiki_world::BlockRegistry,
) -> String {
    let buffer = |label, value: Option<tsumiki_protocol::FactoryBufferView>| match value {
        Some(b) => format!(
            "{label}: {}  {:.1}/{:.0}  ({:+.2}/s)",
            registry.get(b.item).name.replace('_', " "),
            b.amount,
            b.capacity,
            b.rate
        ),
        None => format!("{label}: --"),
    };
    let kind = blocks.get(view.block).name.replace('_', " ");
    let power = if view.block == tsumiki_world::blocks::MINER {
        format!(
            "Power: {:.0}% | Ore remaining: {:.1}",
            view.power_ratio * 100.0,
            view.reserve
        )
    } else {
        format!("Power: {:.0}%", view.power_ratio * 100.0)
    };
    format!(
        "{} | {} | Output: {}\n{}\n{}\n{}",
        kind,
        if view.enabled { "Running" } else { "Stopped" },
        ["East (+X)", "South (+Z)", "West (-X)", "North (-Z)"][view.direction as usize % 4],
        power,
        buffer("Input", view.input),
        buffer("Output", view.output)
    )
}

fn update_status(
    factory: Res<FactoryClient>,
    container: Res<ContainerState>,
    registry: Res<ItemReg>,
    blocks: Res<Registry>,
    mut texts: Query<&mut Text, With<StatusText>>,
) {
    let Some(view) = &factory.view else {
        return;
    };
    if !container
        .open
        .as_ref()
        .is_some_and(|open| open.kind == ContainerKind::Factory && open.pos == view.pos)
    {
        return;
    }
    let label = status_text(view, &registry.0, &blocks.0);
    for mut text in &mut texts {
        if text.0 != label {
            text.0.clone_from(&label);
        }
    }
}

fn handle_actions(
    pause: Res<State<PauseState>>,
    state: Res<GameState>,
    container: Res<ContainerState>,
    buttons: Query<(&Interaction, &ActionButton), Changed<Interaction>>,
    mut transport: ResMut<Transport>,
) {
    if *pause.get() != PauseState::Inventory || state.dead {
        return;
    }
    let Some(open) = container
        .open
        .as_ref()
        .filter(|open| open.kind == ContainerKind::Factory)
    else {
        return;
    };
    for (interaction, button) in &buttons {
        if *interaction == Interaction::Pressed {
            transport.send(ClientToServer::FactoryAction {
                pos: open.pos,
                action: button.0,
            });
        }
    }
}

fn sync_cargo(
    mut commands: Commands,
    factory: Res<FactoryClient>,
    icons: Res<ItemIcons>,
    mesh: Res<CargoMesh>,
    mut entities: ResMut<CargoEntities>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !factory.is_changed() {
        return;
    }
    let wanted: HashMap<_, _> = factory
        .flows
        .iter()
        .filter(|flow| flow.rate > 0.0001)
        .map(|flow| (flow.pos, *flow))
        .collect();
    entities.0.retain(|pos, (entity, material)| {
        if wanted.contains_key(pos) {
            true
        } else {
            commands.entity(*entity).despawn();
            materials.remove(material.id());
            false
        }
    });
    for (pos, flow) in wanted {
        let rect = item_icons::rect(flow.item);
        let uv = Affine2::from_scale_angle_translation(
            rect.size() / item_icons::ATLAS_SIZE,
            0.0,
            rect.min / item_icons::ATLAS_SIZE,
        );
        if let Some((entity, material)) = entities.0.get(&pos) {
            commands.entity(*entity).insert(Cargo(flow));
            if let Some(mut material) = materials.get_mut(material) {
                material.uv_transform = uv;
            }
        } else {
            let material = materials.add(StandardMaterial {
                base_color_texture: Some(icons.image.clone()),
                uv_transform: uv,
                alpha_mode: AlphaMode::Mask(0.5),
                cull_mode: None,
                double_sided: true,
                unlit: true,
                ..default()
            });
            let entity = commands
                .spawn((
                    Mesh3d(mesh.0.clone()),
                    MeshMaterial3d(material.clone()),
                    Transform::from_translation(pos.as_vec3() + Vec3::new(0.5, 1.2, 0.5)),
                    EntityLightTint(Color::WHITE),
                    Cargo(flow),
                ))
                .id();
            entities.0.insert(pos, (entity, material));
        }
    }
}

fn animate_cargo(
    time: Res<Time>,
    cameras: Query<&Transform, (With<crate::camera::Player>, Without<Cargo>)>,
    mut cargo: Query<(&Cargo, &mut Transform), Without<crate::camera::Player>>,
) {
    let camera = cameras.single().ok();
    for (Cargo(flow), mut transform) in &mut cargo {
        let direction = [Vec3::X, Vec3::Z, Vec3::NEG_X, Vec3::NEG_Z][flow.direction as usize % 4];
        let phase = (time.elapsed_secs_f64() * flow.rate.max(0.1)).fract() as f32 - 0.5;
        transform.translation = flow.pos.as_vec3() + Vec3::new(0.5, 1.2, 0.5) + direction * phase;
        // A vertical billboard keeps the item readable at every view angle
        // while its position still follows the server's transport rate.
        if let Some(camera) = camera {
            let facing = camera.translation - transform.translation;
            transform.rotation = Quat::from_rotation_y(facing.x.atan2(facing.z));
        }
    }
}

fn teardown(
    mut commands: Commands,
    mut entities: ResMut<CargoEntities>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut state: ResMut<FactoryClient>,
) {
    for (_, (entity, material)) in entities.0.drain() {
        commands.entity(entity).despawn();
        materials.remove(material.id());
    }
    *state = FactoryClient::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_protocol::{FactoryBufferView, ServerTransport, local};
    use tsumiki_world::{BlockRegistry, ItemRegistry, blocks, items};

    fn open_factory(pos: IVec3) -> crate::state::OpenContainer {
        crate::state::OpenContainer {
            kind: ContainerKind::Factory,
            pos,
            slots: Vec::new(),
            cook: 0.0,
            fuel: 0.0,
        }
    }

    #[test]
    fn actions_require_a_live_open_factory_and_send_only_once_per_press() {
        for gate in 0..5 {
            let (mut server, client) = local::pair();
            let mut app = App::new();
            let pos = IVec3::new(29, 8, 15);
            let mut open = open_factory(pos);
            if gate == 3 {
                open.kind = ContainerKind::Chest;
            }
            app.insert_resource(Transport::new(Box::new(client)))
                .insert_resource(State::new(if gate == 1 {
                    PauseState::Playing
                } else {
                    PauseState::Inventory
                }))
                .insert_resource(GameState {
                    dead: gate == 2,
                    ..default()
                })
                .insert_resource(ContainerState {
                    open: if gate == 4 { None } else { Some(open) },
                })
                .add_systems(Update, handle_actions);
            app.world_mut()
                .spawn((Interaction::Pressed, ActionButton(FactoryAction::Rotate)));
            app.update();
            if gate == 0 {
                assert!(
                    matches!(server.try_recv(), Some((_, ClientToServer::FactoryAction { pos: target, action: FactoryAction::Rotate })) if target == pos)
                );
            }
            assert!(server.try_recv().is_none(), "gate {gate}");
            app.update();
            assert!(
                server.try_recv().is_none(),
                "holding a button must not repeat its action"
            );
        }
    }

    #[test]
    fn status_displays_authoritative_buffer_amounts_rates_power_and_direction() {
        let view = FactoryView {
            pos: IVec3::ZERO,
            block: blocks::POWERED_FURNACE,
            direction: 2,
            enabled: true,
            input: Some(FactoryBufferView {
                item: items::IRON_ORE,
                amount: 7.25,
                capacity: 64.0,
                rate: -0.5,
            }),
            output: Some(FactoryBufferView {
                item: items::IRON_INGOT,
                amount: 2.0,
                capacity: 64.0,
                rate: 0.5,
            }),
            reserve: 0.0,
            power_ratio: 0.5,
        };
        let label = status_text(
            &view,
            &ItemRegistry::prototype(),
            &BlockRegistry::prototype(),
        );
        assert!(label.contains("powered furnace | Running | Output: West (-X)"));
        assert!(label.contains("Power: 50%"));
        assert!(label.contains("Input: iron ore  7.2/64  (-0.50/s)"));
        assert!(label.contains("Output: iron ingot  2.0/64  (+0.50/s)"));
        assert!(!label.contains("Ore remaining"));
    }

    #[test]
    fn belt_flow_updates_reuse_visuals_and_stopped_flows_remove_them() {
        let mut app = App::new();
        let flow = BeltFlow {
            pos: IVec3::new(2, 3, 4),
            direction: 0,
            item: items::IRON_ORE,
            rate: 0.5,
        };
        app.insert_resource(FactoryClient {
            view: None,
            flows: vec![flow],
        })
        .insert_resource(ItemIcons {
            image: Handle::default(),
        })
        .insert_resource(CargoMesh(Handle::default()))
        .init_resource::<CargoEntities>()
        .init_resource::<Assets<StandardMaterial>>()
        .add_systems(Update, sync_cargo);
        app.update();
        let original = app.world().resource::<CargoEntities>().0[&flow.pos].0;
        app.world_mut().resource_mut::<FactoryClient>().flows[0].item = items::IRON_INGOT;
        app.update();
        assert_eq!(
            app.world().resource::<CargoEntities>().0[&flow.pos].0,
            original
        );
        assert_eq!(
            app.world().get::<Cargo>(original).unwrap().0.item,
            items::IRON_INGOT
        );
        assert_eq!(app.world().resource::<Assets<StandardMaterial>>().len(), 1);
        app.world_mut().resource_mut::<FactoryClient>().flows[0].rate = 0.0;
        app.update();
        assert!(app.world().get_entity(original).is_err());
        assert!(app.world().resource::<CargoEntities>().0.is_empty());
        assert_eq!(app.world().resource::<Assets<StandardMaterial>>().len(), 0);
    }

    #[test]
    fn belt_cargo_follows_flow_and_stays_facing_the_camera() {
        let mut app = App::new();
        let mut time = Time::<()>::default();
        time.advance_by(std::time::Duration::from_secs_f32(1.0));
        app.insert_resource(time).add_systems(Update, animate_cargo);
        let camera_pos = Vec3::new(6.0, 5.0, 8.0);
        app.world_mut().spawn((
            crate::camera::Player {
                feet: camera_pos,
                velocity: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
                mode: crate::camera::PlayerMode::Fly,
                spawned: true,
                on_ground: false,
                feet_in_water: false,
                eye_in_water: false,
                landed_this_frame: None,
            },
            Transform::from_translation(camera_pos),
        ));
        let cargo = app
            .world_mut()
            .spawn((
                Cargo(BeltFlow {
                    pos: IVec3::ZERO,
                    direction: 0,
                    item: items::IRON_ORE,
                    rate: 0.25,
                }),
                Transform::default(),
            ))
            .id();
        app.update();
        let transform = app.world().get::<Transform>(cargo).unwrap();
        assert_eq!(transform.translation, Vec3::new(0.25, 1.2, 0.5));
        let facing = transform.rotation * Vec3::Z;
        let toward_camera = (camera_pos - transform.translation) * Vec3::new(1.0, 0.0, 1.0);
        assert!(facing.dot(toward_camera.normalize()) > 0.9999);
    }
}
