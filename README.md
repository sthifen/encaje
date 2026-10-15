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

## Instalación (sin compilar)

Descarga de la [última release](../../releases/latest):

- **`Spacedesk_Linux-*-x86_64.AppImage`**: un solo archivo.
  `chmod +x Spacedesk_Linux-*.AppImage` y ábrelo con doble clic.
- **`spacedesk-linux-*-x86_64.tar.gz`**: binario + acceso directo del menú.
  Descomprime y ejecuta `./install.sh` (instala en `~/.local`, sin sudo;
  `./install.sh --uninstall` lo quita).
- **`spacedesk-linux.apk`**: la app del móvil.

Al abrirlo desde el menú se abre una terminal con la IP y el estado; al
cerrarla se cierra el monitor virtual. Opciones por defecto (ver
[Opciones](#opciones)) en `~/.config/spacedesk-linux.conf`, por ejemplo:

```
--fps 60 --bitrate 20000
```

Requisitos: GNOME sobre Wayland con PipeWire y GStreamer (vienen de serie).
Para codificar con la GPU NVIDIA hace falta el plugin `nvcodec` de
`gstreamer1-plugins-bad`; si no está, se usa la CPU.

Los paquetes se generan con `packaging/build-dist.sh`.

## Compilar desde el código

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

**Si va lento por USB, revisa el cable.** Con un cable de mala calidad
(o sólo de carga a medias) el móvil negocia USB 1.1 a 12 Mbit/s en lugar de
480 Mbit/s. Compruébalo con:

```bash
cat /sys/bus/usb/devices/*/speed  # junto al dispositivo SAMSUNG_Android, etc.
lsusb -t                          # el móvil debería aparecer con 480M
```

Aun así el servidor se adapta: el visor confirma cada frame, el servidor
mide el retraso real y baja el bitrate antes de que se acumule (con un
enlace simulado de 7 Mbit/s el retraso de cola pasa de ~3 s a ~11 ms).

### Móvil o tablet por WiFi

En la app, escribe la IP del PC en el recuadro *WiFi*, o abre
`http://IP-DEL-PC:8787` en cualquier navegador. Se usa WebRTC.

## Resolución y rendimiento

La resolución del monitor virtual **se adapta a la pantalla que se conecta**:
el visor envía su tamaño en píxeles físicos y su orientación, y Mutter cambia
el modo del monitor en caliente (al girar una tablet también). Así cada
dispositivo recibe su proporción exacta, sin bandas negras.

En el menú del visor (⋮ → *Resolución*):

| Opción | Píxeles (móvil 2340×1080) | |
|---|---|---|
| Nativa | 2340×1080 | Máxima nitidez |
| 75 % | 1755×810 | ~56 % de los píxeles |
| 50 % | 1170×540 | ~25 % de los píxeles; ideal para enlaces lentos |
| Otra… | la que escribas | p. ej. `1600x900` |

Bajar la escala reduce el trabajo en **todas** las etapas (GNOME dibuja menos,
la GPU codifica menos y viaja menos por USB/WiFi); la imagen se estira a
pantalla completa, y los textos y ventanas se ven más grandes. La elección se
guarda en cada dispositivo. Si se conectan varios a la vez, manda el último.

## Opciones

| Opción | Por defecto | |
|---|---|---|
| `--width`, `--height` | automática | Fija la resolución e ignora la de los visores. |
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
