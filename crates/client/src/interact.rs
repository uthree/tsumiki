//! Block targeting, mining, placing and container interaction (roadmap M5
//! rework: `PlaceBlock` now names a hotbar slot rather than a block, so the
//! server decides what item is actually placed; right-clicking a block with
//! an interaction opens it instead).
//!
//! - Per frame, raycasts from the player's eye (via `tsumiki_world::raycast`,
//!   targeting solid blocks, reach `tsumiki_protocol::REACH`) and draws a
//!   highlight around the targeted block (a slightly inflated gizmo cuboid).
//!   Targeting is cleared while dead, but keeps running (rather than being
//!   gated off) so it can *become* `None` the instant death happens, instead
//!   of freezing on a stale target.
//! - Mining, while the cursor is grabbed:
//!   - Survival: holding LEFT on a targeted block accrues progress against
//!     `tsumiki_world::tool::break_time_secs(block, tool)`, `tool` being
//!     whatever the selected hotbar slot holds (roadmap M6) -- the same
//!     function and the same tool the server will check, so the bar never
//!     promises a time the server won't honor. Switching targets or
//!     releasing the button resets it. Progress is shown as a small bar
//!     below screen center, tinted with the target block's color -- unless
//!     the held tool cannot actually harvest the block
//!     (`tool::can_harvest`), in which case the bar switches to
//!     [`WRONG_TOOL_BAR_COLOR`] and a line naming the required tool appears
//!     underneath ([`missing_tool_text`]): the block will still break (and
//!     yield nothing), so this is the only warning a player gets before
//!     losing it. On completion, sends `BreakBlock { pos, hotbar }` -- no
//!     local edit; the server's `BlockChanged` is the only thing that
//!     actually removes the block, one server tick later (the point of
//!     server-authoritative mining). `hotbar` names the same selected slot
//!     [`held_tool`] read for the break-time/harvest check above, mirroring
//!     `PlaceBlock`'s own `hotbar` field: the server must not have to guess
//!     which of several carried tools was in hand, and must check the exact
//!     one the client's progress bar was already keyed to.
//!   - Creative: left click sends `BreakBlock` immediately (same `hotbar`
//!     field, though creative ignores tools) and *also* applies the local
//!     prediction edit, exactly like the old `SetBlock` path.
//! - Right click on the targeted block:
//!   - If it has a `BlockInteraction` (chest, crafting table): sends
//!     `OpenContainer { pos }` instead of placing. The screen itself only
//!     opens once the server answers `ContainerOpened` (handled in `net.rs`,
//!     which flips `PauseState` to `Inventory`) -- not predicted here, since
//!     the server can refuse (out of reach, wrong block by the time it
//!     processes the message).
//!   - Otherwise, places whatever item sits in the selected hotbar slot
//!     (`hotbar: u8`, not a block id: a client cannot ask to place something
//!     it does not have selected). Rejected locally when the face normal is
//!     zero, the destination is outside vertical world bounds or not
//!     air/water, it would intersect the player's AABB, or the held item
//!     does not place a block at all (`ItemRegistry::places`). Creative
//!     additionally predicts the edit locally, resolving the block from the
//!     held item; survival never applies a local edit (waits for
//!     `BlockChanged`).
//! - Dead players get no targeting/highlight/clicks (mining/placing/opening
//!   is gated off; see above for why targeting itself stays live).
//! - Click handling runs *before* [`crate::camera::grab_cursor`] each frame,
//!   so the very click that grabs the cursor is seen as "not yet grabbed"
//!   and never also breaks/places/opens.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use tsumiki_protocol::ClientToServer;
use tsumiki_world::physics::{Aabb, PLAYER_EYE_HEIGHT};
use tsumiki_world::raycast::{RayHit, raycast_voxels};
use tsumiki_world::tool::{self, ToolDef};
use tsumiki_world::{
    BlockDef, BlockId, ItemId, ItemRegistry, ItemStack, ToolKind, ToolTier, WORLD_HEIGHT_BLOCKS,
    blocks,
};

use crate::AppState;
use crate::camera::{self, Player};
use crate::hotbar::Hotbar;
use crate::net;
use crate::pause;
use crate::state::{self, GameState, ItemReg};
use crate::ui;
use crate::view::{self, ChunkStore};

