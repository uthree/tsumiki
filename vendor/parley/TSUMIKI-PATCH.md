# Japanese text segmentation

This directory contains the library sources, normalized Cargo manifest, README,
and licenses from the published `parley` 0.9.0 crate. Its upstream commit is
`65fd4ad705deabc89b732cf7e8de9245466a2450` in
<https://github.com/linebender/parley> (`parley/` directory). The original crate
SHA-256 is `8fad031076f48f0d4d85ce1aea9b94b4e715a4d636a030a123038f8f5b5e4343`.

The only source change is in `src/analysis/mod.rs`: the word and line segmenter
constructors use ICU's compiled dictionary models instead of the constructors
for non-complex scripts. The old word segmenter had no Japanese model and
reported an ICU data error for each Japanese text run. The existing
`icu_segmenter/compiled_data` feature includes all required dictionary data;
no additional runtime downloads or Cargo features are needed.

The workspace selects this copy with `[patch.crates-io]`. The client regression
test `japanese_layout_retains_dictionary_word_boundaries` verifies the patched
backend's actual layout boundaries with the bundled font. The upstream MIT and
Apache licenses are retained alongside this note.

When the Bevy text backend includes complex-script segmentation upstream,
upgrade it, remove the Cargo patch and this directory, and rerun that test and
the Japanese settings/inventory screenshot checks.
