//! Shared pixel-art item atlas. Cell zero is transparent; every other cell
//! is addressed by ItemId, matching the generated 8-column icon sheet.

use bevy::image::{ImageLoaderSettings, ImageSampler};
use bevy::prelude::*;
use tsumiki_world::ItemId;

const CELL_SIZE: u16 = 32;
const COLUMNS: u16 = 8;
const ROWS: u16 = 4;
pub(crate) const ATLAS_SIZE: Vec2 = Vec2::new(256.0, 128.0);

/// Pixel bounds for one icon. Invalid sheet indices use the transparent
/// cell, preventing an accidental sample from an unrelated item.
pub(crate) fn rect(item: ItemId) -> Rect {
    let index = if item.0 < COLUMNS * ROWS { item.0 } else { 0 };
    let x = f32::from((index % COLUMNS) * CELL_SIZE);
    let y = f32::from((index / COLUMNS) * CELL_SIZE);
    Rect::new(x, y, x + f32::from(CELL_SIZE), y + f32::from(CELL_SIZE))
}

#[derive(Resource)]
pub(crate) struct ItemIcons {
    pub(crate) image: Handle<Image>,
}

impl FromWorld for ItemIcons {
    fn from_world(world: &mut World) -> Self {
        Self {
            image: world
                .resource::<AssetServer>()
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.sampler = ImageSampler::nearest()
                })
                .load("icons.png"),
        }
    }
}

impl ItemIcons {
    pub(crate) fn node(&self, item: ItemId) -> ImageNode {
        ImageNode::new(self.image.clone()).with_rect(rect(item))
    }
}

pub(crate) fn install(app: &mut App) {
    app.init_resource::<ItemIcons>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::{ItemRegistry, items};

    #[test]
    fn every_registered_item_has_a_distinct_in_bounds_icon() {
        let registry = ItemRegistry::prototype();
        let mut bounds = Vec::new();
        for index in 1..registry.len() {
            let icon = rect(ItemId(index as u16));
            assert_eq!(icon.size(), Vec2::splat(f32::from(CELL_SIZE)));
            assert!(icon.min.cmpge(Vec2::ZERO).all());
            assert!(icon.max.cmple(ATLAS_SIZE).all());
            assert_ne!(icon, rect(ItemId(0)));
            assert!(!bounds.contains(&icon));
            bounds.push(icon);
        }
        assert_eq!(rect(items::TORCH), Rect::new(32.0, 96.0, 64.0, 128.0));
    }

    #[test]
    fn row_boundaries_and_unknown_ids_do_not_sample_neighboring_icons() {
        assert_eq!(rect(ItemId(7)), Rect::new(224.0, 0.0, 256.0, 32.0));
        assert_eq!(rect(ItemId(8)), Rect::new(0.0, 32.0, 32.0, 64.0));
        assert_eq!(rect(ItemId(32)), rect(ItemId(0)));
        assert_eq!(rect(ItemId(u16::MAX)), rect(ItemId(0)));
    }
}