/// Half the highlight cuboid's inflation over the unit block, per axis.
const HIGHLIGHT_INFLATION: f32 = 1.02;

const BAR_WIDTH: f32 = 120.0;
const BAR_HEIGHT: f32 = 14.0;
/// How far down from the very top of the screen the bar's track sits;
/// "just below the screen center".
const BAR_TOP_PADDING_PERCENT: f32 = 54.0;
const BAR_BORDER_COLOR: Color = Color::srgba(0.0, 0.0, 0.0, 0.6);
const BAR_TRACK_COLOR: Color = Color::srgba(0.08, 0.08, 0.1, 0.55);
/// Fill color while the held tool cannot actually harvest the target
/// (roadmap M6) -- a muted clay tone rather than an alarm red, so it reads as
/// "this is off" without shouting (design.md §8's no-pure-black/white,
/// quiet-palette convention).
const WRONG_TOOL_BAR_COLOR: Color = Color::srgb(0.62, 0.42, 0.30);
const PROGRESS_LABEL_FONT_SIZE: f32 = 16.0;
/// Gap between the bar and the "Needs a ..." label underneath it.
const PROGRESS_LABEL_MARGIN_TOP: f32 = 6.0;

/// The block currently under the crosshair, if any. Recomputed every frame.
#[derive(Resource, Default)]
struct TargetedBlock(Option<RayHit>);

/// Survival hold-to-mine progress: which block is being mined and how long
/// it's been held. Reset whenever the target changes or the left button is
/// released.
#[derive(Resource, Default)]
struct MiningProgress {
    target: Option<IVec3>,
    elapsed: f32,
}

#[derive(Component)]
struct ProgressBarRoot;
#[derive(Component)]
struct ProgressBarFill;
/// The "Needs a ..." line under the bar (roadmap M6); empty text when the
/// held tool already meets the block's harvest gate (or it has none).
#[derive(Component)]
struct ProgressBarLabel;

/// Wires the targeting/highlight/mining/placing systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<TargetedBlock>()
        .init_resource::<MiningProgress>()
        .add_systems(OnEnter(AppState::InGame), spawn_progress_bar)
        .add_systems(OnExit(AppState::InGame), teardown_progress_bar)
        .add_systems(
            Update,
            compute_target
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing),
        )
        .add_systems(
            Update,
            (
                draw_highlight,
                update_progress_bar,
                handle_mining_and_placing.before(camera::grab_cursor),
            )
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing)
                .run_if(state::is_alive),
        );
}

fn compute_target(
    mut target: ResMut<TargetedBlock>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    state: Res<GameState>,
    players: Query<&Player>,
) {
    let Ok(player) = players.single() else {
        target.0 = None;
        return;
    };
    if state.dead {
        target.0 = None;
        return;
    }
    let eye = player.feet + Vec3::Y * PLAYER_EYE_HEIGHT;
    let dir = Quat::from_euler(EulerRot::YXZ, player.yaw, player.pitch, 0.0) * Vec3::NEG_Z;
    let is_target = |pos: IVec3| {
        view::block_at(&store, pos)
            .map(|block| registry.0.get(block).is_targetable())
            .unwrap_or(false)
    };
    target.0 = raycast_voxels(eye, dir, tsumiki_protocol::REACH, is_target);
}

fn draw_highlight(target: Res<TargetedBlock>, mut gizmos: Gizmos) {
    let Some(hit) = target.0 else {
        return;
    };
    let center = hit.block.as_vec3() + Vec3::splat(0.5);
    let size = Vec3::splat(HIGHLIGHT_INFLATION);
    gizmos.primitive_3d(&Cuboid::from_size(size), center, Color::BLACK);
}

