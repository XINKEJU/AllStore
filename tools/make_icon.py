# AllStore icon generator — Fluent-style gradient tile with a white
# shopping-bag / window-pane motif. Renders at 1024 px and supersamples down
# to a 6-size .ico (256/128/64/48/32/16) plus a 256 px PNG preview.
# Pure Pillow, no network, no fonts: everything is geometry, so it stays
# crisp at 16 px.

from PIL import Image, ImageDraw

S = 1024  # supersampled master size

# Fluent blue -> cyan diagonal gradient (bilinear blend of 4 corners).
TL = (10, 107, 214)   # deep Windows blue
TR = (33, 160, 240)   # mid blue
BL = (33, 160, 240)
BR = (79, 224, 255)   # light cyan

WHITE = (255, 255, 255, 255)


def gradient(size):
    small = Image.new("RGB", (2, 2))
    small.putpixel((0, 0), TL)
    small.putpixel((1, 0), TR)
    small.putpixel((0, 1), BL)
    small.putpixel((1, 1), BR)
    return small.resize(size, Image.BILINEAR)


def make_master(small=False):
    """`small=True` renders the 16/24 px variant: no window panes, because at
    that size the pane gaps collapse into noise. A solid silhouette reads."""
    # --- rounded-square gradient tile -------------------------------------
    grad = gradient((S, S)).convert("RGBA")
    tile_mask = Image.new("L", (S, S), 0)
    d = ImageDraw.Draw(tile_mask)
    radius = int(S * 0.225)  # Fluent tiles use ~1/4-ish corner radius
    d.rounded_rectangle([0, 0, S - 1, S - 1], radius=radius, fill=255)
    tile = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    tile.paste(grad, (0, 0), tile_mask)

    # --- bag + window-pane motif ------------------------------------------
    # mask: 255 = flat white (bag), 0 = gradient shows through (panes).
    motif = Image.new("L", (S, S), 0)
    d = ImageDraw.Draw(motif)

    if small:
        # bulked-up silhouette, no cut-outs
        d.arc([352, 168, 672, 450], start=180, end=360, fill=255, width=86)
        d.rounded_rectangle([236, 380, 788, 848], radius=72, fill=255)
    else:
        # handle: semicircular arc above the body
        d.arc([372, 138, 652, 418], start=180, end=360, fill=255, width=58)

        # body: rounded rectangle with a softly rounded bottom
        d.rounded_rectangle([255, 368, 769, 820], radius=54, fill=255)

        # 2x2 window panes cut out of the body (the "store front" windows).
        # Panes stay inset so the bag keeps a solid frame and a deeper base band.
        for x0, y0, x1, y1 in [
            (311, 424, 492, 560),
            (532, 424, 713, 560),
            (311, 600, 492, 736),
            (532, 600, 713, 736),
        ]:
            d.rounded_rectangle([x0, y0, x1, y1], radius=26, fill=0)

    white = Image.new("RGBA", (S, S), WHITE)
    tile.paste(white, (0, 0), motif)
    return tile


def main():
    master = make_master()
    small = make_master(small=True)

    master.resize((512, 512), Image.LANCZOS).save("assets/icon-512.png")
    master.resize((256, 256), Image.LANCZOS).save("assets/icon.png")

    sizes = [256, 128, 64, 48, 32, 24, 16]
    frames = [
        (small if n <= 24 else master).resize((n, n), Image.LANCZOS) for n in sizes
    ]
    frames[0].save(
        "assets/allstore.ico",
        format="ICO",
        append_images=frames[1:],
        sizes=[(n, n) for n in sizes],
    )
    print("wrote assets/allstore.ico + icon.png + icon-512.png")


if __name__ == "__main__":
    main()
