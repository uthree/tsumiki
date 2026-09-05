"""Build 16 px block textures and 32 px icons from a shared 32-color palette.

Run with ``uv run generate.py`` from assets-src, or pass --check to verify
the committed bytes. All composition uses large pixel clusters rather than
per-pixel white noise. LOD colors average opaque pixels in linear light and
encode the result as sRGB; they are not an additional texture palette.
"""

from __future__ import annotations

import argparse
import io
import json
import random
import tomllib
from pathlib import Path

from PIL import Image, ImageDraw

TILE = 16
ICON_TILE = 32
FACE_ORDER = ["-X", "+X", "-Y", "+Y", "-Z", "+Z"]
ATLAS_SIZE = (128, 336)
ICONS_SIZE = (256, 160)
TRANSPARENT = (0, 0, 0, 0)


def rgba(hex_color: str) -> tuple[int, int, int, int]:
    return tuple(bytes.fromhex(hex_color.removeprefix("#"))) + (255,)


def tile_rect(index: int, size: int = TILE) -> list[int]:
    return [index % 8 * size, index // 8 * size, size, size]


def seal_edges(image: Image.Image, axes: str = "xy") -> Image.Image:
    """Duplicate periodic endpoints; deliberate directional faces use x only."""
    image = image.copy()
    if "x" in axes:
        for y in range(TILE):
            image.putpixel((15, y), image.getpixel((0, y)))
    if "y" in axes:
        for x in range(TILE):
            image.putpixel((x, 15), image.getpixel((x, 0)))
    return image


def verify_edges(image: Image.Image, axes: str) -> None:
    if "x" in axes and any(image.getpixel((0, p)) != image.getpixel((15, p)) for p in range(16)):
        raise ValueError("horizontal tile edge mismatch")
    if "y" in axes and any(image.getpixel((p, 0)) != image.getpixel((p, 15)) for p in range(16)):
        raise ValueError("vertical tile edge mismatch")


class Pipeline:
    def __init__(self, source: Path):
        palette = json.loads((source / "palette.json").read_text(encoding="utf-8"))
        self.colors = {name: rgba(color) for name, color in palette["colors"].items()}
        if len(self.colors) != 32 or len(set(self.colors.values())) != 32:
            raise ValueError("palette must contain 32 distinct colors")
        with (source / "blocks.toml").open("rb") as file:
            blocks = tomllib.load(file)
        with (source / "items.toml").open("rb") as file:
            items = tomllib.load(file)
        self.layers = blocks["layers"]
        self.blocks = sorted(blocks["blocks"], key=lambda block: block["id"])
        self.items = sorted(items["items"], key=lambda item: item["id"])
        if [block["id"] for block in self.blocks] != list(range(27)):
            raise ValueError("block atlas requires each block ID 0..26 exactly once")
        if [item["id"] for item in self.items] != list(range(1, 39)):
            raise ValueError("icon atlas requires each item ID 1..38 exactly once")
        self.cache: dict[str, Image.Image] = {}

    def color(self, name: str):
        return self.colors[name]

    def nearest(self, color: tuple[int, ...]):
        return min(
            self.colors.values(), key=lambda p: sum((p[i] - color[i]) ** 2 for i in range(3))
        )

    def quantize(self, image: Image.Image) -> Image.Image:
        """Nearest shared color, without dithering or semi-transparent pixels."""
        result = Image.new("RGBA", image.size)
        mapping = {}
        for pixel in set(image.get_flattened_data()):
            mapping[pixel] = TRANSPARENT if pixel[3] == 0 else self.nearest(pixel)
        result.putdata([mapping[pixel] for pixel in image.get_flattened_data()])
        return result

    def layer(self, name: str, ancestors: tuple[str, ...] = ()) -> Image.Image:
        if name in self.cache:
            return self.cache[name].copy()
        if name in ancestors:
            raise ValueError(f"cyclic layer reference: {ancestors + (name,)}")
        recipe = self.layers[name]
        kind = recipe["kind"]
        if kind == "noise":
            image = self.noise(recipe)
        elif kind == "swap":
            image = self.layer(recipe["base"], ancestors + (name,))
            mapping = {self.color(a): self.color(b) for a, b in recipe["mapping"].items()}
            image.putdata([mapping.get(p, p) for p in image.get_flattened_data()])
        elif kind == "overlay":
            image = self.layer(recipe["base"], ancestors + (name,))
            self.overlay(image, recipe)
        else:
            raise ValueError(f"unknown composition primitive: {kind}")
        axes = recipe.get("tiling", "xy")
        image = seal_edges(self.quantize(image), axes)
        verify_edges(image, axes)
        self.cache[name] = image
        return image.copy()

    def noise(self, recipe: dict) -> Image.Image:
        """Seeded, tileable cluster placement: a few broad patches per tile."""
        style = recipe["style"]
        if style == "air":
            return Image.new("RGBA", (TILE, TILE), TRANSPARENT)
        base, shadow, light, high = [self.color(name) for name in recipe["colors"]]
        image = Image.new("RGBA", (TILE, TILE), base)
        draw = ImageDraw.Draw(image)
        rng = random.Random(recipe["seed"])
        if style in {"stone", "dirt", "grass", "leaves", "sand"}:
            patches = [
                (1, 2, 5, 4),
                (9, 1, 14, 3),
                (6, 7, 11, 10),
                (0, 11, 4, 13),
                (12, 12, 17, 15),
            ]
            for n, (x0, y0, x1, y1) in enumerate(patches):
                shift = rng.randrange(-1, 2)
                box = (x0, y0 + shift, x1, y1 + shift)
                if style == "sand":
                    draw.line((x0, y0, x1, y0), fill=light, width=1)
                    if n == 2:
                        draw.line((x0 + 1, y0 + 2, x1 - 1, y0 + 2), fill=shadow)
                elif style in {"grass", "leaves"}:
                    draw.rounded_rectangle(box, radius=2, fill=light if n % 2 else shadow)
                    draw.line((x0 + 1, y0 + shift, x1 - 1, y0 + shift), fill=high)
                else:
                    draw.rounded_rectangle(box, radius=1, fill=light if n % 2 else shadow)
                    if style == "stone" and n % 2:
                        draw.line((x0 + 1, y0 + shift, x1 - 1, y0 + shift), fill=high)
                    if style == "dirt" and n == 2:
                        draw.rectangle((x0 + 1, y0 + 1, x0 + 2, y0 + 1), fill=high)
        elif style == "water":
            for x, y in [(-3, 3), (7, 9), (0, 14)]:
                draw.line((x, y, x + 3, y, x + 5, y - 1, x + 8, y - 1), fill=light, width=1)
                draw.line((x + 2, y + 2, x + 5, y + 2), fill=shadow)
                draw.point((x + 1, y), fill=high)
        elif style == "bark":
            for x in [1, 6, 11]:
                draw.line((x, 0, x, 5, x + 1, 7, x + 1, 15), fill=shadow, width=2)
                draw.line((x + 2, 0, x + 2, 4, x + 3, 6, x + 3, 15), fill=light)
            draw.rounded_rectangle((6, 7, 10, 12), radius=2, outline=shadow)
            draw.line((8, 9, 8, 10), fill=high)
        elif style == "planks":
            for y in [0, 5, 10]:
                draw.line((0, y, 15, y), fill=shadow)
                draw.line((0, y + 1, 15, y + 1), fill=light)
            for x, y in [(5, 1), (12, 6), (3, 11)]:
                draw.line((x, y, x, y + 3), fill=shadow)
                draw.line((x + 2, y + 2, x + 5, y + 2), fill=high)
        elif style == "cobble":
            for y, offset in [(-2, -3), (5, 1), (12, -3)]:
                for x in range(offset, 18, 8):
                    draw.rounded_rectangle((x, y, x + 6, y + 5), radius=1, fill=shadow)
                    draw.line((x + 1, y, x + 5, y), fill=light)
                    draw.line((x + 1, y + 1, x + 3, y + 1), fill=high)
        else:
            raise ValueError(f"unknown noise style: {style}")
        return image

    def overlay(self, image: Image.Image, recipe: dict) -> None:
        draw = ImageDraw.Draw(image)
        c = self.color
        style = recipe["style"]
        if style == "grass_edge":
            draw.rectangle((0, 0, 15, 2), fill=c("green.mid"))
            draw.line((0, 0, 15, 0), fill=c("green.light"))
            for x, depth in [(0, 3), (4, 5), (9, 3), (12, 4)]:
                draw.rectangle((x, 2, x + 2, depth), fill=c("green.mid"))
                draw.point((x + 1, depth), fill=c("green.shadow"))
        elif style == "log_end":
            draw.rounded_rectangle((1, 1, 14, 14), radius=3, fill=c("brown.high"))
            draw.rounded_rectangle((3, 3, 12, 12), radius=2, outline=c("brown.mid"))
            draw.rounded_rectangle((5, 5, 10, 10), radius=1, outline=c("brown.light"))
            draw.line((7, 7, 8, 7, 8, 9), fill=c("brown.mid"))
            draw.line((1, 9, 3, 9), fill=c("brown.shadow"))
        elif style == "ore":
            dark, mid, light = [c(name) for name in recipe["colors"]]
            for x, y in [(2, 3), (10, 2), (6, 10), (12, 11)]:
                draw.polygon(
                    [(x, y + 1), (x + 1, y), (x + 3, y), (x + 3, y + 2), (x + 1, y + 3)], fill=dark
                )
                draw.line((x + 1, y + 1, x + 2, y + 1), fill=light)
                draw.point((x + 2, y + 2), fill=mid)
        elif style.startswith("table"):
            draw.rectangle((0, 0, 15, 15), outline=c("brown.shadow"), width=2)
            if style == "table_top":
                draw.rectangle((3, 3, 12, 12), fill=c("brown.high"))
                for p in [3, 6, 9, 12]:
                    draw.line((3, p, 12, p), fill=c("brown.mid"))
                    draw.line((p, 3, p, 12), fill=c("brown.mid"))
                draw.point((4, 4), fill=c("neutral.cream"))
            else:
                draw.rectangle((2, 3, 13, 5), fill=c("brown.high"))
                draw.line((5, 7, 5, 12), fill=c("brown.shadow"), width=2)
                draw.rectangle((3, 7, 7, 8), fill=c("metal.light"))
                draw.line((10, 7, 10, 12), fill=c("metal.shadow"), width=2)
                draw.point((10, 7), fill=c("metal.high"))
        elif style.startswith("chest"):
            if style != "chest_front":
                draw.rectangle((0, 0, 15, 15), outline=c("brown.shadow"), width=2)
                draw.line((2, 2, 13, 2), fill=c("brown.high"))
                if style == "chest_top":
                    for x in [3, 11]:
                        draw.rectangle((x, 1, x + 1, 14), fill=c("gold.shadow"))
                        draw.line((x, 2, x, 13), fill=c("gold.light"))
                else:
                    draw.line((1, 6, 14, 6), fill=c("brown.shadow"))
                    draw.line((2, 7, 13, 7), fill=c("brown.high"))
            else:
                draw.rectangle((6, 5, 9, 9), fill=c("gold.shadow"))
                draw.rectangle((6, 5, 8, 8), fill=c("gold.light"))
                draw.point((7, 7), fill=c("brown.shadow"))
                draw.point((7, 5), fill=c("gold.high"))
        elif style.startswith("furnace"):
            if style == "furnace_side":
                draw.rounded_rectangle((0, 0, 15, 15), radius=1, outline=c("stone.shadow"), width=2)
                draw.line((2, 2, 13, 2), fill=c("stone.high"))
                for x, y in [(2, 12), (12, 12)]:
                    draw.rectangle((x, y, x + 1, y + 1), fill=c("metal.shadow"))
            elif style == "furnace_top":
                for y in [5, 8, 11]:
                    draw.line((4, y, 11, y), fill=c("metal.shadow"), width=2)
                    draw.line((4, y - 1, 11, y - 1), fill=c("metal.light"))
            else:
                draw.rounded_rectangle((3, 4, 12, 7), radius=1, fill=c("neutral.ink"))
                draw.rounded_rectangle((3, 9, 12, 12), radius=1, fill=c("neutral.ink"))
                draw.line((4, 5, 11, 5), fill=c("neutral.dim"))
                draw.line((5, 12, 10, 12), fill=c("gold.shadow"))
                draw.point((7, 11), fill=c("gold.mid"))
        elif style == "furrows":
            for y in [0, 5, 10]:
                draw.line((0, y, 15, y), fill=c("brown.shadow"), width=2)
                draw.line((0, y + 2, 15, y + 2), fill=c("brown.light"))
        elif style == "crop":
            mature = recipe["mature"]
            for x, top in [(3, 5), (7, 2), (12, 4)]:
                if not mature:
                    top += 5
                stem = "gold.shadow" if mature else "green.shadow"
                leaf = "gold.light" if mature else "green.light"
                draw.line((x, 15, x, top), fill=c(stem))
                for y in range(top + 2, 14, 3):
                    draw.line((x, y + 1, x - 2, y - 1), fill=c(leaf))
                    draw.line((x, y, x + 2, y - 2), fill=c(leaf))
                if mature:
                    draw.rectangle((x - 1, top, x + 1, top + 3), fill=c("gold.mid"))
                    draw.line((x, top, x, top + 2), fill=c("gold.high"))
        elif style == "machine_side":
            draw.rectangle((0, 0, 15, 15), fill=c("metal.shadow"))
            draw.rectangle((1, 1, 14, 14), fill=c("metal.mid"))
            draw.line((2, 2, 13, 2), fill=c("metal.high"))
            draw.rectangle((3, 5, 12, 11), fill=c("metal.light"))
            for x, y in [(1, 1), (14, 1), (1, 14), (14, 14)]:
                draw.point((x, y), fill=c("neutral.cream"))
        elif style == "miner_top":
            draw.ellipse((3, 3, 12, 12), fill=c("metal.shadow"))
            draw.rectangle((6, 4, 9, 11), fill=c("gold.mid"))
            draw.rectangle((4, 6, 11, 9), fill=c("gold.light"))
            draw.rectangle((6, 6, 9, 9), fill=c("metal.light"))
        elif style == "miner_front":
            draw.rectangle((2, 3, 13, 12), fill=c("neutral.ink"))
            for y, half in [(4, 4), (6, 3), (8, 2), (10, 1)]:
                draw.line((7 - half, y, 8 + half, y), fill=c("metal.high"))
                draw.line((8 - half, y + 1, 8 + half, y + 1), fill=c("metal.mid"))
            draw.rectangle((2, 2, 4, 3), fill=c("gold.mid"))
        elif style == "belt_side":
            draw.rectangle((0, 3, 15, 11), fill=c("neutral.ink"))
            for x in [1, 6, 11]:
                draw.ellipse((x, 5, x + 3, 9), fill=c("metal.light"))
                draw.point((x + 1, 7), fill=c("metal.shadow"))
            draw.line((0, 3, 15, 3), fill=c("gold.mid"))
        elif style == "belt_top":
            draw.rectangle((0, 2, 15, 13), fill=c("neutral.ink"))
            for x in [1, 6, 11]:
                draw.line((x, 3, x, 12), fill=c("metal.shadow"))
            # Cargo motion conveys the configured output direction. The
            # static texture remains valid when the machine is rotated.
            for x in [3, 8, 13]:
                draw.line((x, 4, x, 11), fill=c("metal.mid"))
            draw.line((0, 1, 15, 1), fill=c("gold.mid"))
            draw.line((0, 14, 15, 14), fill=c("gold.shadow"))
        elif style == "powered_furnace_front":
            draw.rounded_rectangle((2, 3, 13, 11), radius=1, fill=c("neutral.ink"))
            draw.rectangle((4, 5, 11, 9), fill=c("red.shadow"))
            draw.line((4, 8, 6, 6, 8, 8, 10, 6), fill=c("gold.light"), width=2)
            draw.rectangle((3, 12, 5, 13), fill=c("blue.high"))
            draw.line((8, 12, 12, 12), fill=c("metal.shadow"))
        elif style == "factory_storage_front":
            draw.rectangle((2, 3, 13, 13), fill=c("brown.shadow"))
            for y in [4, 9]:
                draw.rectangle((3, y, 12, y + 3), fill=c("brown.light"))
                draw.line((6, y + 1, 9, y + 1), fill=c("metal.high"))
            draw.rectangle((11, 1, 13, 2), fill=c("green.light"))
        elif style == "solar_top":
            draw.rectangle((2, 2, 13, 13), fill=c("blue.shadow"))
            for y in [3, 7, 11]:
                for x in [3, 7, 11]:
                    draw.rectangle((x, y, x + 1, y + 1), fill=c("blue.mid"))
                    draw.point((x, y), fill=c("blue.light"))
            draw.line((3, 2, 8, 2), fill=c("blue.high"))
        elif style == "lamp":
            ramp = recipe["ramp"]
            draw.rectangle((0, 0, 15, 15), fill=c(f"{ramp}.shadow"))
            draw.rounded_rectangle((1, 1, 14, 14), radius=2, fill=c(f"{ramp}.mid"))
            draw.rectangle((3, 3, 12, 12), fill=c(f"{ramp}.light"))
            draw.rectangle((4, 4, 11, 11), fill=c(f"{ramp}.high"))
            draw.line((4, 4, 11, 4), fill=c("neutral.cream"))
            draw.line((4, 5, 4, 10), fill=c("neutral.cream"))
            for x, y in [(1, 1), (13, 1), (1, 13), (13, 13)]:
                draw.point((x, y), fill=c("metal.light"))
        elif style == "torch_glow":
            draw.rectangle((0, 0, 15, 15), fill=c("gold.shadow"))
            draw.rounded_rectangle((1, 1, 14, 14), radius=3, fill=c("gold.mid"))
            draw.polygon(
                [(3, 12), (3, 7), (5, 8), (6, 3), (8, 5), (11, 2), (12, 11), (10, 13), (5, 13)],
                fill=c("gold.light"),
            )
            draw.polygon(
                [(6, 11), (6, 8), (8, 9), (9, 5), (10, 10), (9, 12)], fill=c("neutral.cream")
            )
        else:
            raise ValueError(f"unknown overlay style: {style}")

    def face_layers(self, block: dict) -> list[str]:
        side = block["side"]
        return [
            side,
            side,
            block.get("bottom", side),
            block.get("top", side),
            block.get("front", side),
            side,
        ]

    def cube_icon(self, block_id: int) -> Image.Image:
        faces = self.face_layers(self.blocks[block_id])
        result = Image.new("RGBA", (ICON_TILE, ICON_TILE), TRANSPARENT)
        # Equal projected axes: horizontal edges are 13 px across and 7.5 px
        # down (30 degrees), while vertical edges are 15 px long. The three
        # parallelograms share exact vertices; no outline obscures the faces.
        # Texture rows follow the top edge downward on both side faces.
        for name, origin, a, b, brightness in [
            (faces[3], (16, 1), (13, 7.5), (-13, 7.5), 1.0),
            (faces[0], (3, 8.5), (13, 7.5), (0, 15), 0.80),
            (faces[4], (16, 16), (13, -7.5), (0, 15), 0.60),
        ]:
            texture = self.layer(name)
            det = a[0] * b[1] - a[1] * b[0]
            shade = {}
            for y in range(ICON_TILE):
                for x in range(ICON_TILE):
                    px, py = x + 0.5 - origin[0], y + 0.5 - origin[1]
                    u = (px * b[1] - py * b[0]) / det
                    v = (a[0] * py - a[1] * px) / det
                    if -1e-9 <= u <= 1 + 1e-9 and -1e-9 <= v <= 1 + 1e-9:
                        tx = max(0, min(TILE - 1, int(u * TILE)))
                        ty = max(0, min(TILE - 1, int(v * TILE)))
                        pixel = texture.getpixel((tx, ty))
                        if pixel[3]:
                            if pixel not in shade:
                                shade[pixel] = self.nearest(
                                    tuple(round(p * brightness) for p in pixel[:3])
                                )
                            result.putpixel((x, y), shade[pixel])
        return result

    def authored_icon(self, item: dict) -> Image.Image:
        image = Image.new("RGBA", (TILE, TILE), TRANSPARENT)
        draw = ImageDraw.Draw(image)
        c = self.color
        style = item["style"]
        if style in {"stick", "pickaxe", "axe", "shovel"}:
            draw.line((4, 13, 11, 4), fill=c("brown.shadow"), width=4)
            draw.line((4, 12, 11, 3), fill=c("brown.light"), width=2)
            draw.line((4, 11, 9, 5), fill=c("brown.high"))
            if style != "stick":
                ramp = {"wood": "brown", "stone": "stone", "iron": "metal"}[item["tier"]]
                if style == "pickaxe":
                    shape = [
                        (3, 3),
                        (5, 1),
                        (10, 2),
                        (13, 4),
                        (14, 7),
                        (12, 8),
                        (11, 5),
                        (7, 4),
                        (4, 6),
                        (2, 5),
                    ]
                elif style == "axe":
                    shape = [(7, 1), (11, 2), (14, 5), (13, 8), (10, 9), (7, 6)]
                else:
                    shape = [(10, 1), (14, 3), (14, 6), (11, 9), (7, 6), (7, 3)]
                draw.polygon(shape, fill=c(f"{ramp}.light"), outline=c("neutral.ink"))
                if style == "pickaxe":
                    draw.line((5, 2, 9, 3, 12, 5), fill=c(f"{ramp}.high"))
                    draw.point((12, 6), fill=c(f"{ramp}.shadow"))
                elif style == "axe":
                    draw.line((9, 3, 12, 5, 12, 7), fill=c(f"{ramp}.high"))
                    draw.line((8, 3, 8, 5), fill=c(f"{ramp}.shadow"))
                else:
                    draw.line((10, 3, 12, 4, 10, 6), fill=c(f"{ramp}.high"))
        elif style == "ore_chunk":
            draw.polygon(
                [(2, 6), (5, 2), (10, 1), (14, 6), (13, 11), (9, 14), (3, 12), (1, 9)],
                fill=c("stone.shadow"),
            )
            draw.polygon(
                [(3, 6), (6, 3), (10, 3), (12, 6), (9, 11), (4, 10)], fill=c("stone.light")
            )
            for x, y in [(5, 4), (10, 7), (5, 9)]:
                draw.rectangle((x, y, x + 2, y + 2), fill=c("red.shadow"))
                draw.line((x, y, x + 1, y), fill=c("gold.mid"))
                draw.point((x + 1, y + 1), fill=c("gold.shadow"))
        elif style == "coal":
            draw.polygon(
                [(2, 6), (5, 2), (10, 1), (14, 6), (13, 11), (9, 14), (3, 12), (1, 9)],
                fill=c("neutral.ink"),
            )
            draw.polygon([(3, 6), (6, 3), (10, 3), (8, 7)], fill=c("neutral.dim"))
            draw.polygon([(9, 8), (12, 6), (12, 10), (9, 12)], fill=c("metal.shadow"))
            draw.line((5, 4, 8, 3), fill=c("metal.mid"))
        elif style == "ingot":
            draw.polygon(
                [(2, 6), (9, 2), (14, 5), (14, 10), (7, 14), (1, 10)], fill=c("metal.shadow")
            )
            draw.polygon([(2, 6), (9, 3), (13, 5), (7, 9)], fill=c("metal.high"))
            draw.polygon([(2, 7), (7, 10), (7, 12), (2, 10)], fill=c("metal.mid"))
            draw.polygon([(8, 10), (13, 6), (13, 9), (8, 12)], fill=c("metal.light"))
            draw.line((4, 6, 9, 4), fill=c("neutral.cream"))
        elif style == "seeds":
            for x, y in [(3, 5), (8, 3), (11, 8), (5, 11)]:
                draw.ellipse((x, y, x + 3, y + 3), fill=c("green.shadow"))
                draw.line((x + 1, y + 1, x + 2, y + 1), fill=c("green.high"))
                draw.point((x + 1, y + 2), fill=c("gold.high"))
        elif style == "wheat":
            for x, top in [(4, 4), (8, 1), (12, 3)]:
                draw.line((7, 14, x, top + 2), fill=c("gold.shadow"))
                for y in range(top, top + 7, 2):
                    draw.line((x, y + 1, x - 2, y), fill=c("gold.light"), width=2)
                    draw.point((x + 1, y), fill=c("gold.high"))
            draw.line((5, 11, 9, 11), fill=c("red.shadow"), width=2)
        elif style == "bread":
            draw.rounded_rectangle((1, 4, 14, 12), radius=4, fill=c("brown.shadow"))
            draw.rounded_rectangle((2, 3, 14, 10), radius=3, fill=c("gold.mid"))
            draw.line((4, 4, 12, 4), fill=c("gold.high"))
            for x in [5, 9]:
                draw.line((x, 5, x - 1, 8), fill=c("brown.high"), width=2)
        elif style == "toast":
            draw.rounded_rectangle((2, 1, 13, 8), radius=3, fill=c("brown.shadow"))
            draw.rectangle((3, 6, 12, 14), fill=c("brown.shadow"))
            draw.rounded_rectangle((3, 2, 12, 7), radius=2, fill=c("gold.mid"))
            draw.rectangle((4, 6, 11, 13), fill=c("gold.mid"))
            draw.rounded_rectangle((5, 4, 10, 11), radius=1, fill=c("brown.high"))
            draw.rectangle((7, 6, 10, 8), fill=c("gold.high"))
            draw.line((7, 6, 9, 6), fill=c("neutral.cream"))
        elif style == "torch":
            draw.line((6, 14, 9, 6), fill=c("brown.shadow"), width=4)
            draw.line((6, 13, 9, 6), fill=c("brown.high"), width=2)
            draw.polygon(
                [(5, 6), (5, 3), (7, 4), (8, 0), (10, 3), (12, 2), (12, 6), (10, 9), (7, 8)],
                fill=c("gold.shadow"),
            )
            draw.polygon([(6, 6), (8, 2), (9, 5), (11, 3), (11, 6), (9, 8)], fill=c("gold.light"))
            draw.line((8, 6, 9, 4, 9, 6), fill=c("neutral.cream"), width=2)
        else:
            raise ValueError(f"unknown item style: {style}")
        return self.quantize(image)

    def build(self) -> tuple[dict[str, bytes], Image.Image, Image.Image]:
        atlas = Image.new("RGBA", ATLAS_SIZE, TRANSPARENT)
        icons = Image.new("RGBA", ICONS_SIZE, TRANSPARENT)
        block_metadata, item_metadata, lod = [], [], []
        for block in self.blocks:
            textures, faces = [], []
            for face, name in enumerate(self.face_layers(block)):
                index = block["id"] * 6 + face
                rect = tile_rect(index)
                texture = self.layer(name)
                atlas.paste(texture, tuple(rect[:2]))
                textures.append(texture)
                faces.append(
                    {
                        "face": FACE_ORDER[face],
                        "index": index,
                        "rect": rect,
                        "tiling": self.layers[name].get("tiling", "xy"),
                    }
                )
            block_metadata.append({"id": block["id"], "name": block["name"], "faces": faces})
            lod.append(
                {
                    "id": block["id"],
                    "top": mean_color([textures[3]]),
                    "side": mean_color([textures[i] for i in [0, 1, 4, 5]]),
                    "bottom": mean_color([textures[2]]),
                }
            )
        for item in self.items:
            if "style" in item:
                icon = self.authored_icon(item).resize(
                    (ICON_TILE, ICON_TILE), Image.Resampling.NEAREST
                )
            else:
                icon = self.cube_icon(item["block"])
            rect = tile_rect(item["id"], ICON_TILE)
            icons.paste(icon, tuple(rect[:2]))
            item_metadata.append(
                {
                    "id": item["id"],
                    "name": item["name"],
                    "placeable_block": item.get("block"),
                    "rect": rect,
                }
            )
        outputs = {
            "atlas.png": png_bytes(atlas),
            "icons.png": png_bytes(icons),
            "atlas.json": json_bytes(
                {
                    "tile_size": TILE,
                    "size": ATLAS_SIZE,
                    "face_order": FACE_ORDER,
                    "blocks": block_metadata,
                }
            ),
            "icons.json": json_bytes(
                {"tile_size": ICON_TILE, "size": ICONS_SIZE, "items": item_metadata}
            ),
            "lod_colors.json": json_bytes(lod),
        }
        return outputs, atlas, icons


def mean_color(images: list[Image.Image]) -> list[int]:
    pixels = [pixel for image in images for pixel in image.get_flattened_data() if pixel[3]]
    if not pixels:
        return [0, 0, 0]

    def linear(value: int) -> float:
        encoded = value / 255
        return encoded / 12.92 if encoded <= 0.04045 else ((encoded + 0.055) / 1.055) ** 2.4

    def srgb(value: float) -> int:
        encoded = value * 12.92 if value <= 0.0031308 else 1.055 * value ** (1 / 2.4) - 0.055
        return round(encoded * 255)

    return [
        srgb(sum(linear(pixel[channel]) for pixel in pixels) / len(pixels)) for channel in range(3)
    ]


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=False, compress_level=9)
    return buffer.getvalue()


