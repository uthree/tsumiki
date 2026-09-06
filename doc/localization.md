# Languages and item labels

Settings on the title screen and pause menu include a language button. Click
it to cycle between English and Japanese. The choice applies immediately
and is saved as `language` (`en` or `ja`) in `settings.json`. Existing settings
files without this field retain English.

Selecting a populated hotbar slot shows its localized item name briefly
above the hotbar. Hovering an occupied inventory slot shows the same name
beside the pointer, including chest and furnace slots. Empty slots have no
tooltip. Item identifiers, world names and player names stay unchanged.

## Adding a language

Translations live in `assets/locales/en.json` and `ja.json`. Keys describe
their meaning, such as `item.stone`, rather than using displayed text as an
identifier. `crates/client/src/i18n.rs` embeds the catalogs, resolves keys
and substitutes named placeholders. Missing translations fall back to
English; unknown item names have a readable identifier-based fallback.

To add another language:

1. Add its catalog with the English keys and the same named placeholders.
2. Register its stable code, native display name and catalog in `Language`
   and the catalog lookup in `i18n.rs`.
3. Include it in the catalog consistency tests and check that the bundled
   font covers its characters.

Use `LocalizedText` for text that should update when the language changes,
and `tr`, `tr_args` or `item_name` when building dynamic displays. Inserted
values are not interpreted as translation templates, so user-entered names
containing braces remain intact. The bundled Misaki Gothic font supports
the English and Japanese UI.

`--language en|ja` temporarily overrides the loaded setting without saving
the override on startup. It is useful for visual verification:

```sh
cargo run -- --language ja --settings-screenshot target/settings-ja.png
cargo run -- --language en --settings-screenshot target/settings-en.png
cargo run -- --language ja --ephemeral --inventory-screenshot target/inventory-ja.png
cargo run -- --language ja --ephemeral --hotbar-screenshot target/hotbar-ja.png
```

The inventory capture moves the pointer over an occupied slot. The hotbar
capture switches slots after the terrain has loaded, while the name is visible.
