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
    assert atlas.size == (128, 336)
    assert metadata["face_order"] == ["-X", "+X", "-Y", "+Y", "-Z", "+Z"]
    assert [block["id"] for block in metadata["blocks"]] == list(range(27))
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
            expected = {0} if block["id"] == 0 else {0, 255} if block["id"] in {20, 21} else {255}
            assert alpha == expected
    assert len(seen) == 162
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
    assert icons.size == (256, 160)
    assert metadata["tile_size"] == 32
    assert [item["id"] for item in metadata["items"]] == list(range(1, 39))
    visible = []
    for item in metadata["items"]:
        assert item["rect"] == generate.tile_rect(item["id"], 32)
        icon = tile(icons, item["rect"])
        assert set(icon.getchannel("A").get_flattened_data()) == {0, 255}
        visible.append(icon.tobytes())
    assert len(set(visible)) == 38, "each item must have a distinct readable icon"
    for index in [0, 39]:
        assert tile(icons, generate.tile_rect(index, 32)).getbbox() is None
    items = {item["name"]: item for item in metadata["items"]}
    assert items["iron_ore"]["placeable_block"] is None
    assert items["torch"]["placeable_block"] == 15
    for name, block in [("demo_red_light", 16), ("demo_green_light", 17), ("demo_blue_light", 18)]:
        assert items[name]["placeable_block"] == block
    for name in ["coal", "stick", "iron_ingot", "wooden_pickaxe", "stone_axe", "iron_shovel"]:
        assert items[name]["placeable_block"] is None


def test_lod_mean_is_linear_light_and_ignores_transparent_pixels():
    image = Image.new("RGBA", (3, 1))
    image.putdata([(0, 0, 0, 255), (255, 255, 255, 255), (0, 0, 0, 0)])
    assert generate.mean_color([image]) == [188, 188, 188]
    assert generate.mean_color([Image.new("RGBA", (16, 16))]) == [0, 0, 0]


def test_crop_sprites_have_transparent_canopies_and_distinct_growth_stages(built):
    pipeline, _, _, _ = built
    young = pipeline.layer("wheat_young")
    mature = pipeline.layer("wheat_mature")
    assert young.getbbox()[1] > mature.getbbox()[1]
    for crop in [young, mature]:
        assert crop.getbbox()[3] == 16, "stems meet the soil"
        assert crop.getpixel((0, 0))[3] == 0
        assert crop.getpixel((15, 0))[3] == 0
        assert set(crop.getchannel("A").get_flattened_data()) == {0, 255}
    green = pipeline.color("green.light")
    gold = pipeline.color("gold.light")
    assert green in set(young.get_flattened_data())
    assert gold in set(mature.get_flattened_data())


def test_machine_textures_and_food_icons_remain_distinct(built):
    pipeline, outputs, atlas, icons = built
    metadata = json.loads(outputs["atlas.json"])
    fronts = [tile(atlas, metadata["blocks"][index]["faces"][4]["rect"]) for index in [22, 24, 25]]
    assert len({face.tobytes() for face in fronts}) == 3
    for face in fronts:
        assert pipeline.color("metal.high") in set(face.get_flattened_data())
    food = [tile(icons, generate.tile_rect(index, 32)) for index in [30, 31, 32, 33]]
    assert len({icon.tobytes() for icon in food}) == 4


def test_cube_icons_have_a_closed_isometric_silhouette_without_holes(built):
    pipeline, outputs, _, icons = built
    metadata = json.loads(outputs["icons.json"])
    cubes = {item["id"] for item in pipeline.items if "block" in item and "style" not in item}
    for item in metadata["items"]:
        if item["id"] not in cubes:
            continue
        icon = tile(icons, item["rect"])
        assert icon.getbbox() == (3, 1, 29, 31)
        for y in range(32):
            for x in range(32):
                px, py = x + 0.5, y + 0.5
                # The projected cube has vertical outer edges and symmetric
                # 30-degree upper/lower edges, not a rounded pebble outline.
                offset = abs(px - 16) * 7.5 / 13
                expected = 3 <= px <= 29 and 1 + offset <= py <= 31 - offset
                assert bool(icon.getpixel((x, y))[3]) == expected, (item["name"], x, y)


