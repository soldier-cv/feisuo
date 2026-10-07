# ==============================================================
#  图标生成器（**可重复运行**，与 migrations/ 下的一次性脚本不同）
# ==============================================================
# 改设计后重跑本脚本即可重新生成全部尺寸的 PNG，产物直接覆盖
# `desktop/src-tauri/icons/` 与 `mobile/android/app/src/main/res/`。
# 它是"源"，不是"迁移"—— 所以留在 scripts/ 根目录而不是 migrations/。
#
# 依赖 Pillow（`pip install pillow`）。它**不参与**任何 CI 检查：
# 图标是二进制产物，不适合用文本规则校验。
import math
from PIL import Image, ImageDraw, ImageFilter

def create_master_app_icon():
    """
    生成 1024x1024 超高精度应用主图标 (Windows 任务栏、窗口与启动图标)
    Fluent 风格立体 Squircle 底座 + 极速折纸未来飞梭
    """
    size = 1024
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # 1. 外部环境光晕 (Ambient Emerald Glow)
    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    g_draw = ImageDraw.Draw(glow)
    margin = 84
    corner = 210
    g_draw.rounded_rectangle(
        [margin - 16, margin - 16, size - margin + 16, size - margin + 16],
        radius=corner + 12,
        fill=(0, 220, 130, 70)
    )
    glow = glow.filter(ImageFilter.GaussianBlur(22))
    img.alpha_composite(glow)

    # 2. 科技底座 (Tech Squircle Base)
    base = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    b_draw = ImageDraw.Draw(base)
    b_draw.rounded_rectangle(
        [margin, margin, size - margin, size - margin],
        radius=corner,
        fill=(11, 19, 34, 255), # slate-950/deep navy
        outline=(0, 220, 130, 180),
        width=12
    )

    # 底座内部渐变微光
    inner = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    i_draw = ImageDraw.Draw(inner)
    i_draw.ellipse(
        [size * 0.25, size * 0.2, size * 0.9, size * 0.85],
        fill=(14, 165, 233, 40)
    )
    inner = inner.filter(ImageFilter.GaussianBlur(55))
    base.alpha_composite(inner)
    img.alpha_composite(base)

    # 3. 极速折纸未来飞梭 (Paper Dart Plane)
    plane = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    p_draw = ImageDraw.Draw(plane)

    # 几何顶点定义 (向右上角 45 度极速破空)
    nose = (size * 0.77, size * 0.23)         # 机头尖端
    tail_c = (size * 0.29, size * 0.71)       # 机身中轴尾端
    tail_l = (size * 0.20, size * 0.58)       # 左机翼尾尖
    tail_r = (size * 0.42, size * 0.80)       # 右机翼尾尖
    fold_l = (size * 0.41, size * 0.52)       # 左侧立体折面顶点
    fold_r = (size * 0.48, size * 0.59)       # 右侧立体折面顶点

    # 飞梭环境阴影 (Drop Shadow)
    shadow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    s_draw = ImageDraw.Draw(shadow)
    sx, sy = 24, 32
    s_poly = [
        (nose[0] + sx, nose[1] + sy),
        (tail_l[0] + sx, tail_l[1] + sy),
        (tail_c[0] + sx, tail_c[1] + sy),
        (tail_r[0] + sx, tail_r[1] + sy)
    ]
    s_draw.polygon(s_poly, fill=(0, 0, 0, 160))
    shadow = shadow.filter(ImageFilter.GaussianBlur(26))
    img.alpha_composite(shadow)

    # 绘制飞梭各个折叠面
    # A. 左外侧折面 (背光暗翡翠)
    p_draw.polygon([nose, tail_l, fold_l], fill=(5, 120, 85, 255))
    # B. 右外侧折面 (阴影深翠)
    p_draw.polygon([nose, fold_r, tail_r], fill=(4, 90, 65, 255))
    # C. 左主翼面 (科技纯白与极光浅青)
    p_draw.polygon([nose, fold_l, tail_c], fill=(245, 255, 250, 255))
    # D. 右主翼面 (高饱和极速翠绿 #00dc82)
    p_draw.polygon([nose, tail_c, fold_r], fill=(0, 220, 130, 255))

    # E. 高光锐利骨骼线 (Crisp Edges)
    p_draw.line([nose, tail_c], fill=(255, 255, 255, 255), width=8)  # 纯白主背脊
    p_draw.line([nose, tail_l], fill=(255, 255, 255, 220), width=5)
    p_draw.line([nose, tail_r], fill=(52, 211, 153, 240), width=5)
    p_draw.line([tail_c, fold_r], fill=(0, 180, 100, 220), width=4)
    p_draw.line([tail_c, fold_l], fill=(200, 255, 230, 220), width=4)

    img.alpha_composite(plane)
    return img