#[allow(clippy::too_many_arguments)]
fn handle_mining_and_placing(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    target: Res<TargetedBlock>,
    mut store: ResMut<ChunkStore>,
    hotbar: Res<Hotbar>,
    players: Query<&Player>,
    mut transport: ResMut<net::Transport>,
    mode: Res<state::GameMode>,
    game_state: Res<GameState>,
    item_reg: Res<ItemReg>,
    registry: Res<view::Registry>,
    time: Res<Time>,
    mut mining: ResMut<MiningProgress>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    // Not grabbed yet: this click (if any) is the one `grab_cursor` is about
    // to consume to grab the cursor, not an edit.
    if cursor.grab_mode != CursorGrabMode::Locked
        || game_state.dead
        || keys.just_pressed(KeyCode::Escape)
        || keys.just_pressed(KeyCode::KeyE)
    {
        *mining = MiningProgress::default();
        return;
    }

    let current_target = target.0.map(|hit| hit.block);
    if mining.target != current_target || !mouse_buttons.pressed(MouseButton::Left) {
        mining.target = None;
        mining.elapsed = 0.0;
    }

    // Food is usable while looking at the sky, and takes priority over
    // block interactions. The server validates hunger and consumes it.
    if mouse_buttons.just_pressed(MouseButton::Right)
        && hotbar
            .selected_stack(&game_state.main)
            .is_some_and(|stack| tsumiki_world::food::nutrition(stack.item).is_some())
    {
        transport.send(ClientToServer::Eat {
            hotbar: hotbar.selected as u8,
        });
        return;
    }

    let Some(hit) = target.0 else {
        return;
    };

    if mouse_buttons.pressed(MouseButton::Left) {
        if mode.is_survival() {
            mining.target = Some(hit.block);
            mining.elapsed += time.delta_secs();
            let block = view::block_at(&store, hit.block).unwrap_or(BlockId::AIR);
            let def = registry.0.get(block);
            let held = held_tool(hotbar.selected_stack(&game_state.main), &item_reg.0);
            let required = tool::break_time_secs(def, held.as_ref());
            if required <= 0.0 || mining.elapsed >= required {
                transport.send(ClientToServer::BreakBlock {
                    pos: hit.block,
                    hotbar: hotbar.selected as u8,
                });
                mining.target = None;
                mining.elapsed = 0.0;
            }
        } else if mouse_buttons.just_pressed(MouseButton::Left) {
            view::set_block(&mut store, hit.block, blocks::AIR);
            transport.send(ClientToServer::BreakBlock {
                pos: hit.block,
                hotbar: hotbar.selected as u8,
            });
        }
    }

    if mouse_buttons.just_pressed(MouseButton::Right) {
        let looked_at = view::block_at(&store, hit.block).unwrap_or(BlockId::AIR);
        let held = held_tool(hotbar.selected_stack(&game_state.main), &item_reg.0);
        if can_till(looked_at, hit.face_normal, held.as_ref()) {
            transport.send(ClientToServer::TillSoil {
                pos: hit.block,
                hotbar: hotbar.selected as u8,
            });
        } else if registry.0.get(looked_at).interaction.is_some() {
            transport.send(ClientToServer::OpenContainer { pos: hit.block });
        } else {
            try_place(
                &mut store,
                &mut transport,
                hit,
                &hotbar,
                &players,
                mode.0,
                &game_state,
                &item_reg.0,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_place(
    store: &mut ChunkStore,
    transport: &mut net::Transport,
    hit: RayHit,
    hotbar: &Hotbar,
    players: &Query<&Player>,
    mode: tsumiki_protocol::GameMode,
    game_state: &GameState,
    item_reg: &ItemRegistry,
) {
    if hit.face_normal == IVec3::ZERO {
        return;
    }
    let dest = hit.block + hit.face_normal;
    if !(0..WORLD_HEIGHT_BLOCKS).contains(&dest.y) {
        return;
    }
    let Some(existing) = view::block_at(store, dest) else {
        return;
    };
    if !(existing.is_air() || existing == blocks::WATER) {
        return;
    }
    let Ok(player) = players.single() else {
        return;
    };
    if Aabb::player(player.feet).intersects_block(dest) {
        return;
    }

    let Some(stack) = hotbar.selected_stack(&game_state.main) else {
        return;
    };
    let Some(block) = item_reg.places(stack.item) else {
        return;
    };
    if block == blocks::WHEAT_YOUNG
        && view::block_at(store, dest - IVec3::Y) != Some(blocks::FARMLAND)
    {
        return;
    }

    match mode {
        tsumiki_protocol::GameMode::Creative => {
            view::set_block(store, dest, block);
            transport.send(ClientToServer::PlaceBlock {
                pos: dest,
                hotbar: hotbar.selected as u8,
            });
        }
        tsumiki_protocol::GameMode::Survival => {
            transport.send(ClientToServer::PlaceBlock {
                pos: dest,
                hotbar: hotbar.selected as u8,
            });
        }
    }
}

fn spawn_progress_bar(mut commands: Commands, font: Res<crate::UiFont>) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                padding: UiRect::top(Val::Percent(BAR_TOP_PADDING_PERCENT)),
                ..default()
            },
            Visibility::Hidden,
            ProgressBarRoot,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: Val::Px(BAR_WIDTH),
                    height: Val::Px(BAR_HEIGHT),
                    border: UiRect::all(Val::Px(2.0)),
                    padding: UiRect::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(BAR_TRACK_COLOR),
                BorderColor::all(BAR_BORDER_COLOR),
            ))
            .with_children(|track| {
                track.spawn((
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    ProgressBarFill,
                ));
            });
            root.spawn((
                Node {
                    margin: UiRect::top(Val::Px(PROGRESS_LABEL_MARGIN_TOP)),
                    ..default()
                },
                Text::new(""),
                font.text(PROGRESS_LABEL_FONT_SIZE),
                TextColor(ui::PANEL_TEXT_COLOR),
                ProgressBarLabel,
            ));
        });
}