def test_cube_faces_are_distinctly_shaded_without_an_extra_outline_color():
    pipeline = generate.Pipeline(SOURCE)
    flat = pipeline.color("neutral.cream")
    pipeline.cache["flat_test"] = Image.new("RGBA", (16, 16), flat)
    pipeline.blocks[1] = {"id": 1, "name": "test_cube", "side": "flat_test"}
    icon = pipeline.cube_icon(1)
    top = flat
    left = pipeline.nearest(tuple(round(p * 0.8) for p in flat[:3]))
    right = pipeline.nearest(tuple(round(p * 0.6) for p in flat[:3]))
    assert len({top, left, right}) == 3
    assert icon.getpixel((16, 6)) == top
    assert icon.getpixel((7, 16)) == left
    assert icon.getpixel((24, 16)) == right
    assert set(icon.get_flattened_data()) == {generate.TRANSPARENT, top, left, right}


def test_cube_samples_top_side_and_front_in_the_correct_orientation():
    pipeline = generate.Pipeline(SOURCE)
    palette = pipeline.color
    pipeline.cache["test_top"] = Image.new("RGBA", (16, 16), palette("red.high"))
    side = Image.new("RGBA", (16, 16), palette("green.high"))
    front = Image.new("RGBA", (16, 16), palette("blue.high"))
    for x in range(16):
        for y in range(4):
            side.putpixel((x, y), palette("neutral.cream"))
            front.putpixel((x, y), palette("metal.high"))
        for y in range(12, 16):
            side.putpixel((x, y), palette("green.shadow"))
            front.putpixel((x, y), palette("blue.shadow"))
    pipeline.cache["test_side"] = side
    pipeline.cache["test_front"] = front
    pipeline.blocks[1] = {
        "id": 1,
        "name": "test_cube",
        "top": "test_top",
        "side": "test_side",
        "front": "test_front",
    }
    icon = pipeline.cube_icon(1)

    def shade(name, value):
        return pipeline.nearest(tuple(round(p * value) for p in palette(name)[:3]))

    assert icon.getpixel((16, 6)) == palette("red.high")
    assert icon.getpixel((6, 11)) == shade("neutral.cream", 0.8)
    assert icon.getpixel((6, 22)) == shade("green.shadow", 0.8)
    assert icon.getpixel((25, 11)) == shade("metal.high", 0.6)
    assert icon.getpixel((25, 22)) == shade("blue.shadow", 0.6)


def test_authored_material_and_tool_icons_are_exact_nearest_neighbor_copies(built):
    pipeline, outputs, _, icons = built
    metadata = {item["id"]: item for item in json.loads(outputs["icons.json"])["items"]}
    for item in pipeline.items:
        if "style" not in item:
            continue
        source = pipeline.authored_icon(item)
        icon = tile(icons, metadata[item["id"]]["rect"])
        for y in range(32):
            for x in range(32):
                assert icon.getpixel((x, y)) == source.getpixel((x // 2, y // 2))


def test_demo_lamp_frames_have_distinct_rgb_hues_and_bright_inlays(built):
    pipeline, outputs, atlas, _ = built
    blocks = json.loads(outputs["atlas.json"])["blocks"]
    for block_id, name, channel in [
        (16, "demo_red_light", 0),
        (17, "demo_green_light", 1),
        (18, "demo_blue_light", 2),
    ]:
        block = blocks[block_id]
        assert block["name"] == name
        face = tile(atlas, block["faces"][0]["rect"])
        frame = face.getpixel((2, 7))[:3]
        assert frame[channel] > max(v for i, v in enumerate(frame) if i != channel)
        assert face.getpixel((4, 4)) == pipeline.color("neutral.cream")
        assert sum(face.getpixel((8, 8))[:3]) > sum(frame)


def test_lod_colors_match_the_shipped_face_textures(built):
    _, outputs, atlas, _ = built
    metadata = json.loads(outputs["atlas.json"])
    lod = json.loads(outputs["lod_colors.json"])
    assert [entry["id"] for entry in lod] == list(range(27))
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
