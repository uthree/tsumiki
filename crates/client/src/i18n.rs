//! Client-only translation catalogs. Wire identifiers and saved names stay stable.
//!
//! Add a locale to `Language` and `catalog`, then supply the same keys as English.
//! Missing translations fall back to English; unknown keys remain readable.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "ja")]
    Japanese,
    #[default]
    #[serde(rename = "en", other)]
    English,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::Japanese];

    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Japanese => "日本語",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|&language| language == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

type Catalog = BTreeMap<String, String>;

static ENGLISH: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../assets/locales/en.json"))
        .expect("bundled English translations must be valid JSON")
});
static JAPANESE: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../assets/locales/ja.json"))
        .expect("bundled Japanese translations must be valid JSON")
});

fn catalog(language: Language) -> &'static Catalog {
    match language {
        Language::English => &ENGLISH,
        Language::Japanese => &JAPANESE,
    }
}

pub fn tr(language: Language, key: &str) -> String {
    lookup(catalog(language), &ENGLISH, key).to_owned()
}

fn lookup<'a>(locale: &'a Catalog, english: &'a Catalog, key: &'a str) -> &'a str {
    locale
        .get(key)
        .or_else(|| english.get(key))
        .map_or(key, String::as_str)
}

/// Substitutes named placeholders once, keeping inserted user text opaque.
pub fn tr_args(language: Language, key: &str, args: &[(&str, String)]) -> String {
    let template = tr(language, key);
    let mut result = String::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}').map(|end| start + end) else {
            result.push_str(&rest[start..]);
            return result;
        };
        let name = &rest[start + 1..end];
        if let Some((_, value)) = args.iter().find(|(key, _)| *key == name) {
            result.push_str(value);
        } else {
            result.push_str(&rest[start..=end]);
        }
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result
}

pub fn item_name(language: Language, name: &str) -> String {
    let key = format!("item.{name}");
    let translated = tr(language, &key);
    if translated != key {
        return translated;
    }
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Static text or parameterized text that follows language changes live.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct LocalizedText {
    key: String,
    args: Vec<(String, String)>,
}

impl LocalizedText {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            args: Vec::new(),
        }
    }

    pub fn with_args(key: impl Into<String>, args: Vec<(String, String)>) -> Self {
        Self {
            key: key.into(),
            args,
        }
    }

    pub fn render(&self, language: Language) -> String {
        let args: Vec<_> = self
            .args
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        tr_args(language, &self.key, &args)
    }
}

fn update_localized_text(
    settings: Res<Settings>,
    mut labels: Query<(&mut Text, Ref<LocalizedText>)>,
) {
    for (mut text, label) in &mut labels {
        if settings.is_changed() || label.is_changed() {
            text.0 = label.render(settings.language);
        }
    }
}

pub fn install(app: &mut App) {
    // Update after gameplay/menu changes and before Bevy measures text for UI.
    app.add_systems(
        PostUpdate,
        update_localized_text.before(bevy::ui::UiSystems::Content),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_layout_retains_dictionary_word_boundaries() {
        use bevy::text::{FontCx, LayoutCx};
        use parley::{StyleProperty, WordBreak};

        let mut fonts = FontCx::default();
        let font =
            Font::from_bytes(include_bytes!("../../../assets/fonts/misaki_gothic.ttf").to_vec());
        let families = fonts.collection.register_fonts(font.data, None);
        let family = fonts
            .collection
            .family_name(families[0].0)
            .unwrap()
            .to_owned();
        fonts.set_sans_serif_family(&family).unwrap();
        let mut context = LayoutCx::default();
        let text = "こんにちは世界";
        let mut builder = context.ranged_builder(&mut fonts.context, text, 1.0, true);
        // Exclude per-character CJK line boundaries so the actual dictionary
        // word boundary between the greeting and noun remains observable.
        builder.push_default(StyleProperty::WordBreak(WordBreak::KeepAll));
        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        let mut boundaries = Vec::new();
        for line in layout.lines() {
            for run in line.runs() {
                for cluster in run.clusters() {
                    if cluster.is_word_boundary() {
                        boundaries.push(cluster.text_range().start);
                    }
                }
            }
        }
        assert_eq!(boundaries, [0, 15]);
    }

    #[test]
    fn catalogs_have_identical_keys_and_placeholders() {
        fn placeholders(text: &str) -> Vec<&str> {
            let mut names: Vec<_> = text
                .split('{')
                .skip(1)
                .filter_map(|part| part.split_once('}').map(|(name, _)| name))
                .collect();
            names.sort_unstable();
            names
        }
        for language in Language::ALL {
            let locale = catalog(language);
            assert_eq!(
                ENGLISH.keys().collect::<Vec<_>>(),
                locale.keys().collect::<Vec<_>>()
            );
            for (key, english) in ENGLISH.iter() {
                assert_eq!(placeholders(english), placeholders(&locale[key]), "{key}");
            }
        }
    }

    #[test]
    fn substitutions_do_not_interpret_user_supplied_placeholders() {
        assert_eq!(
            tr_args(
                Language::English,
                "world.delete_confirm",
                &[("name", "{name}".into())]
            ),
            "Delete \"{name}\"? This cannot be undone."
        );
    }

    #[test]
    fn an_incomplete_locale_falls_back_to_english_per_key() {
        let partial = Catalog::from([("menu.settings".into(), "設定".into())]);
        assert_eq!(lookup(&partial, &ENGLISH, "menu.settings"), "設定");
        assert_eq!(lookup(&partial, &ENGLISH, "menu.back"), "Back");
    }

    #[test]
    fn labels_follow_live_language_changes_and_new_entities() {
        let mut app = App::new();
        app.init_resource::<Settings>();
        install(&mut app);
        let label = app
            .world_mut()
            .spawn((Text::default(), LocalizedText::new("menu.settings")))
            .id();
        app.update();
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Settings");
        app.world_mut().resource_mut::<Settings>().language = Language::Japanese;
        app.update();
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "設定");
        let item = app
            .world_mut()
            .spawn((Text::default(), LocalizedText::new("item.stone")))
            .id();
        app.update();
        assert_eq!(app.world().get::<Text>(item).unwrap().0, "石");
    }

    #[test]
    fn every_item_has_translations_and_unknown_items_remain_readable() {
        let registry = tsumiki_world::ItemRegistry::prototype();
        for id in 1..registry.len() {
            let item = registry.get(tsumiki_world::ItemId(id as u16));
            let key = format!("item.{}", item.name);
            for language in Language::ALL {
                assert!(catalog(language).contains_key(&key), "{key}");
            }
        }
        assert_eq!(
            item_name(Language::Japanese, "future_block"),
            "Future Block"
        );
        assert_eq!(tr(Language::Japanese, "future.key"), "future.key");
    }
}
