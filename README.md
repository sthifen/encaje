# Spacedesk Linux

Monitor virtual para **GNOME / Wayland** que se transmite a un móvil Android
(app nativa) o a cualquier navegador, por **cable USB** o por **WiFi**, con
entrada táctil y de ratón de vuelta al PC.

```
Mutter RecordVirtual ──► PipeWire ──► H.264 (NVENC / VA-API / OpenH264)
   (monitor virtual)       (BGRx)            │
                                             ├─► WebSocket + WebCodecs ── USB (adb reverse)
                                             └─► WebRTC ───────────────── WiFi / LAN
Visor ──(táctil, ratón, teclado)──► WebSocket ──► Mutter RemoteDesktop
```

## Uso rápido

```bash
scripts/setup-deps.sh          # una vez: cabeceras de PipeWire sin sudo
cargo build --release
./target/release/linux-spacedesk-clone --width 2340 --height 1080 --fps 60
```

Aparece un monitor nuevo en *Configuración → Pantallas*; puedes arrastrar
ventanas a él. Al cerrar el programa (Ctrl+C) desaparece.

### Móvil por USB (como Spacedesk)

1. Activa *Opciones de desarrollador → Depuración USB* y conecta el cable.
2. Instala la app: `android/build-apk.sh --install` (o copia
   `android/spacedesk-linux.apk` al móvil).
3. Abre **Spacedesk Linux**: detecta el PC sola y entra a pantalla completa.

El servidor ejecuta `adb reverse tcp:8787 tcp:8787` en cada móvil conectado
(y lo repite si el móvil se reconecta). Por USB se usa WebSocket +
WebCodecs, porque `adb reverse` sólo reenvía TCP.

### Móvil o tablet por WiFi

En la app, escribe la IP del PC en el recuadro *WiFi*, o abre
`http://IP-DEL-PC:8787` en cualquier navegador. Se usa WebRTC.

## Opciones

| Opción | Por defecto | |
|---|---|---|
| `--width`, `--height` | 1920×1080 | Resolución del monitor virtual. Usa la del móvil en horizontal. |
| `--fps` | 60 | Límite de fotogramas por segundo. |
| `--bitrate` | 12000 | kbit/s. |
| `--encoder` | `auto` | `nvenc`, `vaapi`, `openh264`. `auto` prueba el hardware y usa la CPU si falla. |
| `--port` | 8787 | Puerto HTTP. |
| `--no-usb` | | No ejecutar `adb reverse`. |

`RUST_LOG=debug` muestra más detalle. Cada 5 s se registran los fps de
captura y envío, el coste de codificar y la edad del frame.

## Rendimiento medido (2340×1080, GTX 1650 + i7 Alder Lake)

| Codificador | Coste por frame | fps enviados |
|---|---|---|
| NVENC | ~1.5 ms (asíncrono) + ~1.5 ms de copia | ≈ los capturados (~50) |
| OpenH264 (CPU) | 30–80 ms | 10–20 |

OpenH264 sólo es razonable hasta ~1280×720.

## Estructura

| Ruta | |
|---|---|
| `src/display.rs` | Detección del entorno, monitor virtual y entrada (D-Bus a Mutter). |
| `src/capture.rs` | Consumidor PipeWire; deja sólo el frame más reciente. |
| `src/encoder.rs` | Backends H.264, keyframes bajo demanda, estadísticas. |
| `src/server.rs` | HTTP (axum): visor, `/offer` WebRTC, `/ws/video`, `/ws/input`. |
| `src/rtc_peer.rs` | Un `PeerConnection` (webrtc-rs) por visor, sin trickle ICE. |
| `src/adb.rs` | Modo USB (`adb reverse` automático). |
| `web/index.html` | Visor (incrustado en el binario). |
| `android/` | App: WebView a pantalla completa y lanzador USB/WiFi. Se compila sin Gradle. |

## Limitaciones

- Sólo GNOME sobre Wayland: `RecordVirtual` es una API de Mutter. En X11,
  KDE o wlroots haría falta otro mecanismo (evdi, `krfb-virtualmonitor`, …).
- Sin audio.
- WebRTC no reacciona a los PLI del navegador; se recupera con el IDR
  periódico (cada 2 s).
- El backend VA-API no se ha probado (en este equipo el driver de Intel no
  codifica H.264).
- La resolución se fija al arrancar; no se adapta a cada cliente.
