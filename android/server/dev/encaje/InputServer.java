package dev.encaje;

import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.system.Os;
import android.system.OsConstants;

import java.io.BufferedReader;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.FileDescriptor;
import java.io.IOException;
import java.io.InputStreamReader;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Modo "solo entrada": crea un ratón y un teclado HID virtuales (UHID) y les
 * reenvía los informes que manda el PC. Android los trata como periféricos
 * físicos: muestra su propio puntero y aplica la distribución de teclado.
 *
 * Se ejecuta con permisos de shell (el grupo uhid da acceso a /dev/uhid):
 *   adb push input-server.jar /data/local/tmp/
 *   adb shell CLASSPATH=/data/local/tmp/input-server.jar app_process / dev.encaje.InputServer
 *
 * Protocolo por el socket local "encaje-input":
 *   PC → tablet: [u8 dispositivo (0 ratón, 1 teclado)] [u8 longitud] [informe HID]
 *   tablet → PC: [u8 1] [f32 px por unidad de ratón] [u16 ancho] [u16 alto]
 *                al conectar y cada vez que gira la pantalla (big endian).
 *   en ambos sentidos: [u8 2] [u8 tipo (0 texto UTF-8, 1 imagen)] [u32 longitud]
 *                [datos], el portapapeles cuando cambia (ver Clipboard).
 *
 * Android 16 ignora los ratones HID absolutos, así que el ratón es relativo.
 * Con la aceleración desactivada el puntero avanza una cantidad fija por
 * unidad, y el PC puede llevar la cuenta de dónde está. Una sola sesión por
 * proceso: al cerrarse el socket se destruyen los dispositivos, se restaura
 * la aceleración y el proceso termina.
 */
public final class InputServer {
    private static final String SOCKET = "encaje-input";

    private static final int UHID_DESTROY = 1;
    private static final int UHID_CREATE2 = 11;
    private static final int UHID_INPUT2 = 12;
    private static final int BUS_VIRTUAL = 0x06;

    /** Ajustes de aceleración: el de Samsung y el de AOSP (Android 15+). */
    private static final String[] ACCEL_SETTINGS = {"enhance_pointer_precision", "mouse_pointer_acceleration_enabled"};

    /** Ratón relativo: 5 botones, X/Y -32767..32767, rueda vertical y horizontal. */
    static final byte[] MOUSE = {
        0x05, 0x01, 0x09, 0x02, (byte) 0xA1, 0x01, 0x09, 0x01, (byte) 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01,
        (byte) 0x95, 0x05, 0x75, 0x01, (byte) 0x81, 0x02,
        (byte) 0x95, 0x01, 0x75, 0x03, (byte) 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, (byte) 0x80, 0x26, (byte) 0xFF, 0x7F,
        0x75, 0x10, (byte) 0x95, 0x02, (byte) 0x81, 0x06,
        0x09, 0x38, 0x15, (byte) 0x81, 0x25, 0x7F, 0x75, 0x08, (byte) 0x95, 0x01, (byte) 0x81, 0x06,
        0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, (byte) 0x81, 0x25, 0x7F, 0x75, 0x08, (byte) 0x95, 0x01,
        (byte) 0x81, 0x06,
        (byte) 0xC0, (byte) 0xC0,
    };

    /** Teclado estándar (boot): modificadores + 6 teclas, con LEDs. */
    static final byte[] KEYBOARD = {
        0x05, 0x01, 0x09, 0x06, (byte) 0xA1, 0x01,
        0x05, 0x07, 0x19, (byte) 0xE0, 0x29, (byte) 0xE7, 0x15, 0x00, 0x25, 0x01,
        0x75, 0x01, (byte) 0x95, 0x08, (byte) 0x81, 0x02,
        (byte) 0x95, 0x01, 0x75, 0x08, (byte) 0x81, 0x01,
        0x05, 0x08, 0x19, 0x01, 0x29, 0x05, (byte) 0x95, 0x05, 0x75, 0x01, (byte) 0x91, 0x02,
        (byte) 0x95, 0x01, 0x75, 0x03, (byte) 0x91, 0x01,
        0x05, 0x07, 0x19, 0x00, 0x2A, (byte) 0xFF, 0x00, 0x15, 0x00, 0x26, (byte) 0xFF, 0x00,
        0x75, 0x08, (byte) 0x95, 0x06, (byte) 0x81, 0x00,
        (byte) 0xC0,
    };