def create_tray_icon():
    """
    生成高对比度系统托盘图标 (Tray Icon)
    纯粹饱满的立体飞梭造型，带有 1.5px 暗色墨蓝锐利描边与荧光翠绿主翼
    彻底杜绝在浅色任务栏与白色悬停框中的隐形和白方块问题！
    """
    # 以 128x128 绘制，包含完整细节与描边，然后下采样至 32x32 与 16x16
    size = 128
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # 45 度飞梭在 128 空间中的居中坐标 (留 16px padding)
    nose = (108, 20)
    tail_c = (42, 86)
    tail_l = (22, 60)
    tail_r = (68, 106)
    fold_l = (58, 54)
    fold_r = (70, 72)

    # 1. 墨蓝深色外边缘描边 (在纯白底座与高亮 hover 框下提供清晰的边界剪影)
    stroke_layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    st_draw = ImageDraw.Draw(stroke_layer)
    radius = 5
    for dx in range(-radius, radius + 1):
        for dy in range(-radius, radius + 1):
            if dx*dx + dy*dy <= radius*radius:
                poly = [
                    (nose[0] + dx, nose[1] + dy),
                    (tail_l[0] + dx, tail_l[1] + dy),
                    (tail_c[0] + dx, tail_c[1] + dy),
                    (tail_r[0] + dx, tail_r[1] + dy),
                ]
                st_draw.polygon(poly, fill=(8, 15, 28, 240)) # 近乎不透明的深邃描边
    img.alpha_composite(stroke_layer)

    # 2. 机身实体填充
    plane = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    p_draw = ImageDraw.Draw(plane)

    # 左外折面 (深翡翠)
    p_draw.polygon([nose, tail_l, fold_l], fill=(6, 120, 85, 255))
    # 右外折面 (暗翡翠)
    p_draw.polygon([nose, fold_r, tail_r], fill=(4, 90, 65, 255))
    # 左主翼 (高对比度纯白)
    p_draw.polygon([nose, fold_l, tail_c], fill=(255, 255, 255, 255))
    # 右主翼 (高饱和度荧光翠绿 #00dc82)
    p_draw.polygon([nose, tail_c, fold_r], fill=(0, 220, 130, 255))

    # 核心机脊中缝
    p_draw.line([nose, tail_c], fill=(6, 95, 70, 255), width=3)

    img.alpha_composite(plane)

    # 下采样生成多尺寸
    tray_32 = img.resize((32, 32), Image.Resampling.LANCZOS)
    tray_16 = img.resize((16, 16), Image.Resampling.LANCZOS)
    tray_24 = img.resize((24, 24), Image.Resampling.LANCZOS)
    return tray_32, tray_24, tray_16

