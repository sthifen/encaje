#!/usr/bin/env python3
"""Genera las imágenes y el video promocional de Encaje (SVG -> PNG con librsvg, MP4 con ffmpeg)."""
import math, os, subprocess, sys
from concurrent.futures import ProcessPoolExecutor

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")
FONT = "Cantarell"

NAVY, DEEP, BLUE, SKY, WHITE = "#1E3A8A", "#070D1C", "#4F8CFF", "#93C5FD", "#FFFFFF"


# ---------- utilidades ----------
def clamp(x, a=0.0, b=1.0): return max(a, min(b, x))
def ease(x): x = clamp(x); return x * x * (3 - 2 * x)
def out_back(x):
    x = clamp(x); c1, c3 = 1.70158, 2.70158
    return 1 + c3 * (x - 1) ** 3 + c1 * (x - 1) ** 2
def seg(t, a, b): return ease((t - a) / (b - a)) if b > a else float(t >= a)
def lerp(a, b, k): return a + (b - a) * k


def defs():
    return f"""<defs>
  <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0" stop-color="#0A1430"/><stop offset=".55" stop-color="{DEEP}"/><stop offset="1" stop-color="#120A2E"/>
  </linearGradient>
  <radialGradient id="glowB"><stop offset="0" stop-color="{BLUE}" stop-opacity=".55"/><stop offset="1" stop-color="{BLUE}" stop-opacity="0"/></radialGradient>
  <radialGradient id="glowP"><stop offset="0" stop-color="#8B5CF6" stop-opacity=".45"/><stop offset="1" stop-color="#8B5CF6" stop-opacity="0"/></radialGradient>
  <linearGradient id="wall" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0" stop-color="#2B4FB8"/><stop offset=".5" stop-color="#5B3FC4"/><stop offset="1" stop-color="#C2417F"/>
  </linearGradient>
  <linearGradient id="andro" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0" stop-color="#0F766E"/><stop offset="1" stop-color="#1E3A8A"/>
  </linearGradient>
  <linearGradient id="txt" x1="0" y1="0" x2="1" y2="0">
    <stop offset="0" stop-color="{WHITE}"/><stop offset="1" stop-color="{SKY}"/>
  </linearGradient>
  <linearGradient id="accent" x1="0" y1="0" x2="1" y2="0">
    <stop offset="0" stop-color="{BLUE}"/><stop offset="1" stop-color="#A78BFA"/>
  </linearGradient>
  <linearGradient id="metal" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0" stop-color="#3A4258"/><stop offset="1" stop-color="#1A1F2E"/>
  </linearGradient>
  <filter id="shadow" x="-30%" y="-30%" width="160%" height="160%">
    <feGaussianBlur in="SourceAlpha" stdDeviation="18"/><feOffset dy="18"/>
    <feComponentTransfer><feFuncA type="linear" slope=".55"/></feComponentTransfer>
    <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
  </filter>
  <filter id="soft" x="-30%" y="-30%" width="160%" height="160%">
    <feGaussianBlur in="SourceAlpha" stdDeviation="6"/><feOffset dy="6"/>
    <feComponentTransfer><feFuncA type="linear" slope=".45"/></feComponentTransfer>
    <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
  </filter>
  <filter id="blur"><feGaussianBlur stdDeviation="4"/></filter>
</defs>"""