    private static final String[] savedAccel = new String[ACCEL_SETTINGS.length];
    private static FileDescriptor mouse;
    private static FileDescriptor keyboard;

    private static final int DEV_CLIPBOARD = 2;
    private static final int MAX_CLIP = 32 << 20;
    private static Clipboard clipboard;
    /** Lo último que pasó por el portapapeles en cualquier sentido: evita el eco. */
    private static byte[] lastClip;
    /** Leer y escribir el portapapeles fuera del hilo de la entrada. */
    private static final ExecutorService clipThread = Executors.newSingleThreadExecutor();

    public static void main(String[] args) throws Exception {
        LocalServerSocket server = new LocalServerSocket(SOCKET);
        System.out.println("listo");
        LocalSocket client = server.accept();
        server.close();

        Runtime.getRuntime().addShutdownHook(new Thread(InputServer::cleanup));
        disableAcceleration();
        mouse = create("Encaje Mouse", 0x0001, MOUSE);
        keyboard = create("Encaje Keyboard", 0x0002, KEYBOARD);
        try {
            DataOutputStream out = new DataOutputStream(client.getOutputStream());
            float scale = calibrate();
            System.out.println("escala del puntero: " + scale);
            watchDisplay(out, scale);
            watchClipboard(out);

            DataInputStream in = new DataInputStream(client.getInputStream());
            byte[] report = new byte[255];
            while (true) {
                int dev = in.readUnsignedByte();
                if (dev == DEV_CLIPBOARD) {
                    readClip(in);
                    continue;
                }
                int len = in.readUnsignedByte();
                in.readFully(report, 0, len);
                input(dev == 0 ? mouse : keyboard, report, len);
            }
        } catch (IOException e) {
            // El PC cerró la conexión.
        } finally {
            cleanup();
            client.close();
        }
        System.exit(0);
    }

    private static synchronized void cleanup() {
        if (mouse != null) destroy(mouse);
        if (keyboard != null) destroy(keyboard);
        mouse = keyboard = null;
        restoreAcceleration();
    }

    // ---- Aceleración y calibración ----

    /**
     * Los valores originales se guardan también en un archivo: si el proceso
     * muere sin restaurarlos (adb desconectado de golpe), la siguiente
     * ejecución los recupera de ahí en lugar de leer los ya desactivados.
     */
    private static final String SAVED_FILE = "/data/local/tmp/encaje-accel.saved";

    private static void disableAcceleration() {
        String[] previous = sh("cat " + SAVED_FILE + " 2>/dev/null").split("\n");
        StringBuilder file = new StringBuilder();
        for (int i = 0; i < ACCEL_SETTINGS.length; i++) {
            String v = i < previous.length && !previous[i].isEmpty() ? previous[i].trim()
                : sh("settings get system " + ACCEL_SETTINGS[i]).trim();
            savedAccel[i] = v;
            file.append(v).append('\n');
        }
        sh("printf '" + file + "' > " + SAVED_FILE);
        for (String key : ACCEL_SETTINGS) sh("settings put system " + key + " 0");
    }

    private static void restoreAcceleration() {
        if (savedAccel[0] == null) return;
        for (int i = 0; i < ACCEL_SETTINGS.length; i++) {
            String v = savedAccel[i];
            savedAccel[i] = null;
            if (v.isEmpty() || v.equals("null")) sh("settings delete system " + ACCEL_SETTINGS[i]);
            else sh("settings put system " + ACCEL_SETTINGS[i] + " " + v);
        }
        sh("rm -f " + SAVED_FILE);
    }

    private static final Pattern HOVER = Pattern.compile("hoveringPointers=\\[Pointer\\(id=0, MOUSE\\) at \\(([\\d.]+), ([\\d.]+)\\)");

