# ==============================================================
#  图标生成器（**可重复运行**，与 migrations/ 下的一次性脚本不同）
# ==============================================================
# 改设计后重跑本脚本即可重新生成全部尺寸的 PNG/ICO/SVG，产物直接覆盖
# `desktop/src-tauri/icons/` 与 `ui/public/`、`ui/dist/`。
#
# 依赖 Pillow（`pip install pillow`）。它**不参与**任何 CI 检查：
# 图标是二进制产物，不适合用文本规则校验。
import math
from PIL import Image, ImageDraw, ImageFilter

def create_master_app_icon():
    """
    生成 1024x1024 超高精度应用主图标 (Windows 任务栏、窗口与启动图标)
    极简现代科技 Squircle 底座 + 极简纯粹几何超音速飞梭
    设计语言：去繁就简，移除多道碎骨线与外围厚重毛糙光晕，
    完全依托大色块几何对比，兼具大图质感与 16px/32px 微尺寸极高辨识度。
    """
    size = 1024
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # 1. 科技底座 (Tech Squircle Base)
    margin = 96
    corner = 208
    base = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    b_draw = ImageDraw.Draw(base)
    b_draw.rounded_rectangle(
        [margin, margin, size - margin, size - margin],
        radius=corner,
        fill=(11, 19, 34, 255),      # 深邃太空蓝黑
        outline=(0, 220, 130, 150),  # 精致翡翠绿细边框
        width=7
    )
    img.alpha_composite(base)

    # 2. 超音速几何飞梭 (Supersonic Dart Plane)
    # 45 度极速破空姿态
    nose = (size * 0.77, size * 0.23)         # 机头尖端
    tail_c = (size * 0.29, size * 0.71)       # 机身中轴尾端
    tail_l = (size * 0.20, size * 0.58)       # 左机翼尾尖
    tail_r = (size * 0.42, size * 0.80)       # 右机翼尾尖
    fold_l = (size * 0.41, size * 0.52)       # 左侧立体折面顶点
    fold_r = (size * 0.48, size * 0.59)       # 右侧立体折面顶点

    # 柔和紧凑的悬浮微阴影 (Drop Shadow)
    shadow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    s_draw = ImageDraw.Draw(shadow)
    sx, sy = 18, 26
    s_poly = [
        (nose[0] + sx, nose[1] + sy),
        (tail_l[0] + sx, tail_l[1] + sy),
        (tail_c[0] + sx, tail_c[1] + sy),
        (tail_r[0] + sx, tail_r[1] + sy)
    ]
    s_draw.polygon(s_poly, fill=(0, 0, 0, 150))
    shadow = shadow.filter(ImageFilter.GaussianBlur(22))
    img.alpha_composite(shadow)

    # 绘制飞梭纯净立体几何色块（零杂线，靠色彩明暗自然呈现锋芒棱角）
    plane = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    p_draw = ImageDraw.Draw(plane)

    # A. 左外侧折面 (深翡翠背光折面)
    p_draw.polygon([nose, tail_l, fold_l], fill=(4, 120, 87, 255))
    # B. 右外侧折面 (暗翡翠侧翼折面)
    p_draw.polygon([nose, fold_r, tail_r], fill=(6, 82, 60, 255))
    # C. 左主翼面 (高光极白，如刀锋穿梭)
    p_draw.polygon([nose, fold_l, tail_c], fill=(255, 255, 255, 255))
    # D. 右主翼面 (飞梭标志性极速翡翠绿 #00DC82)
    p_draw.polygon([nose, tail_c, fold_r], fill=(0, 220, 130, 255))

    img.alpha_composite(plane)
    return img