def background(w, h, t=0.0):
    dx = 60 * math.sin(t * 0.6); dy = 40 * math.cos(t * 0.5)
    dots = []
    step = 48
    for y in range(step // 2, h, step):
        for x in range(step // 2, w, step):
            dots.append(f'<circle cx="{x}" cy="{y}" r="1.4"/>')
    return (f'<rect width="{w}" height="{h}" fill="url(#bg)"/>'
            f'<circle cx="{w*0.18+dx}" cy="{h*0.2+dy}" r="{max(w,h)*0.45}" fill="url(#glowB)"/>'
            f'<circle cx="{w*0.85-dx}" cy="{h*0.85-dy}" r="{max(w,h)*0.42}" fill="url(#glowP)"/>'
            f'<g fill="#FFFFFF" opacity=".06">{"".join(dots)}</g>')


def logo(x, y, s, op=1.0):
    """Icono de la app (viewBox 108) en (x,y) con lado s."""
    k = s / 108
    return f"""<g transform="translate({x},{y}) scale({k})" opacity="{op}">
  <rect width="108" height="108" rx="22" fill="{NAVY}"/>
  <path fill="{SKY}" d="M22,30h46a4,4 0 0 1 4,4v28a4,4 0 0 1 -4,4h-46a4,4 0 0 1 -4,-4v-28a4,4 0 0 1 4,-4z"/>
  <path fill="{NAVY}" d="M24,36h42v24h-42z"/>
  <path fill="{SKY}" d="M38,66h14v6h6v4h-26v-4h6z"/>
  <path fill="#FFFFFF" d="M66,46h20a3,3 0 0 1 3,3v28a3,3 0 0 1 -3,3h-20a3,3 0 0 1 -3,-3v-28a3,3 0 0 1 3,-3z"/>
  <path fill="{BLUE}" d="M67,50h18v24h-18z"/>
</g>"""


def text(x, y, s, size, weight=400, fill=WHITE, anchor="start", op=1.0, ls=0, family=FONT):
    s = s.replace("&", "&amp;")
    return (f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" '
            f'fill="{fill}" text-anchor="{anchor}" opacity="{op}" letter-spacing="{ls}">{s}</text>')


def pill(x, y, label, size=26, fill="#FFFFFF", bg="#FFFFFF", bgop=.08, anchor="start", op=1.0, stroke=True):
    w = len(label) * size * 0.56 + size * 1.6
    h = size * 2
    if anchor == "middle": x -= w / 2
    st = f'stroke="#FFFFFF" stroke-opacity=".18" stroke-width="1.5"' if stroke else ""
    return (f'<g opacity="{op}"><rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{h/2}" fill="{bg}" fill-opacity="{bgop}" {st}/>'
            + text(x + w / 2, y + h * 0.66, label, size, 600, fill, "middle") + "</g>"), w


def pills_row(cx, y, labels, size=26, gap=16, op=1.0):
    widths = [len(l) * size * 0.56 + size * 1.6 for l in labels]
    total = sum(widths) + gap * (len(labels) - 1)
    x = cx - total / 2; out = []
    for l, w in zip(labels, widths):
        out.append(pill(x, y, l, size, op=op)[0]); x += w + gap
    return "".join(out)


def pointer(x, y, s=1.0, fill=WHITE, op=1.0):
    return (f'<g transform="translate({x},{y}) scale({s})" opacity="{op}" filter="url(#soft)">'
            f'<path d="M0,0 L0,30 L8,23 L13,35 L18,33 L13,21 L23,21 Z" fill="{fill}" stroke="#000" stroke-width="1.6" stroke-linejoin="round"/></g>')


def window(x, y, w, h, title_color=BLUE, kind="doc"):
    tb = 26
    body = []
    if kind == "doc":
        for i in range(5):
            ww = w * (0.8 if i % 3 else 0.55) - 30
            body.append(f'<rect x="{x+16}" y="{y+tb+18+i*20}" width="{ww}" height="9" rx="4.5" fill="#CBD5E1"/>')
        body.append(f'<rect x="{x+16}" y="{y+tb+125}" width="{w-32}" height="{h-tb-140}" rx="8" fill="url(#wall)" opacity=".85"/>')
    elif kind == "chart":
        for i, v in enumerate([.4, .7, .55, .9, .65, .8, .5]):
            bh = (h - tb - 40) * v
            body.append(f'<rect x="{x+20+i*(w-40)/7}" y="{y+h-20-bh}" width="{(w-40)/7-10}" height="{bh}" rx="5" fill="{BLUE if i%2 else "#A78BFA"}"/>')
    elif kind == "code":
        cols = ["#F472B6", "#93C5FD", "#FCD34D", "#86EFAC", "#93C5FD", "#F472B6", "#CBD5E1"]
        for i, c in enumerate(cols):
            indent = (i % 3) * 18
            body.append(f'<rect x="{x+16+indent}" y="{y+tb+16+i*19}" width="{(w-60)*(0.35+0.4*((i*37)%10)/10)}" height="8" rx="4" fill="{c}" opacity=".9"/>')
    bodyfill = "#F8FAFC" if kind == "doc" else "#111827"
    return (f'<g filter="url(#soft)"><rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{bodyfill}"/>'
            f'<path d="M{x},{y+tb} v-{tb-10} a10,10 0 0 1 10,-10 h{w-20} a10,10 0 0 1 10,10 v{tb-10} z" fill="#E2E8F0" opacity="{1 if kind=="doc" else .12}"/>'
            f'<circle cx="{x+w-16}" cy="{y+13}" r="5" fill="#EF4444"/><circle cx="{x+w-32}" cy="{y+13}" r="5" fill="#F59E0B"/>'
            f'<rect x="{x+14}" y="{y+9}" width="{w*0.3}" height="8" rx="4" fill="{title_color}" opacity=".8"/>'
            + "".join(body) + "</g>")


# ---------- escena de dispositivos ----------
# Pantallas en coordenadas de un lienzo de 1920x1080; se escala con transform.
LS = (300, 250, 820, 512)      # pantalla del portátil: x, y, w, h
TS = (1270, 360, 520, 325)     # pantalla de la tablet


def devices(st):
    """st: dict con win_x (posición virtual de la ventana), mode ('monitor'|'input'),
    ptr (x,y en coords de lienzo o None), link (0..1 pulso), clip (0..1 chip volando), tablet_in, laptop_in"""
    lx, ly, lw, lh = LS; tx, ty, tw, th = TS
    li = st.get("laptop_in", 1); ti = st.get("tablet_in", 1)
    out = []
    out.append(f'<clipPath id="cl"><rect x="{lx}" y="{ly}" width="{lw}" height="{lh}" rx="6"/></clipPath>')
    out.append(f'<clipPath id="ct"><rect x="{tx}" y="{ty}" width="{tw}" height="{th}" rx="10"/></clipPath>')

    # --- portátil ---
    lap = []
    lap.append(f'<rect x="{lx-22}" y="{ly-22}" width="{lw+44}" height="{lh+44}" rx="22" fill="#0B0F1A" stroke="#2A3245" stroke-width="2"/>')
    lap.append(f'<path d="M{lx-120},{ly+lh+22} h{lw+240} l-40,34 h-{lw+160} z" fill="url(#metal)"/>')
    lap.append(f'<rect x="{lx+lw/2-80}" y="{ly+lh+24}" width="160" height="10" rx="5" fill="#0B0F1A" opacity=".6"/>')
    scr = [f'<rect x="{lx}" y="{ly}" width="{lw}" height="{lh}" fill="url(#wall)"/>',
           f'<rect x="{lx}" y="{ly}" width="{lw}" height="24" fill="#000" opacity=".75"/>',
           text(lx + 14, ly + 17, "Actividades", 13, 600, WHITE),
           text(lx + lw / 2, ly + 17, "mar 21 oct  10:24", 13, 600, WHITE, "middle"),
           f'<rect x="{lx+lw/2-170}" y="{ly+lh-58}" width="340" height="46" rx="16" fill="#000" opacity=".35"/>']
    for i, c in enumerate(["#60A5FA", "#F59E0B", "#34D399", "#F472B6", "#A78BFA"]):
        scr.append(f'<rect x="{lx+lw/2-150+i*64}" y="{ly+lh-50}" width="30" height="30" rx="8" fill="{c}"/>')
    scr.append(window(lx + 50, ly + 60, 330, 260, "#F472B6", "code"))
    if st.get("mode", "monitor") == "monitor":
        scr.append(window(lx + st["win_x"], ly + 44, 300, 270, BLUE, "doc"))
    lap.append(f'<g clip-path="url(#cl)">{"".join(scr)}</g>')
    lap.append(f'<rect x="{lx}" y="{ly}" width="{lw}" height="{lh}" fill="#FFFFFF" opacity=".04"/>')
    out.append(f'<g opacity="{li}" transform="translate({-200*(1-li)},0)" filter="url(#shadow)">{"".join(lap)}</g>')

    # --- enlace (cable / ondas) ---
    link = st.get("link", 0)
    if link > 0:
        yc = ty + th / 2; x1, x2 = lx + lw + 30, tx - 26
        d = f"M{x1},{yc} L{x2},{yc}"
        out.append(f'<circle cx="{(x1+x2)/2}" cy="{yc}" r="{46*link}" fill="url(#glowB)"/>')
        out.append(f'<path d="{d}" stroke="url(#accent)" stroke-width="5" fill="none" stroke-linecap="round" opacity="{.35*link}"/>')
        ph = st.get("phase", 0)
        out.append(f'<path d="{d}" stroke="{SKY}" stroke-width="5" fill="none" stroke-linecap="round" '
                   f'stroke-dasharray="14 34" stroke-dashoffset="{-ph*48}" opacity="{link}"/>')

    # --- tablet ---
    tab = [f'<rect x="{tx-18}" y="{ty-18}" width="{tw+36}" height="{th+36}" rx="30" fill="#0B0F1A" stroke="#2A3245" stroke-width="2"/>',
           f'<circle cx="{tx-9}" cy="{ty+th/2}" r="3" fill="#1F2937"/>']
    ts = []
    if st.get("mode", "monitor") == "monitor":
        ts.append(f'<rect x="{tx}" y="{ty}" width="{tw}" height="{th}" fill="url(#wall)"/>')
        ts.append(f'<rect x="{tx}" y="{ty}" width="{tw}" height="24" fill="#000" opacity=".75"/>')
        ts.append(window(tx + (st["win_x"] - lw - 40), ty + 44, 300, 270, BLUE, "doc"))
        ts.append(window(tx + 345, ty + 60, 160, 190, "#34D399", "chart"))
    else:
        ts.append(f'<rect x="{tx}" y="{ty}" width="{tw}" height="{th}" fill="url(#andro)"/>')
        ts.append(text(tx + 18, ty + 22, "10:24", 15, 600, WHITE))
        ts.append(text(tx + tw / 2, ty + 92, "10:24", 54, 300, WHITE, "middle"))
        ts.append(text(tx + tw / 2, ty + 120, "martes, 21 de octubre", 15, 400, WHITE, "middle", .8))
        cols = ["#F59E0B", "#34D399", "#60A5FA", "#F472B6", "#A78BFA", "#FB7185"]
        for i, c in enumerate(cols):
            cx = tx + tw / 2 - 175 + i * 70
            ts.append(f'<circle cx="{cx}" cy="{ty+200}" r="24" fill="{c}"/>')
            ts.append(f'<rect x="{cx-18}" y="{ty+232}" width="36" height="6" rx="3" fill="#FFFFFF" opacity=".5"/>')
        ts.append(f'<rect x="{tx+60}" y="{ty+th-52}" width="{tw-120}" height="34" rx="17" fill="#FFFFFF" opacity=".9"/>')
        ts.append(text(tx + 84, ty + th - 30, "Buscar", 15, 400, "#475569"))
    tab.append(f'<g clip-path="url(#ct)">{"".join(ts)}</g>')
    out.append(f'<g opacity="{ti}" transform="translate({200*(1-ti)},0)" filter="url(#shadow)">{"".join(tab)}</g>')

    # --- chip de portapapeles ---
    c = st.get("clip")
    if c is not None and 0 < c < 1.15:
        k = ease(c)
        cx = lerp(lx + 520, tx + 200, k); cy = lerp(ly + 200, ty + 150, k) - 140 * math.sin(math.pi * clamp(c))
        op = clamp(c * 6) * clamp((1.15 - c) * 6)
        out.append(f'<g transform="translate({cx},{cy})" opacity="{op}" filter="url(#soft)">'
                   f'<rect x="-92" y="-26" width="184" height="52" rx="26" fill="{WHITE}"/>'
                   f'<rect x="-74" y="-14" width="22" height="28" rx="4" fill="none" stroke="{NAVY}" stroke-width="3"/>'
                   f'<rect x="-68" y="-18" width="10" height="8" rx="2" fill="{NAVY}"/>'
                   + text(10, 8, "Copiado", 22, 700, NAVY, "middle") + "</g>")

    p = st.get("ptr")
    if p: out.append(pointer(p[0], p[1], 1.3))
    return "".join(out)


def svg(w, h, body):
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">{defs()}{body}</svg>'


def scene_group(body, x, y, s):
    return f'<g transform="translate({x},{y}) scale({s})">{body}</g>'


def vptr(vx, vy):
    """Puntero en el escritorio virtual (portátil y, a su derecha, la tablet) -> coords de lienzo."""
    lx, ly, lw, _ = LS; tx, ty, _, _ = TS
    return (lx + vx, ly + vy) if vx < lw else (tx + vx - lw - 40, ty + vy)


# ---------- imágenes fijas ----------
STILL_MON = dict(win_x=880, mode="monitor", link=1, phase=.3, ptr=(1500, 520))
STILL_IN = dict(win_x=0, mode="input", link=1, phase=.3, ptr=(1475, 600), clip=.55)


def img_hero():
    w, h = 1920, 1080
    b = background(w, h)
    b += scene_group(devices(STILL_MON), 0, 150, 0.92)
    b += logo(110, 90, 84)
    b += text(214, 150, "Encaje", 64, 800, "url(#txt)", ls=-1)
    b += text(110, 270, "Tu tablet, tu segundo monitor.", 78, 800, WHITE, ls=-1.5)
    b += text(110, 325, "Para GNOME y Wayland · por cable USB o WiFi · gratis", 32, 400, SKY)
    b += pills_row(w / 2 + 330, 960, ["Hasta 60 fps", "H.264 por GPU", "Ratón y teclado", "Portapapeles"], 26)
    return svg(w, h, b)


def img_social():
    w, h = 1280, 640
    b = background(w, h)
    b += scene_group(devices(STILL_MON), 470, 90, 0.44)
    b += logo(70, 70, 70)
    b += text(158, 122, "Encaje", 54, 800, "url(#txt)")
    b += text(70, 240, "Tu tablet como", 56, 800, WHITE, ls=-1)
    b += text(70, 305, "segundo monitor", 56, 800, "url(#accent)", ls=-1)
    b += text(70, 360, "GNOME · Wayland · Android", 26, 400, SKY)
    b += text(70, 590, "github.com/sthifen/encaje", 24, 600, WHITE, op=.75)
    return svg(w, h, b)


FEATURES = [
    ("01", "Segundo monitor", ["Arrastra ventanas a tu tablet", "como a cualquier pantalla."], ["Hasta 60 fps", "NVENC · VA-API"], STILL_MON),
    ("02", "Ratón y teclado", ["Lleva el ratón al borde y", "controla tu Android."], ["Sin root", "Sin apps extra"], dict(STILL_IN, clip=None)),
    ("03", "Portapapeles", ["Copia en el PC, pega en la", "tablet. Texto e imágenes."], ["Ida y vuelta", "Al instante"], STILL_IN),
    ("04", "USB o WiFi", ["Por cable con adb o por WiFi", "con WebRTC. Se adapta a la red."], ["Baja latencia", "Cualquier navegador"], dict(STILL_MON, ptr=None)),
]


def img_feature(i, story=False):
    num, title, lines, tags, st = FEATURES[i]
    if story:
        w, h = 1080, 1920
        b = background(w, h)
        b += logo(w / 2 - 60, 170, 120)
        b += text(w / 2, 380, "Encaje", 56, 800, "url(#txt)", "middle")
        b += text(w / 2, 560, title, 104, 800, WHITE, "middle", ls=-2)
        for k, l in enumerate(lines):
            b += text(w / 2, 650 + k * 56, l, 42, 400, SKY, "middle")
        b += scene_group(devices(st), -56, 800, 0.6)
        b += pills_row(w / 2, 1480, tags, 30)
        b += text(w / 2, 1720, "Descárgalo gratis", 44, 800, WHITE, "middle")
        b += text(w / 2, 1780, "github.com/sthifen/encaje", 32, 600, SKY, "middle")
        return svg(w, h, b)
    w, h = 1080, 1080
    b = background(w, h)
    b += logo(70, 70, 64) + text(150, 116, "Encaje", 40, 800, "url(#txt)")
    b += text(w - 70, 116, num, 40, 800, WHITE, "end", .35)
    b += text(70, 260, title, 92, 800, WHITE, ls=-2)
    for k, l in enumerate(lines):
        b += text(70, 330 + k * 50, l, 38, 400, SKY)
    b += scene_group(devices(st), -76, 400, 0.62)
    b += pills_row(w / 2, 960, tags, 28)
    return svg(w, h, b)


def img_cta_square():
    w, h = 1080, 1080
    b = background(w, h)
    b += logo(w / 2 - 90, 150, 180)
    b += text(w / 2, 450, "Encaje", 110, 800, "url(#txt)", "middle", ls=-2)
    b += text(w / 2, 530, "Tu tablet Android como segundo monitor,", 36, 400, SKY, "middle")
    b += text(w / 2, 578, "o con el ratón y el teclado del PC.", 36, 400, SKY, "middle")
    b += pills_row(w / 2, 650, ["AppImage", "tar.gz", "APK"], 30)
    b += f'<rect x="{w/2-300}" y="790" width="600" height="96" rx="48" fill="url(#accent)" filter="url(#soft)"/>'
    b += text(w / 2, 851, "Descárgalo gratis", 40, 800, WHITE, "middle")
    b += text(w / 2, 960, "github.com/sthifen/encaje", 32, 600, WHITE, "middle", .8)
    return svg(w, h, b)


# ---------- video ----------
FPS, DUR = 30, 16.0


def frame(t, vertical=False):
    w, h = (1080, 1920) if vertical else (1920, 1080)
    b = background(w, h, t)
    # Escena 1: logo (0–2.6 s)
    a1 = seg(t, 0.2, 1.0) * (1 - seg(t, 2.2, 2.7))
    if a1 > 0:
        s = lerp(0.6, 1.0, out_back(clamp((t - 0.2) / 0.8)))
        L = 200 * s
        b += logo(w / 2 - L / 2, h / 2 - 230 - L / 2 + 60, L, a1)
        b += text(w / 2, h / 2 + 90, "Encaje", 120, 800, "url(#txt)", "middle", a1 * seg(t, 0.6, 1.2), ls=-2)
        b += text(w / 2, h / 2 + 160, "Tu tablet, tu segundo monitor", 44, 400, SKY, "middle", a1 * seg(t, 1.0, 1.6))

    # Escenas 2–4: dispositivos (2.6–13 s)
    if 2.5 < t < 13.2:
        d_in = seg(t, 2.6, 3.4); d_out = 1 - seg(t, 12.6, 13.1)
        st = dict(laptop_in=d_in * d_out, tablet_in=seg(t, 2.9, 3.7) * d_out,
                  link=seg(t, 3.5, 4.0) * d_out, phase=t * 1.4)
        if t < 8.6:
            # monitor: la ventana pasa del portátil a la tablet; el puntero la arrastra
            k = seg(t, 4.6, 6.6)
            st.update(mode="monitor", win_x=lerp(420, 880, k))
            if 4.0 < t < 7.4:
                st["ptr"] = vptr(st["win_x"] + 150, 44 + 14)
            cap = ("Segundo monitor", "Arrastra ventanas a la tablet, a 60 fps")
            ca = seg(t, 3.8, 4.3) * (1 - seg(t, 8.2, 8.6))
        else:
            # entrada: el puntero cruza al Android y llega el portapapeles
            k = seg(t, 9.0, 10.4)
            st.update(mode="input", win_x=0, ptr=vptr(lerp(560, 1025, k), lerp(300, 240, k)), clip=(t - 10.6) / 1.6)
            cap = ("Ratón, teclado y portapapeles", "Controla tu Android desde el PC. Copia y pega entre los dos")
            ca = seg(t, 8.7, 9.2) * (1 - seg(t, 12.4, 12.9))
        if vertical:
            b += scene_group(devices(st), -56, 700, 0.6)
            b += text(w / 2, 400, cap[0], 72, 800, WHITE, "middle", ca, ls=-1)
            words = cap[1].split(". ")
            for i, l in enumerate(words):
                b += text(w / 2, 480 + i * 52, l, 38, 400, SKY, "middle", ca)
            b += logo(w / 2 - 40, 140, 80, d_in * d_out)
            b += pills_row(w / 2, 1330, ["GNOME", "Wayland", "USB", "WiFi"], 30, op=st["link"] * .9)
            b += text(w / 2, 1700, "github.com/sthifen/encaje", 36, 600, WHITE, "middle", .7 * d_in * d_out)
        else:
            b += scene_group(devices(st), 60, 190, 0.86)
            b += text(w / 2, 130, cap[0], 64, 800, WHITE, "middle", ca, ls=-1)
            b += text(w / 2, 185, cap[1], 32, 400, SKY, "middle", ca)
            b += pills_row(w / 2, 960, ["GNOME", "Wayland", "USB", "WiFi"], 24, op=st["link"] * .9)

    # Escena 5: llamada a la acción (13–16 s)
    a5 = seg(t, 13.0, 13.6)
    if a5 > 0:
        s = lerp(0.7, 1.0, out_back(clamp((t - 13.0) / 0.6)))
        cy = h / 2 - 40
        b += logo(w / 2 - 75 * s, cy - 330 + 75 * (1 - s), 150 * s, a5)
        b += text(w / 2, cy - 50, "Encaje", 130, 800, "url(#txt)", "middle", a5, ls=-2)
        b += text(w / 2, cy + 40, "Tu tablet, tu segundo monitor", 40, 400, SKY, "middle", seg(t, 13.3, 13.9))
        b += pills_row(w / 2, cy + 90, ["AppImage", "tar.gz", "APK"], 28, op=seg(t, 13.6, 14.2))
        bo = seg(t, 13.9, 14.5)
        b += f'<rect x="{w/2-280}" y="{cy+210}" width="560" height="90" rx="45" fill="url(#accent)" opacity="{bo}" filter="url(#soft)"/>'
        b += text(w / 2, cy + 268, "Descárgalo gratis", 40, 800, WHITE, "middle", bo)
        b += text(w / 2, cy + 370, "github.com/sthifen/encaje", 36, 600, WHITE, "middle", .85 * seg(t, 14.2, 14.8))
    return svg(w, h, b)


def render(svg_text, png):
    p = png[:-4] + ".svg"
    with open(p, "w") as f: f.write(svg_text)
    subprocess.run(["magick", "-background", "none", f"rsvg:{p}", png], check=True)
    os.remove(p)


def render_frame(args):
    i, vertical, d = args
    render(frame(i / FPS, vertical), os.path.join(d, f"f{i:04d}.png"))


def video(vertical):
    name = "encaje-video-vertical" if vertical else "encaje-video"
    d = os.path.join(OUT, "_frames_" + name); os.makedirs(d, exist_ok=True)
    n = int(FPS * DUR)
    with ProcessPoolExecutor() as ex:
        list(ex.map(render_frame, [(i, vertical, d) for i in range(n)], chunksize=4))
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(FPS), "-i", f"{d}/f%04d.png",
                    "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18", "-preset", "slow",
                    "-movflags", "+faststart", os.path.join(OUT, name + ".mp4")], check=True)
    subprocess.run(["rm", "-rf", d])


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    what = sys.argv[1] if len(sys.argv) > 1 else "all"
    if what in ("all", "images"):
        render(img_hero(), f"{OUT}/encaje-banner-1920x1080.png")
        render(img_social(), f"{OUT}/encaje-github-1280x640.png")
        render(img_cta_square(), f"{OUT}/encaje-post-descarga.png")
        slugs = ["monitor", "raton-teclado", "portapapeles", "usb-wifi"]
        for i, s in enumerate(slugs):
            render(img_feature(i), f"{OUT}/encaje-post-{i+1}-{s}.png")
            render(img_feature(i, True), f"{OUT}/encaje-historia-{i+1}-{s}.png")
    if what.startswith("frame"):
        t = float(what[5:]); render(frame(t), f"{OUT}/_test.png")
    if what in ("all", "video"):
        video(False); video(True)
    if what == "vertical":
        video(True)
