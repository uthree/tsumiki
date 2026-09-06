//! Climate regions and their stable names, shared by terrain and presentation.

use serde::{Deserialize, Serialize};

/// The terrain recipe is part of a world's identity: saved edits must never
/// become surrounded by terrain generated with a different recipe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationVersion {
    Legacy,
    #[default]
    Biomes,
}

/// A column's dominant climate region. Heights blend continuous climate
/// fields independently of these discrete labels, so borders have no steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Biome {
    #[default]
    Plains,
    Forest,
    Desert,
    Tundra,
    Mountains,
}

impl Biome {
    pub const ALL: [Self; 5] = [
        Self::Plains,
        Self::Forest,
        Self::Desert,
        Self::Tundra,
        Self::Mountains,
    ];

    /// Stable localization key suffix, independent of the displayed language.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plains => "plains",
            Self::Forest => "forest",
            Self::Desert => "desert",
            Self::Tundra => "tundra",
            Self::Mountains => "mountains",
        }
    }

    /// Eligible tree anchors per column; zero denotes treeless terrain.
    pub(crate) const fn tree_divisor(self) -> u64 {
        match self {
            Self::Plains => 160,
            Self::Forest => 16,
            Self::Desert | Self::Tundra => 0,
            Self::Mountains => 100,
        }
    }
}