    /**
     * Píxeles que avanza el puntero por cada unidad de movimiento. Depende de
     * la densidad y de la velocidad del puntero, así que se mide: se lleva el
     * puntero a una esquina, se mueve 200 unidades y se lee su posición en
     * `dumpsys input`. Si no se puede leer, se asume 1.
     */
    private static float calibrate() throws Exception {
        Thread.sleep(600); // Android tarda un poco en abrir el dispositivo nuevo.
        for (int attempt = 0; attempt < 3; attempt++) {
            move(-32767, -32767);
            Thread.sleep(50);
            move(0, 1);
            move(0, -1);
            Thread.sleep(150);
            float[] a = hoverPosition();
            // Android 15: el dump ya no trae la posición; no tiene sentido reintentar.
            if (a == null && sh("dumpsys input").contains("hoveringPointers=[Pointer(id=0, MOUSE)]")) break;
            for (int i = 0; i < 4; i++) {
                move(50, 50);
                Thread.sleep(20);
            }
            Thread.sleep(150);
            float[] b = hoverPosition();
            if (a != null && b != null) {
                float dx = Math.abs(b[0] - a[0]), dy = Math.abs(b[1] - a[1]);
                // Con la pantalla girada X e Y vienen intercambiados; da igual.
                float k = (dx + dy) / 400f;
                if (k > 0.1f && k < 10f) return k;
            }
        }
        float k = calibrateByScreenshot();
        if (k > 0) return k;
        System.out.println("no se pudo medir la escala del puntero; se usa 1");
        return 1f;
    }

    /**
     * Android 15 ya no pone la posición del puntero en `dumpsys input`: se
     * busca el cursor en capturas de pantalla. Desde la esquina se baja 100
     * unidades (captura A), se avanza 200 a la derecha (B) y se vuelve (C),
     * todo despacio para que no haya aceleración. Sólo cuenta lo que cambia
     * entre A y B y vuelve a estar igual en C (el cursor, no un reloj o una
     * animación). La escala se mide en X (cuánto avanzó) y en Y (a qué altura
     * quedó) y sólo vale si las dos coinciden.
     */
    private static float calibrateByScreenshot() throws Exception {
        for (int attempt = 0; attempt < 3; attempt++) {
            move(-32767, -32767);
            Thread.sleep(600);
            walk(25, 0, 4);
            int[][] a = screenshot();
            walk(50, 4, 0);
            int[][] b = screenshot();
            walk(50, -4, 0);
            int[][] c = screenshot();
            float k = cursorScale(a, b, c);
            System.out.println("escala del puntero por capturas (intento " + (attempt + 1) + "): " + k);
            if (k > 0) return k;
        }
        return -1;
    }

    /** n pasos de (dx, dy) cada 20 ms (200 u/s) y una pausa para que se pinte. */
    private static void walk(int n, int dx, int dy) throws Exception {
        for (int i = 0; i < n; i++) {
            move(dx, dy);
            Thread.sleep(20);
        }
        Thread.sleep(300);
    }

    private static float cursorScale(int[][] a, int[][] b, int[][] c) {
        if (a == null || b == null || c == null) return -1;
        int h = a.length, w = a[0].length;
        if (b.length != h || c.length != h || b[0].length != w || c[0].length != w) return -1;
        // Franja del cursor de partida, pegado al borde izquierdo.
        int top = -1, bottom = -1;
        for (int y = 0; y < h; y++) {
            for (int x = 0; x < 30; x++) {
                if (cursor(a, b, c, x, y)) {
                    if (top < 0) top = y;
                    bottom = y;
                    break;
                }
            }
        }
        if (top < 0 || bottom - top > 80) return -1;
        // Primera columna a la derecha con varios píxeles del cursor en esa franja.
        int found = -1;
        for (int x = 40; x < w && found < 0; x++) {
            int n = 0;
            for (int y = top; y <= bottom; y++) {
                if (cursor(a, b, c, x, y)) n++;
            }
            if (n >= 3) found = x;
        }
        if (found < 0) return -1;
        float kx = found / 200f, ky = top / 100f;
        if (kx < 0.3f || kx > 6f || Math.abs(kx - ky) > 0.1f * kx) return -1;
        return kx;
    }