if __name__ == "__main__":
    print("1. Generating master 1024x1024 application icon...")
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
    # 包含了 Windows 任务栏和资源管理器所需的所有尺寸
    ico_frames = [resized[s] for s in [256, 128, 64, 48, 32, 24, 16]]
    ico_frames[0].save(
        "desktop/src-tauri/icons/icon.ico",
        format="ICO",
        sizes=[(s, s) for s in [256, 128, 64, 48, 32, 24, 16]],
        append_images=ico_frames[1:]
    )
    print("Saved desktop/src-tauri/icons/icon.ico successfully.")

    # 2. 生成专业托盘图标
    tray_32, tray_24, tray_16 = create_tray_icon()
    tray_32.save("desktop/src-tauri/icons/tray.png")
    print("Saved desktop/src-tauri/icons/tray.png.")

    # 保存包含 32, 24, 16 的专业 tray.ico
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
    
    svg_content = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="100%" height="100%">
  <defs>
    <filter id="emerald-glow" x="-20%" y="-20%" width="140%" height="140%">
      <feGaussianBlur stdDeviation="16" result="blur" />
      <feComposite in="SourceGraphic" in2="blur" operator="over" />
    </filter>
    <filter id="plane-shadow" x="-30%" y="-30%" width="160%" height="160%">
      <feDropShadow dx="12" dy="16" stdDeviation="14" flood-color="#000000" flood-opacity="0.65" />
    </filter>
    <radialGradient id="inner-ambient" cx="65%" cy="35%" r="60%">
      <stop offset="0%" stop-color="#0ea5e9" stop-opacity="0.35" />
      <stop offset="60%" stop-color="#10b981" stop-opacity="0.15" />
      <stop offset="100%" stop-color="#0b1322" stop-opacity="0" />
    </radialGradient>
    <linearGradient id="squircle-stroke" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#00dc82" stop-opacity="0.85" />
      <stop offset="100%" stop-color="#047857" stop-opacity="0.4" />
    </linearGradient>
    <linearGradient id="wing-left-grad" x1="100%" y1="0%" x2="0%" y2="100%">
      <stop offset="0%" stop-color="#ffffff" />
      <stop offset="100%" stop-color="#ecfdf5" />
    </linearGradient>
    <linearGradient id="wing-right-grad" x1="100%" y1="0%" x2="0%" y2="100%">
      <stop offset="0%" stop-color="#00dc82" />
      <stop offset="100%" stop-color="#059669" />
    </linearGradient>
  </defs>
  <rect x="42" y="42" width="428" height="428" rx="105" fill="none" stroke="#00dc82" stroke-width="8" stroke-opacity="0.35" filter="url(#emerald-glow)" />
  <rect x="42" y="42" width="428" height="428" rx="105" fill="#0b1322" stroke="url(#squircle-stroke)" stroke-width="6" />
  <rect x="42" y="42" width="428" height="428" rx="105" fill="url(#inner-ambient)" />
  <g filter="url(#plane-shadow)">
    <polygon points="394,117.5 102.5,297 210,266" fill="#057855" />
    <polygon points="394,117.5 245.5,302 215,409.5" fill="#045a41" />
    <polygon points="394,117.5 210,266 148.5,363.5" fill="url(#wing-left-grad)" />
    <polygon points="394,117.5 148.5,363.5 245.5,302" fill="url(#wing-right-grad)" />
    <line x1="394" y1="117.5" x2="148.5" y2="363.5" stroke="#ffffff" stroke-width="4" stroke-linecap="round" />
    <line x1="394" y1="117.5" x2="102.5" y2="297" stroke="#ffffff" stroke-width="2.5" stroke-opacity="0.85" stroke-linecap="round" />
    <line x1="394" y1="117.5" x2="215" y2="409.5" stroke="#34d399" stroke-width="2.5" stroke-opacity="0.9" stroke-linecap="round" />
    <line x1="148.5" y1="363.5" x2="245.5" y2="302" stroke="#00dc82" stroke-width="2" stroke-opacity="0.8" />
    <line x1="148.5" y1="363.5" x2="210" y2="266" stroke="#d1fae5" stroke-width="2" stroke-opacity="0.8" />
  </g>
</svg>"""
    for svg_dest in ["ui/public/favicon.svg", "ui/dist/favicon.svg", "desktop/src-tauri/icons/icon.svg"]:
        with open(svg_dest, "w", encoding="utf-8") as f:
            f.write(svg_content)
    print("Synchronized UI favicons and SVGs successfully.")
