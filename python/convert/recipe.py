"""The `quickdraw-issue-19` recipe: a raw Quick, Draw! drawing to the 28 x 28 model input
(docs/engines/convert.md, "Recipe").

1. `simplify`: the dataset README's preprocessing of its `simplified` files: align the drawing to
   0, scale it uniformly so its longer side is 255, resample every stroke at 1 px, then
   Ramer-Douglas-Peucker at epsilon 2.0, coordinates rounded to integers.
2. `rasterise`: `vector_to_raster` from the dataset's GitHub issue #19: cairo, ANTIALIAS_BEST,
   round caps and joins, line width 16 and padding 16 in the 256 space, the drawing centred on its
   bounding box, everything scaled by 28 / (256 + 2 * 16 + 16) and drawn straight onto the 28 x 28
   surface. Values are ink 1.0 on 0.0 (`/ 255`).
"""

import math

import numpy as np

SIDE = 28
ORIGINAL_SIDE = 256.0
LINE_DIAMETER = 16.0
PADDING = 16.0
SCALE_TO = 255.0
RESAMPLE_SPACING = 1.0
RDP_EPSILON = 2.0


def simplify(drawing):
    """Raw strokes (`[xs, ys, ts]` each) to simplified strokes (`[xs, ys]` of ints)."""
    strokes = [(list(map(float, stroke[0])), list(map(float, stroke[1]))) for stroke in drawing]
    strokes = [(xs, ys) for xs, ys in strokes if xs]
    if not strokes:
        return []
    min_x = min(min(xs) for xs, _ in strokes)
    min_y = min(min(ys) for _, ys in strokes)
    extent = max(
        max(max(xs) for xs, _ in strokes) - min_x,
        max(max(ys) for _, ys in strokes) - min_y,
    )
    scale = SCALE_TO / extent if extent > 0 else 1.0
    simplified = []
    for xs, ys in strokes:
        points = [((x - min_x) * scale, (y - min_y) * scale) for x, y in zip(xs, ys)]
        points = rdp(resample(points, RESAMPLE_SPACING), RDP_EPSILON)
        simplified.append(([round_half_up(x) for x, _ in points], [round_half_up(y) for _, y in points]))
    return simplified


def round_half_up(value):
    return int(math.floor(value + 0.5))


def resample(points, spacing):
    """Points every `spacing` along the polyline, from its first point, and its last point."""
    if len(points) < 2:
        return list(points)
    out = [points[0]]
    carried = 0.0  # length walked since the last emitted point
    for (x0, y0), (x1, y1) in zip(points, points[1:]):
        length = math.hypot(x1 - x0, y1 - y0)
        if length == 0:
            continue
        walked = spacing - carried
        while walked <= length:
            t = walked / length
            out.append((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t))
            walked += spacing
        carried = length - (walked - spacing)
    if out[-1] != points[-1]:
        out.append(points[-1])
    return out


def rdp(points, epsilon):
    """Ramer-Douglas-Peucker: the points a polyline keeps within `epsilon` of the original."""
    if len(points) < 3:
        return list(points)
    keep = [False] * len(points)
    keep[0] = keep[-1] = True
    stack = [(0, len(points) - 1)]
    while stack:
        first, last = stack.pop()
        (x0, y0), (x1, y1) = points[first], points[last]
        dx, dy = x1 - x0, y1 - y0
        norm = math.hypot(dx, dy)
        farthest, distance = first, -1.0
        for i in range(first + 1, last):
            px, py = points[i]
            if norm == 0:
                d = math.hypot(px - x0, py - y0)
            else:
                d = abs(dy * px - dx * py + x1 * y0 - y1 * x0) / norm
            if d > distance:
                farthest, distance = i, d
        if distance > epsilon:
            keep[farthest] = True
            stack.append((first, farthest))
            stack.append((farthest, last))
    return [point for point, kept in zip(points, keep) if kept]


def rasterise(simplified):
    """Simplified strokes to a (28, 28) float32 raster, ink 1.0 on 0.0, row-major."""
    import cairocffi as cairo

    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, SIDE, SIDE)
    ctx = cairo.Context(surface)
    ctx.set_antialias(cairo.ANTIALIAS_BEST)
    ctx.set_line_cap(cairo.LINE_CAP_ROUND)
    ctx.set_line_join(cairo.LINE_JOIN_ROUND)
    ctx.set_line_width(LINE_DIAMETER)
    total_padding = PADDING * 2.0 + LINE_DIAMETER
    scale = SIDE / (ORIGINAL_SIDE + total_padding)
    ctx.scale(scale, scale)
    ctx.translate(total_padding / 2.0, total_padding / 2.0)
    ctx.set_source_rgb(0, 0, 0)
    ctx.paint()
    if simplified:
        max_x = max(max(xs) for xs, _ in simplified)
        max_y = max(max(ys) for _, ys in simplified)
        offset_x = (ORIGINAL_SIDE - max_x) / 2.0
        offset_y = (ORIGINAL_SIDE - max_y) / 2.0
        ctx.set_source_rgb(1, 1, 1)
        for xs, ys in simplified:
            ctx.move_to(xs[0] + offset_x, ys[0] + offset_y)
            for x, y in zip(xs, ys):
                ctx.line_to(x + offset_x, y + offset_y)
            ctx.stroke()
    surface.flush()
    stride = surface.get_stride()
    data = np.frombuffer(bytes(surface.get_data()), dtype=np.uint8).reshape(SIDE, stride)
    # ARGB32 is native-endian; white ink makes every channel equal, so byte 0 of each pixel is it.
    pixels = data[:, : SIDE * 4 : 4]
    return (pixels.astype(np.float32) / 255.0).reshape(SIDE, SIDE)


def raster_of(drawing):
    """A raw drawing straight to its raster."""
    return rasterise(simplify(drawing))