    /** Cambió al mover el cursor y volvió a estar igual al traerlo de vuelta. */
    private static boolean cursor(int[][] a, int[][] b, int[][] c, int x, int y) {
        return differs(a[y][x], b[y][x]) && !differs(a[y][x], c[y][x]);
    }

    private static boolean differs(int p, int q) {
        for (int s = 0; s < 24; s += 8) {
            if (Math.abs(((p >> s) & 0xFF) - ((q >> s) & 0xFF)) > 60) return true;
        }
        return false;
    }

    /** `screencap` sin -p: cabecera (ancho, alto, formato[, espacio de color]) y RGBA. */
    private static int[][] screenshot() {
        try {
            Process p = Runtime.getRuntime().exec(new String[] {"screencap"});
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream(1 << 23);
            byte[] buf = new byte[1 << 16];
            for (int n; (n = p.getInputStream().read(buf)) > 0; ) out.write(buf, 0, n);
            byte[] data = out.toByteArray();
            p.waitFor();
            ByteBuffer bb = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
            int w = bb.getInt(0), h = bb.getInt(4);
            int header = data.length - w * h * 4;
            if (w <= 0 || h <= 0 || header < 12 || header > 16) return null;
            int[][] px = new int[h][w];
            for (int y = 0; y < h; y++) {
                for (int x = 0; x < w; x++) {
                    int o = header + (y * w + x) * 4;
                    px[y][x] = (data[o] & 0xFF) | (data[o + 1] & 0xFF) << 8 | (data[o + 2] & 0xFF) << 16;
                }
            }
            return px;
        } catch (Exception e) {
            return null;
        }
    }

    private static float[] hoverPosition() {
        Matcher m = HOVER.matcher(sh("dumpsys input"));
        if (!m.find()) return null;
        return new float[] {Float.parseFloat(m.group(1)), Float.parseFloat(m.group(2))};
    }

    private static void move(int dx, int dy) throws Exception {
        ByteBuffer r = ByteBuffer.allocate(7).order(ByteOrder.LITTLE_ENDIAN);
        r.put((byte) 0).putShort((short) dx).putShort((short) dy).put((byte) 0).put((byte) 0);
        input(mouse, r.array(), 7);
    }

    // ---- Tamaño y orientación de la pantalla ----

    /** Envía el tamaño lógico (ya girado) al conectar y cada vez que cambia. */
    private static void watchDisplay(DataOutputStream out, float scale) throws IOException {
        int[] size = displaySize();
        sendInfo(out, scale, size);
        Thread t = new Thread(() -> {
            int[] last = size;
            try {
                while (true) {
                    Thread.sleep(500);
                    int[] now = displaySize();
                    if (now[0] != last[0] || now[1] != last[1]) {
                        sendInfo(out, scale, now);
                        last = now;
                    }
                }
            } catch (Exception ignored) {
            }
        });
        t.setDaemon(true);
        t.start();
    }

    private static synchronized void sendInfo(DataOutputStream out, float scale, int[] size) throws IOException {
        out.writeByte(1);
        out.writeFloat(scale);
        out.writeShort(size[0]);
        out.writeShort(size[1]);
        out.flush();
    }

    private static Method getDisplayInfo;
    private static Object displayManager;

    private static int[] displaySize() {
        try {
            if (getDisplayInfo == null) {
                Class<?> dmg = Class.forName("android.hardware.display.DisplayManagerGlobal");
                displayManager = dmg.getMethod("getInstance").invoke(null);
                getDisplayInfo = dmg.getMethod("getDisplayInfo", int.class);
            }
            Object info = getDisplayInfo.invoke(displayManager, 0);
            int w = info.getClass().getField("logicalWidth").getInt(info);
            int h = info.getClass().getField("logicalHeight").getInt(info);
            return new int[] {w, h};
        } catch (Exception e) {
            // Plan B: `wm size` da el tamaño sin girar.
            Matcher m = Pattern.compile("(\\d+)x(\\d+)").matcher(sh("wm size"));
            return m.find() ? new int[] {Integer.parseInt(m.group(1)), Integer.parseInt(m.group(2))} : new int[] {1920, 1080};
        }
    }

    // ---- Portapapeles ----