def json_bytes(value) -> bytes:
    return (json.dumps(value, indent=2, ensure_ascii=True) + "\n").encode("utf-8")


def preview(pipeline: Pipeline, atlas: Image.Image, icons: Image.Image, path: Path) -> None:
    """An optional nearest-neighbor contact sheet, outside shipped output."""
    item_y = 40 + ((len(pipeline.blocks) + 9) // 10) * 124
    height = item_y + ((len(pipeline.items) + 13) // 14) * 96
    sheet = Image.new("RGB", (1120, height), pipeline.color("neutral.cream")[:3])
    draw = ImageDraw.Draw(sheet)
    draw.text((20, 10), "TSUMIKI / BLOCK FACES + INVENTORY", fill=pipeline.color("brown.shadow"))
    for block in pipeline.blocks:
        x, y = 20 + block["id"] % 10 * 110, 40 + block["id"] // 10 * 124
        face = 4 if block["id"] in {9, 10, 14} else 3
        sx, sy, _, _ = tile_rect(block["id"] * 6 + face)
        tile = atlas.crop((sx, sy, sx + TILE, sy + TILE)).resize((64, 64), Image.Resampling.NEAREST)
        sheet.paste(tile, (x, y), tile)
        draw.text((x, y + 70), block["name"], fill=pipeline.color("brown.shadow"))
    for item in pipeline.items:
        x, y = 20 + (item["id"] - 1) % 14 * 76, item_y + (item["id"] - 1) // 14 * 96
        sx, sy, _, _ = tile_rect(item["id"], ICON_TILE)
        tile = icons.crop((sx, sy, sx + ICON_TILE, sy + ICON_TILE)).resize(
            (64, 64), Image.Resampling.NEAREST
        )
        sheet.paste(tile, (x, y), tile)
        draw.text((x + 26, y + 68), str(item["id"]), fill=pipeline.color("brown.shadow"))
    path.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = Path(__file__).resolve().parent
    parser.add_argument("--source", type=Path, default=source)
    parser.add_argument("--output", type=Path, default=source.parent / "assets")
    parser.add_argument("--check", action="store_true", help="fail if committed output differs")
    parser.add_argument("--preview", type=Path, help="write a nearest-neighbor contact sheet")
    args = parser.parse_args()
    pipeline = Pipeline(args.source)
    outputs, atlas, icons = pipeline.build()
    if args.check:
        changed = [
            name
            for name, data in outputs.items()
            if not (args.output / name).is_file() or (args.output / name).read_bytes() != data
        ]
        if changed:
            parser.exit(1, "Generated assets are stale: " + ", ".join(changed) + "\n")
        print("All five generated assets match their sources.")
    else:
        args.output.mkdir(parents=True, exist_ok=True)
        for name, data in outputs.items():
            (args.output / name).write_bytes(data)
        print(
            f"Generated {len(pipeline.blocks) * 6} block faces and "
            f"{len(pipeline.items)} item icons in {args.output}"
        )
    if args.preview:
        preview(pipeline, atlas, icons, args.preview)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
