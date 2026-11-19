"""生成应用图标：深色圆角底 + 绿色波形（与 ui/src/app.css 的 --accent 一致）。

一次性脚本，产物入库（tauri-build 在编译期就要读 icons/icon.ico）。
重新生成：python scripts/gen-icons.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

ACCENT = (95, 227, 161, 255)      # --accent #5fe3a1
BG_TOP = (26, 30, 38, 255)        # 略亮的深色，圆角外是透明
BG_BOTTOM = (15, 17, 21, 255)     # --bg #0f1115
OUT = Path(__file__).resolve().parent.parent / "apps" / "app" / "icons"


def draw(size: int) -> Image.Image:
    scale = 4
    n = size * scale
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # 圆角方块，纵向渐变（逐行画，简单可靠）
    radius = int(n * 0.22)
    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, n - 1, n - 1], radius=radius, fill=255)
    grad = Image.new("RGBA", (n, n))
    gd = ImageDraw.Draw(grad)
    for y in range(n):
        t = y / max(n - 1, 1)
        gd.line(
            [(0, y), (n, y)],
            fill=tuple(int(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOTTOM)),
        )
    img.paste(grad, (0, 0), mask)

    # 波形：7 根竖条，高度按正弦包络，中间最高。
    # 条宽与间隙按比例给（别用「总宽反推 gap」，那样算错一次就糊成一团）。
    bars = 7
    heights = [0.18, 0.38, 0.62, 0.86, 0.62, 0.38, 0.18]
    width = n * 0.07
    gap = n * 0.032
    total = bars * width + (bars - 1) * gap
    x = (n - total) / 2
    cy = n / 2
    for h in heights:
        bar_h = n * h
        d.rounded_rectangle(
            [x, cy - bar_h / 2, x + width, cy + bar_h / 2],
            radius=width / 2,
            fill=ACCENT,
        )
        x += width + gap

    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, size in [
        ("32x32.png", 32),
        ("128x128.png", 128),
        ("128x128@2x.png", 256),
        ("icon.png", 512),
    ]:
        draw(size).save(OUT / name)
    # .ico 要多尺寸，Windows 会按 DPI 挑
    draw(256).save(
        OUT / "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print(f"图标已写入 {OUT}")


if __name__ == "__main__":
    main()