    private static void watchClipboard(DataOutputStream out) {
        try {
            clipboard = new Clipboard();
            clipboard.listen(() -> clipThread.execute(() -> {
                try {
                    Clipboard.Content c = clipboard.read();
                    if (c == null || c.data.length > MAX_CLIP || Arrays.equals(c.data, lastClip)) return;
                    lastClip = c.data;
                    sendClip(out, c);
                } catch (Exception e) {
                    System.out.println("portapapeles: " + e);
                }
            }));
            System.out.println("portapapeles compartido");
        } catch (Exception e) {
            clipboard = null;
            System.out.println("sin portapapeles compartido: " + e);
        }
    }

    private static void readClip(DataInputStream in) throws IOException {
        int kind = in.readUnsignedByte();
        int len = in.readInt();
        if (len < 0 || len > MAX_CLIP) throw new IOException("portapapeles demasiado grande: " + len);
        byte[] data = new byte[len];
        in.readFully(data);
        if (clipboard == null) return;
        clipThread.execute(() -> {
            try {
                lastClip = data;
                if (kind == Clipboard.PNG) clipboard.writeImage(data);
                else clipboard.writeText(new String(data, "UTF-8"));
            } catch (Exception e) {
                System.out.println("portapapeles: " + e);
            }
        });
    }

    private static synchronized void sendClip(DataOutputStream out, Clipboard.Content c) throws IOException {
        out.writeByte(DEV_CLIPBOARD);
        out.writeByte(c.kind);
        out.writeInt(c.data.length);
        out.write(c.data);
        out.flush();
    }

    // ---- UHID ----

    private static FileDescriptor create(String name, int product, byte[] descriptor) throws Exception {
        FileDescriptor fd = Os.open("/dev/uhid", OsConstants.O_RDWR, 0);
        ByteBuffer ev = ByteBuffer.allocate(4 + 4372).order(ByteOrder.nativeOrder());
        ev.putInt(UHID_CREATE2);
        byte[] n = name.getBytes("UTF-8");
        ev.put(n, 0, Math.min(n.length, 127));
        ev.position(4 + 128 + 64 + 64);
        ev.putShort((short) descriptor.length);
        ev.putShort((short) BUS_VIRTUAL);
        ev.putInt(0x18D1); // vendor
        ev.putInt(product);
        ev.putInt(1); // version
        ev.putInt(0); // country
        ev.put(descriptor);
        Os.write(fd, ev.array(), 0, ev.capacity());
        drain(fd);
        return fd;
    }

    private static void input(FileDescriptor fd, byte[] report, int len) throws Exception {
        ByteBuffer ev = ByteBuffer.allocate(4 + 2 + len).order(ByteOrder.nativeOrder());
        ev.putInt(UHID_INPUT2);
        ev.putShort((short) len);
        ev.put(report, 0, len);
        Os.write(fd, ev.array(), 0, ev.capacity());
    }

    private static void destroy(FileDescriptor fd) {
        try {
            ByteBuffer ev = ByteBuffer.allocate(4).order(ByteOrder.nativeOrder());
            ev.putInt(UHID_DESTROY);
            Os.write(fd, ev.array(), 0, 4);
            Os.close(fd);
        } catch (Exception ignored) {
        }
    }

    /** El kernel encola eventos (START, OPEN, OUTPUT con los LEDs…): se leen y se descartan. */
    private static void drain(FileDescriptor fd) {
        Thread t = new Thread(() -> {
            byte[] buf = new byte[4 + 4372];
            try {
                while (Os.read(fd, buf, 0, buf.length) > 0) {
                }
            } catch (Exception ignored) {
            }
        });
        t.setDaemon(true);
        t.start();
    }

    private static String sh(String cmd) {
        try {
            Process p = Runtime.getRuntime().exec(new String[] {"sh", "-c", cmd});
            StringBuilder sb = new StringBuilder();
            try (BufferedReader r = new BufferedReader(new InputStreamReader(p.getInputStream()))) {
                String line;
                while ((line = r.readLine()) != null) sb.append(line).append('\n');
            }
            p.waitFor();
            return sb.toString();
        } catch (Exception e) {
            return "";
        }
    }
}