fn teardown_progress_bar(mut commands: Commands, roots: Query<Entity, With<ProgressBarRoot>>) {
    for entity in &roots {
        commands.entity(entity).despawn();
    }
}

#[allow(clippy::too_many_arguments)]
fn update_progress_bar(
    mining: Res<MiningProgress>,
    mode: Res<state::GameMode>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    item_reg: Res<ItemReg>,
    hotbar: Res<Hotbar>,
    game_state: Res<GameState>,
    mut roots: Query<&mut Visibility, With<ProgressBarRoot>>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), With<ProgressBarFill>>,
    mut labels: Query<&mut Text, With<ProgressBarLabel>>,
) {
    let active = mode.is_survival() && mining.target.is_some();
    for mut vis in &mut roots {
        *vis = if active {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    let Some(target) = mining.target.filter(|_| active) else {
        return;
    };

    let block = view::block_at(&store, target).unwrap_or(BlockId::AIR);
    let def = registry.0.get(block);
    let held = held_tool(hotbar.selected_stack(&game_state.main), &item_reg.0);
    let required = tool::break_time_secs(def, held.as_ref()).max(0.0001);
    let fraction = (mining.elapsed / required).clamp(0.0, 1.0);
    let color = if tool::can_harvest(def, held.as_ref()) {
        Color::srgb_u8(def.color_top[0], def.color_top[1], def.color_top[2])
    } else {
        WRONG_TOOL_BAR_COLOR
    };

    for (mut node, mut bg) in &mut fills {
        node.width = Val::Percent(fraction * 100.0);
        *bg = BackgroundColor(color);
    }

    let text = missing_tool_text(&item_reg.0, def, held).unwrap_or_default();
    for mut label in &mut labels {
        label.0 = text.clone();
    }
}

/// The tool in the selected hotbar slot, if any -- the single place mining
/// code decides "what is the player holding". Pure and unit-tested so the
/// hold-to-mine timer and the progress bar can never disagree about it.
fn held_tool(stack: Option<ItemStack>, item_reg: &ItemRegistry) -> Option<ToolDef> {
    stack.and_then(|s| item_reg.tool(s.item).copied())
}

fn can_till(block: BlockId, normal: IVec3, tool: Option<&ToolDef>) -> bool {
    normal == IVec3::Y
        && matches!(block, blocks::GRASS | blocks::DIRT)
        && tool.is_some_and(|tool| tool.kind == ToolKind::Shovel)
}

/// Human-readable requirement text for `block`'s harvest gate, e.g.
/// `"Needs a stone pickaxe"` -- `None` once `tool` already satisfies the
/// gate (or the block has none). Derived from the block's `tool`/
/// `harvest_tier` and the item registry rather than a hardcoded string
/// table, so a renamed or added tool tier stays correct without a second
/// place to update. Pure and unit-tested.
fn missing_tool_text(
    item_reg: &ItemRegistry,
    block: &BlockDef,
    tool: Option<ToolDef>,
) -> Option<String> {
    if tool::can_harvest(block, tool.as_ref()) {
        return None;
    }
    let tier = block.harvest_tier?;
    let kind = block.tool?;
    let name = tool_name_for(item_reg, kind, tier)?;
    Some(format!("Needs a {}", name.replace('_', " ")))
}

/// The catalog name of a tool matching `kind`/`tier` exactly, if the item
/// registry has one. A linear scan is fine: the catalog is a handful of
/// items (design.md's small-catalog discipline).
fn tool_name_for(item_reg: &ItemRegistry, kind: ToolKind, tier: ToolTier) -> Option<&'static str> {
    (1..item_reg.len() as u16).find_map(|id| {
        let def = item_reg.get(ItemId(id));
        def.tool
            .filter(|t| t.kind == kind && t.tier == tier)
            .map(|_| def.name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_protocol::{ServerTransport, local};
    use tsumiki_world::{BlockRegistry, blocks, items};

    fn interaction_app(item: ItemId) -> (App, local::LocalServerTransport, Entity) {
        let (server, client) = local::pair();
        let mut app = App::new();
        let mut game_state = GameState::default();
        game_state.main[0] = Some(ItemStack::one(item));
        app.insert_resource(net::Transport::new(Box::new(client)))
            .insert_resource(game_state)
            .insert_resource(state::GameMode(tsumiki_protocol::GameMode::Survival))
            .insert_resource(ItemReg(ItemRegistry::prototype()))
            .insert_resource(view::Registry(BlockRegistry::prototype()))
            .insert_resource(State::new(pause::PauseState::Playing))
            .init_resource::<ChunkStore>()
            .init_resource::<Hotbar>()
            .init_resource::<TargetedBlock>()
            .init_resource::<MiningProgress>()
            .init_resource::<ButtonInput<MouseButton>>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Time>()
            .add_systems(
                Update,
                handle_mining_and_placing
                    .run_if(pause::is_playing)
                    .run_if(state::is_alive),
            );
        let window = app
            .world_mut()
            .spawn((
                PrimaryWindow,
                CursorOptions {
                    grab_mode: CursorGrabMode::Locked,
                    ..default()
                },
            ))
            .id();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Right);
        (app, server, window)
    }

    #[test]
    fn food_right_click_sends_eat_without_a_target_and_waits_for_server_inventory() {
        let (mut app, mut server, _) = interaction_app(items::BREAD);
        app.update();
        assert!(matches!(
            server.try_recv(),
            Some((_, ClientToServer::Eat { hotbar: 0 }))
        ));
        assert!(server.try_recv().is_none());
        assert_eq!(
            app.world().resource::<GameState>().main[0],
            Some(ItemStack::one(items::BREAD))
        );
    }

    #[test]
    fn food_does_not_consume_grab_menu_or_death_clicks() {
        for gate in 0..5 {
            let (mut app, mut server, window) = interaction_app(items::TOAST);
            match gate {
                0 => {
                    app.world_mut()
                        .get_mut::<CursorOptions>(window)
                        .unwrap()
                        .grab_mode = CursorGrabMode::None
                }
                1 => app.world_mut().resource_mut::<GameState>().dead = true,
                2 => {
                    app.insert_resource(State::new(pause::PauseState::Inventory));
                }
                3 => app
                    .world_mut()
                    .resource_mut::<ButtonInput<KeyCode>>()
                    .press(KeyCode::Escape),
                _ => app
                    .world_mut()
                    .resource_mut::<ButtonInput<KeyCode>>()
                    .press(KeyCode::KeyE),
            }
            app.update();
            assert!(server.try_recv().is_none(), "gate {gate}");
        }
    }

    #[test]
    fn shovel_right_click_requests_tilling_without_predicting_soil_or_tool_wear() {
        let (mut app, mut server, _) = interaction_app(items::WOODEN_SHOVEL);
        let pos = IVec3::new(2, 2, 2);
        let mut chunk = tsumiki_world::Chunk::filled(blocks::AIR);
        chunk.set(pos.as_uvec3(), blocks::DIRT);
        app.world_mut()
            .resource_mut::<ChunkStore>()
            .chunks
            .insert(IVec3::ZERO, chunk);
        app.world_mut().resource_mut::<TargetedBlock>().0 = Some(RayHit {
            block: pos,
            face_normal: IVec3::Y,
            distance: 2.0,
        });
        app.update();
        assert!(
            matches!(server.try_recv(), Some((_, ClientToServer::TillSoil { pos: hit, hotbar: 0 })) if hit == pos)
        );
        assert_eq!(
            view::block_at(app.world().resource::<ChunkStore>(), pos),
            Some(blocks::DIRT)
        );
        assert_eq!(
            app.world().resource::<GameState>().main[0],
            Some(ItemStack::one(items::WOODEN_SHOVEL))
        );
    }

    #[test]
    fn tilling_requires_a_shovel_and_the_top_of_soil() {
        let items_reg = ItemRegistry::prototype();
        for shovel in [
            items::WOODEN_SHOVEL,
            items::STONE_SHOVEL,
            items::IRON_SHOVEL,
        ] {
            for block in [blocks::GRASS, blocks::DIRT] {
                assert!(can_till(block, IVec3::Y, items_reg.tool(shovel)));
                assert!(!can_till(block, IVec3::X, items_reg.tool(shovel)));
                assert!(!can_till(block, IVec3::NEG_Y, items_reg.tool(shovel)));
            }
            assert!(!can_till(blocks::STONE, IVec3::Y, items_reg.tool(shovel)));
        }
        assert!(!can_till(blocks::DIRT, IVec3::Y, None));
        assert!(!can_till(
            blocks::DIRT,
            IVec3::Y,
            items_reg.tool(items::IRON_AXE)
        ));
    }

    #[test]
    fn held_tool_is_none_for_bare_hands() {
        let reg = ItemRegistry::prototype();
        assert_eq!(held_tool(None, &reg), None);
    }

    #[test]
    fn held_tool_is_none_for_a_non_tool_item() {
        let reg = ItemRegistry::prototype();
        assert_eq!(held_tool(Some(ItemStack::one(items::STICK)), &reg), None);
    }

    #[test]
    fn held_tool_reads_the_selected_stack() {
        let reg = ItemRegistry::prototype();
        let expected = *reg.tool(items::STONE_PICKAXE).unwrap();
        assert_eq!(
            held_tool(Some(ItemStack::one(items::STONE_PICKAXE)), &reg),
            Some(expected)
        );
    }

    #[test]
    fn break_time_reflects_the_held_tool() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);

        let bare = held_tool(None, &items_reg);
        let pick = held_tool(Some(ItemStack::one(items::STONE_PICKAXE)), &items_reg);
        assert!(
            tool::break_time_secs(stone, pick.as_ref())
                < tool::break_time_secs(stone, bare.as_ref())
        );
    }

    #[test]
    fn break_time_is_penalized_for_the_wrong_tool_tier() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let ore = blocks_reg.get(blocks::IRON_ORE);

        let wood = held_tool(Some(ItemStack::one(items::WOODEN_PICKAXE)), &items_reg);
        let stone = held_tool(Some(ItemStack::one(items::STONE_PICKAXE)), &items_reg);
        assert!(
            tool::break_time_secs(ore, wood.as_ref()) > tool::break_time_secs(ore, stone.as_ref())
        );
    }

    #[test]
    fn missing_tool_text_is_none_once_the_gate_is_met() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);
        let pick = held_tool(Some(ItemStack::one(items::WOODEN_PICKAXE)), &items_reg);

        assert_eq!(missing_tool_text(&items_reg, stone, pick), None);
    }

    #[test]
    fn missing_tool_text_names_the_required_tool_bare_handed() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);

        assert_eq!(
            missing_tool_text(&items_reg, stone, None),
            Some("Needs a wooden pickaxe".to_string())
        );
    }

    #[test]
    fn missing_tool_text_names_a_higher_tier_for_iron_ore() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let ore = blocks_reg.get(blocks::IRON_ORE);
        let wood = held_tool(Some(ItemStack::one(items::WOODEN_PICKAXE)), &items_reg);

        assert_eq!(
            missing_tool_text(&items_reg, ore, wood),
            Some("Needs a stone pickaxe".to_string())
        );
    }

    #[test]
    fn missing_tool_text_is_none_for_an_ungated_block() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let dirt = blocks_reg.get(blocks::DIRT);

        assert_eq!(missing_tool_text(&items_reg, dirt, None), None);
    }
}