def create_tray_icon():
    """
    生成高对比度极简系统托盘图标 (Tray Icon)
    饱满大体量纯几何飞梭，带有精细半透明深色微描边与纯白/极速翡翠双主翼
    在 Windows 深色任务栏与浅色任务栏/气泡框中均能锐利呈现，绝无模糊白块。
    """
    size = 128
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # 45 度飞梭在 128 空间中饱满居中 (占满约 88% 画布，确保 16x16 下清晰明了)
    nose = (114, 14)
    tail_c = (36, 92)
    tail_l = (16, 64)
    tail_r = (64, 112)
    fold_l = (52, 58)
    fold_r = (66, 72)

    # 1. 精细外轮廓描边 (保证浅色任务栏/高亮悬停背景下有清晰边界)
    stroke_layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    st_draw = ImageDraw.Draw(stroke_layer)
    radius = 3
    for dx in range(-radius, radius + 1):
        for dy in range(-radius, radius + 1):
            if dx*dx + dy*dy <= radius*radius:
                poly = [
                    (nose[0] + dx, nose[1] + dy),
                    (tail_l[0] + dx, tail_l[1] + dy),
                    (tail_c[0] + dx, tail_c[1] + dy),
                    (tail_r[0] + dx, tail_r[1] + dy),
                ]
                st_draw.polygon(poly, fill=(8, 15, 28, 220))
    img.alpha_composite(stroke_layer)

    # 2. 机身实体填充 (纯净 4 色几何立体)
    plane = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    p_draw = ImageDraw.Draw(plane)

    # 左外折面 (深翡翠)
    p_draw.polygon([nose, tail_l, fold_l], fill=(4, 120, 87, 255))
    # 右外折面 (暗翡翠)
    p_draw.polygon([nose, fold_r, tail_r], fill=(6, 82, 60, 255))
    # 左主翼 (高光纯白)
    p_draw.polygon([nose, fold_l, tail_c], fill=(255, 255, 255, 255))
    # 右主翼 (高饱和极速翠绿 #00DC82)
    p_draw.polygon([nose, tail_c, fold_r], fill=(0, 220, 130, 255))

    img.alpha_composite(plane)

    # 下采样生成多尺寸
    tray_32 = img.resize((32, 32), Image.Resampling.LANCZOS)
    tray_24 = img.resize((24, 24), Image.Resampling.LANCZOS)
    tray_16 = img.resize((16, 16), Image.Resampling.LANCZOS)
    return tray_32, tray_24, tray_16

if __name__ == "__main__":
    print("1. Generating simplified & optimized master application icon...")
    master = create_master_app_icon()

    sizes = [256, 128, 64, 48, 32, 24, 16]
    resized = {s: master.resize((s, s), Image.Resampling.LANCZOS) for s in sizes}

    # 保存各类标准 PNG
    master.resize((512, 512), Image.Resampling.LANCZOS).save("desktop/src-tauri/icons/icon.png")
    resized[256].save("desktop/src-tauri/icons/128x128@2x.png")
    resized[128].save("desktop/src-tauri/icons/128x128.png")
    resized[32].save("desktop/src-tauri/icons/32x32.png")
    print("Saved application PNG icons.")

    # 保存完整的多分辨率 Windows ICO
    ico_frames = [resized[s] for s in [256, 128, 64, 48, 32, 24, 16]]
    ico_frames[0].save(
        "desktop/src-tauri/icons/icon.ico",
        format="ICO",
        sizes=[(s, s) for s in [256, 128, 64, 48, 32, 24, 16]],
        append_images=ico_frames[1:]
    )
    print("Saved desktop/src-tauri/icons/icon.ico successfully.")

    # 2. 生成专业极简托盘图标
    tray_32, tray_24, tray_16 = create_tray_icon()
    tray_32.save("desktop/src-tauri/icons/tray.png")
    print("Saved desktop/src-tauri/icons/tray.png.")

    tray_32.save(
        "desktop/src-tauri/icons/tray.ico",
        format="ICO",
        sizes=[(32, 32), (24, 24), (16, 16)],
        append_images=[tray_24, tray_16]
    )
    print("Saved desktop/src-tauri/icons/tray.ico.")

    # 3. 同步到前端 Webview 与 Dist Favicon
    resized[128].save("ui/public/favicon.png")
    resized[32].save("ui/public/favicon.ico", format="ICO")
    resized[128].save("ui/dist/favicon.png")
    resized[32].save("ui/dist/favicon.ico", format="ICO")

    # 4. 生成极简纯粹矢量 SVG (无模糊毛糙光晕，无碎线，干净利落)
    svg_content = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="100%" height="100%">
  <defs>
    <filter id="plane-shadow" x="-20%" y="-20%" width="150%" height="150%">
      <feDropShadow dx="9" dy="13" stdDeviation="10" flood-color="#000000" flood-opacity="0.45" />
    </filter>
    <linearGradient id="squircle-stroke" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#00dc82" stop-opacity="0.8" />
      <stop offset="100%" stop-color="#00dc82" stop-opacity="0.25" />
    </linearGradient>
  </defs>
  <rect x="48" y="48" width="416" height="416" rx="104" fill="#0b1322" stroke="url(#squircle-stroke)" stroke-width="4" />
  <g filter="url(#plane-shadow)">
    <polygon points="394.2,117.8 102.4,297.0 209.9,266.2" fill="#047857" />
    <polygon points="394.2,117.8 245.8,302.1 215.0,409.6" fill="#06523c" />
    <polygon points="394.2,117.8 209.9,266.2 148.5,363.5" fill="#ffffff" />
    <polygon points="394.2,117.8 148.5,363.5 245.8,302.1" fill="#00dc82" />
  </g>
</svg>"""
    for svg_dest in ["ui/public/favicon.svg", "ui/dist/favicon.svg", "desktop/src-tauri/icons/icon.svg"]:
        with open(svg_dest, "w", encoding="utf-8") as f:
            f.write(svg_content)
    print("Synchronized UI favicons and SVGs successfully.")
