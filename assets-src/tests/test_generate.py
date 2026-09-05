"""Validate shipped asset contracts, reproducibility, palette and tile seams."""

import importlib.util
import io
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from PIL import Image

SOURCE = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("generate", SOURCE / "generate.py")
generate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generate)


@pytest.fixture(scope="module")
def built():
    pipeline = generate.Pipeline(SOURCE)
    outputs, atlas, icons = pipeline.build()
    return pipeline, outputs, atlas, icons


def tile(image, rect):
    x, y, width, height = rect
    return image.crop((x, y, x + width, y + height))


def test_all_generated_pixels_use_the_shared_palette(built):
    pipeline, _, atlas, icons = built
    allowed = set(pipeline.colors.values()) | {generate.TRANSPARENT}
    assert len(pipeline.colors) == 32
    assert (0, 0, 0, 255) not in allowed
    assert (255, 255, 255, 255) not in allowed
    for image in [atlas, icons]:
        assert set(image.get_flattened_data()) <= allowed


def test_block_face_coverage_and_fixed_shader_addresses(built):
    _, outputs, atlas, _ = built
    metadata = json.loads(outputs["atlas.json"])
    assert atlas.size == (128, 192)
    assert metadata["face_order"] == ["-X", "+X", "-Y", "+Y", "-Z", "+Z"]
    assert [block["id"] for block in metadata["blocks"]] == list(range(16))
    seen = set()
    for block in metadata["blocks"]:
        assert len(block["faces"]) == 6
        for face_index, face in enumerate(block["faces"]):
            index = block["id"] * 6 + face_index
            assert face["index"] == index
            assert face["rect"] == [index % 8 * 16, index // 8 * 16, 16, 16]
            assert tuple(face["rect"]) not in seen
            seen.add(tuple(face["rect"]))
            alpha = set(tile(atlas, face["rect"]).getchannel("A").get_flattened_data())
            assert alpha == ({0} if block["id"] == 0 else {255})
    assert len(seen) == 96
    # Front faces must remain visually distinct and use the -Z slot, since
    # the renderer does not infer orientation from the texture names.
    for block_id in [10, 14]:
        faces = metadata["blocks"][block_id]["faces"]
        assert tile(atlas, faces[0]["rect"]).tobytes() != tile(atlas, faces[4]["rect"]).tobytes()


def test_every_declared_world_facing_edge_tiles(built):
    _, outputs, atlas, _ = built
    for block in json.loads(outputs["atlas.json"])["blocks"]:
        for face in block["faces"]:
            generate.verify_edges(tile(atlas, face["rect"]), face["tiling"])
    grass_side = json.loads(outputs["atlas.json"])["blocks"][3]["faces"][0]
    assert grass_side["tiling"] == "x"
    with pytest.raises(ValueError, match="vertical"):
        generate.verify_edges(tile(atlas, grass_side["rect"]), "xy")


def test_edge_verification_rejects_a_broken_endpoint():
    image = Image.new("RGBA", (16, 16), (100, 120, 140, 255))
    image.putpixel((0, 5), (80, 90, 100, 255))
    with pytest.raises(ValueError, match="horizontal"):
        generate.verify_edges(image, "x")


def test_icon_coverage_transparency_and_placeable_mapping(built):
    _, outputs, _, icons = built
    metadata = json.loads(outputs["icons.json"])
    assert icons.size == (128, 64)
    assert [item["id"] for item in metadata["items"]] == list(range(1, 26))
    visible = []
    for item in metadata["items"]:
        assert item["rect"] == generate.tile_rect(item["id"])
        icon = tile(icons, item["rect"])
        assert set(icon.getchannel("A").get_flattened_data()) == {0, 255}
        visible.append(icon.tobytes())
    assert len(set(visible)) == 25, "each item must have a distinct readable icon"
    for index in [0, *range(26, 32)]:
        assert tile(icons, generate.tile_rect(index)).getbbox() is None
    items = {item["name"]: item for item in metadata["items"]}
    assert items["iron_ore"]["placeable_block"] is None
    assert items["torch"]["placeable_block"] == 15
    for name in ["coal", "stick", "iron_ingot", "wooden_pickaxe", "stone_axe", "iron_shovel"]:
        assert items[name]["placeable_block"] is None


def test_lod_mean_is_linear_light_and_ignores_transparent_pixels():
    image = Image.new("RGBA", (3, 1))
    image.putdata([(0, 0, 0, 255), (255, 255, 255, 255), (0, 0, 0, 0)])
    assert generate.mean_color([image]) == [188, 188, 188]
    assert generate.mean_color([Image.new("RGBA", (16, 16))]) == [0, 0, 0]


def test_lod_colors_match_the_shipped_face_textures(built):
    _, outputs, atlas, _ = built
    metadata = json.loads(outputs["atlas.json"])
    lod = json.loads(outputs["lod_colors.json"])
    assert [entry["id"] for entry in lod] == list(range(16))
    for entry, block in zip(lod, metadata["blocks"], strict=True):
        faces = [tile(atlas, face["rect"]) for face in block["faces"]]
        assert entry["top"] == generate.mean_color([faces[3]])
        assert entry["bottom"] == generate.mean_color([faces[2]])
        assert entry["side"] == generate.mean_color([faces[i] for i in [0, 1, 4, 5]])


def test_palette_swap_changes_ore_but_preserves_the_stone_layer():
    pipeline = generate.Pipeline(SOURCE)
    stone = pipeline.layer("stone")
    coal = pipeline.layer("coal_ore")
    iron = pipeline.layer("iron_ore")
    unchanged, replaced = 0, 0
    for base, a, b in zip(
        stone.get_flattened_data(),
        coal.get_flattened_data(),
        iron.get_flattened_data(),
        strict=True,
    ):
        if a == base:
            assert b == base
            unchanged += 1
        else:
            assert a != b
            replaced += 1
    assert unchanged > replaced > 0


def test_a_cycle_is_reported_instead_of_recursing_forever():
    pipeline = generate.Pipeline(SOURCE)
    pipeline.layers["stone"] = {"kind": "overlay", "base": "stone", "style": "ore"}
    with pytest.raises(ValueError, match="cyclic layer"):
        pipeline.layer("stone")


def test_png_bytes_are_deterministic_and_decode_to_the_generated_pixels(built):
    _, outputs, atlas, icons = built
    another, _, _ = generate.Pipeline(SOURCE).build()
    assert another == outputs
    for name, expected in [("atlas.png", atlas), ("icons.png", icons)]:
        decoded = Image.open(io.BytesIO(outputs[name])).convert("RGBA")
        assert decoded.size == expected.size
        assert decoded.tobytes() == expected.tobytes()


def test_cli_is_reproducible_across_processes_and_check_detects_stale_output(tmp_path, built):
    _, outputs, _, _ = built
    command = [sys.executable, str(SOURCE / "generate.py"), "--output", str(tmp_path)]
    first = subprocess.run(command, capture_output=True, text=True, check=False)
    assert first.returncode == 0, first.stderr
    for name, data in outputs.items():
        assert (tmp_path / name).read_bytes() == data
    second = subprocess.run(
        command + ["--check"],
        env={**os.environ, "PYTHONHASHSEED": "987"},
        capture_output=True,
        text=True,
        check=False,
    )
    assert second.returncode == 0, second.stderr
    (tmp_path / "icons.json").write_bytes(b"stale")
    stale = subprocess.run(command + ["--check"], capture_output=True, text=True, check=False)
    assert stale.returncode == 1
    assert "icons.json" in stale.stderr
    assert (tmp_path / "icons.json").read_bytes() == b"stale", "check must never write output"
