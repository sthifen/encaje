package dev.encaje;

import android.app.Activity;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.res.Configuration;
import android.graphics.Color;
import android.graphics.Point;
import android.media.MediaCodec;
import android.media.MediaFormat;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.util.TypedValue;
import android.view.Display;
import android.view.Gravity;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.Menu;
import android.view.MotionEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.PopupMenu;
import android.widget.TextView;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Locale;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Visor nativo: el mismo flujo que el visor web (/ws/video, H.264 Annex-B),
 * pero decodificado con MediaCodec en modo de baja latencia y dibujado
 * directamente en un SurfaceView. Sin WebView, sin canvas y sin el hilo de
 * JavaScript por medio: en tablets modestas el WebView no pasaba de ~80 fps
 * y retenía ~14 frames (120 ms); así cada frame va a la pantalla en cuanto
 * sale del decodificador.
 *
 * Hilos: uno lee el vídeo de la red; otro (codecThread) es el único que toca
 * MediaCodec, con sus callbacks asíncronos; otro mantiene la conexión de
 * entrada, y los envíos salen de un ejecutor para no hacer red en el hilo de
 * la interfaz.
 */
public class NativeViewerActivity extends Activity implements SurfaceHolder.Callback {
    static final String EXTRA_HOST = "host", EXTRA_PORT = "port";
    /** Resultado: el usuario pidió el visor web. */
    static final int RESULT_WEB = RESULT_FIRST_USER;

    private static final int[] RATES = {144, 120, 90, 75, 60, 48, 30};

    private String host;
    private int port;
    private SharedPreferences prefs;

    private FrameLayout root;
    private SurfaceView surface;
    private TextView status;
    private EditText kbd;
    private final Handler ui = new Handler(Looper.getMainLooper());

    private volatile boolean running;
    private volatile SurfaceHolder holder;
    private Thread videoThread;
    private volatile WebSocketClient videoWs, inputWs;
    private final ExecutorService sender = Executors.newSingleThreadExecutor();

    // ── Estado del decodificador (sólo en codecThread) ──
    private HandlerThread codecThread;
    private Handler codec;
    private MediaCodec decoder;
    private String decoderName = "?";
    private byte[] configuredSps;
    private boolean waitKey = true;
    private final ArrayDeque<Integer> freeInputs = new ArrayDeque<>();
    private final ArrayDeque<Frame> pending = new ArrayDeque<>();
    private final HashMap<Long, Long> queuedAt = new HashMap<>();
    private int videoW, videoH;

    // Medidas (codecThread), como las del visor web.
    private int decoded, resyncs, maxPending, winResyncs, winMaxPending, winN, clean;
    private double decodeMs, winDecodeMs;
    private long statsSince = System.nanoTime(), raisedAt, decoderSince;
    private int fpsCap = Integer.MAX_VALUE;
    private final java.util.HashSet<Integer> banned = new java.util.HashSet<>();

    private static final class Frame {
        final boolean key;
        final long pts;
        final byte[] data;
        final int off;
        Frame(boolean key, long pts, byte[] data, int off) { this.key = key; this.pts = pts; this.data = data; this.off = off; }
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        host = getIntent().getStringExtra(EXTRA_HOST);
        port = getIntent().getIntExtra(EXTRA_PORT, 8787);
        prefs = getSharedPreferences("encaje", MODE_PRIVATE);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            getWindow().getAttributes().layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        }
        MainActivity.preferHighestRefreshRate(this);

        root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        surface = new SurfaceView(this);
        surface.getHolder().addCallback(this);
        root.addView(surface, new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT, Gravity.CENTER));

        status = new TextView(this);
        status.setTextColor(0xffe8e8e8);
        status.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        status.setBackgroundColor(0xd0141414);
        int pad = dp(16);
        status.setPadding(pad, pad, pad, pad);
        root.addView(status, new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.CENTER));

        TextView menu = new TextView(this);
        menu.setText("⋮");
        menu.setTextColor(0xffe8e8e8);
        menu.setTextSize(TypedValue.COMPLEX_UNIT_SP, 20);
        menu.setGravity(Gravity.CENTER);
        menu.setBackgroundColor(0xd0141414);
        menu.setAlpha(0.35f);
        menu.setOnClickListener(this::showMenu);
        FrameLayout.LayoutParams mlp = new FrameLayout.LayoutParams(dp(40), dp(40), Gravity.TOP | Gravity.END);
        mlp.setMargins(0, dp(8), dp(8), 0);
        root.addView(menu, mlp);

        // Campo invisible para el teclado en pantalla: siempre contiene un
        // espacio, así se distingue "borrar" de "escribir".
        kbd = new EditText(this);
        kbd.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        kbd.setAlpha(0f);
        kbd.setText(" ");
        kbd.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence s, int a, int b, int c) {}
            @Override public void onTextChanged(CharSequence s, int a, int b, int c) {}
            @Override public void afterTextChanged(Editable e) {
                String t = e.toString();
                if (t.equals(" ")) return;
                if (t.isEmpty()) {
                    tapKey(0xff08);
                } else {
                    for (int i = 1; i < t.length(); ) {
                        int cp = t.codePointAt(i);
                        tapKey(cp == '\n' ? 0xff0d : keysymOfCodePoint(cp));
                        i += Character.charCount(cp);
                    }
                }
                e.replace(0, e.length(), " ");
            }
        });
        root.addView(kbd, new FrameLayout.LayoutParams(1, 1, Gravity.TOP | Gravity.START));

        root.addOnLayoutChangeListener((v, l, t, r, b, ol, ot, or, ob) -> relayout());
        surface.setOnTouchListener((v, ev) -> onTouch(ev));
        surface.setOnGenericMotionListener((v, ev) -> onGeneric(ev));
        setContentView(root);
        setStatus("Conectando…");

        codecThread = new HandlerThread("encaje-codec", android.os.Process.THREAD_PRIORITY_URGENT_DISPLAY);
        codecThread.start();
        codec = new Handler(codecThread.getLooper());
        running = true;
        videoThread = new Thread(this::videoLoop, "encaje-video");
        videoThread.setPriority(Thread.MAX_PRIORITY);
        videoThread.start();
        new Thread(this::inputLoop, "encaje-input").start();
        codec.postDelayed(this::adaptTick, 2000);
        codec.postDelayed(this::reportTick, 5000);
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }

    private void setStatus(String text) {
        ui.post(() -> {
            status.setText(text == null ? "" : text);
            status.setVisibility(text == null ? View.GONE : View.VISIBLE);
        });
    }

    // ── Red: vídeo ──────────────────────────────────────────────────────────

    private void videoLoop() {
        while (running) {
            try (WebSocketClient ws = WebSocketClient.connect(host, port, "/ws/video", 3000)) {
                videoWs = ws;
                codec.post(() -> { waitKey = true; pending.clear(); });
                while (running) {
                    WebSocketClient.Message m = ws.read();
                    if (m.opcode != WebSocketClient.BINARY || m.data.length < 10) continue;
                    byte[] d = m.data;
                    long pts = 0;
                    for (int i = 1; i <= 8; i++) pts = (pts << 8) | (d[i] & 0xff);
                    // Confirmación de llegada: el servidor ajusta el bitrate a
                    // lo que entrega de verdad el enlace.
                    ws.sendText("{\"ack\":" + pts + "}");
                    Frame f = new Frame((d[0] & 1) == 1, pts, d, 9);
                    codec.post(() -> onFrame(f));
                }
            } catch (IOException e) {
                if (running) {
                    setStatus("Desconectado · reintentando…\n" + e.getMessage());
                    sleep(1000);
                }
            } finally {
                videoWs = null;
            }
        }
    }

    /** Pide un keyframe por haber perdido la cadena de referencias. */
    private void requestKey() {
        resyncs++;
        winResyncs++;
        askKeyframe();
    }

    /** Pide un keyframe sin contarlo como resincronización (p. ej. superficie nueva). */
    private void askKeyframe() {
        queuedAt.clear();
        WebSocketClient ws = videoWs;
        if (ws != null) sender.execute(() -> { try { ws.sendText("keyframe"); } catch (IOException ignored) {} });
    }

    // ── Decodificador (codecThread) ─────────────────────────────────────────

    private void onFrame(Frame f) {
        if (f.key) {
            byte[][] ps = paramSets(f.data, f.off);
            if (ps[0] != null && (decoder == null || !Arrays.equals(ps[0], configuredSps))) {
                if (!createDecoder(ps[0], ps[1])) return;
            }
            waitKey = false;
        }
        if (waitKey || decoder == null) return;
        pending.add(f);
        maxPending = Math.max(maxPending, pending.size());
        winMaxPending = Math.max(winMaxPending, pending.size());
        // El decodificador no da abasto: descartar hasta el próximo keyframe
        // en vez de acumular retraso.
        // Al arrancar, el decodificador tarda en aceptar los primeros frames:
        // más margen durante esos segundos.
        boolean starting = System.nanoTime() - decoderSince < 4_000_000_000L;
        if (pending.size() > (starting ? 45 : 8)) {
            pending.clear();
            waitKey = true;
            requestKey();
            return;
        }
        feed();
    }

    private void feed() {
        while (decoder != null && !pending.isEmpty() && !freeInputs.isEmpty()) {
            Frame f = pending.poll();
            int idx = freeInputs.poll();
            try {
                ByteBuffer buf = decoder.getInputBuffer(idx);
                int len = f.data.length - f.off;
                if (buf == null || buf.capacity() < len) {
                    // Frame más grande que el búfer: se pierde la cadena.
                    freeInputs.addFirst(idx);
                    pending.clear();
                    waitKey = true;
                    requestKey();
                    return;
                }
                buf.clear();
                buf.put(f.data, f.off, len);
                queuedAt.put(f.pts, System.nanoTime());
                if (queuedAt.size() > 256) queuedAt.clear();
                decoder.queueInputBuffer(idx, 0, len, f.pts, 0);
            } catch (IllegalStateException e) {
                resetDecoder("cola de entrada: " + e);
                return;
            }
        }
    }

    private boolean createDecoder(byte[] sps, byte[] pps) {
        releaseDecoder();
        SurfaceHolder h = holder;
        if (h == null || !h.getSurface().isValid()) return false;
        int[] size = SpsSize.parse(sps);
        int w = size != null ? size[0] : 1920, hgt = size != null ? size[1] : 1080;
        for (int attempt = 0; attempt < 2; attempt++) {
            MediaCodec c = null;
            try {
                MediaFormat fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, hgt);
                fmt.setByteBuffer("csd-0", ByteBuffer.wrap(withStartCode(sps)));
                if (pps != null) fmt.setByteBuffer("csd-1", ByteBuffer.wrap(withStartCode(pps)));
                fmt.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, Math.max(w * hgt, 1 << 20));
                if (attempt == 0) lowLatency(fmt);
                c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
                final MediaCodec dec = c;
                c.setCallback(new MediaCodec.Callback() {
                    @Override public void onInputBufferAvailable(MediaCodec m, int index) {
                        if (m != decoder) return;
                        freeInputs.add(index);
                        feed();
                    }
                    @Override public void onOutputBufferAvailable(MediaCodec m, int index, MediaCodec.BufferInfo info) {
                        if (m != decoder) return;
                        Long t = queuedAt.remove(info.presentationTimeUs);
                        if (t != null) {
                            double ms = (System.nanoTime() - t) / 1e6;
                            decodeMs += ms;
                            winDecodeMs += ms;
                            winN++;
                        }
                        decoded++;
                        try {
                            m.releaseOutputBuffer(index, true); // a la pantalla ya
                        } catch (IllegalStateException ignored) {}
                        if (decoded == 1) setStatus(null);
                    }
                    @Override public void onError(MediaCodec m, MediaCodec.CodecException e) {
                        if (m == decoder) resetDecoder("decodificador: " + e.getDiagnosticInfo());
                    }
                    @Override public void onOutputFormatChanged(MediaCodec m, MediaFormat f) {
                        if (m != decoder) return;
                        int vw = f.getInteger(MediaFormat.KEY_WIDTH), vh = f.getInteger(MediaFormat.KEY_HEIGHT);
                        if (f.containsKey("crop-right") && f.containsKey("crop-left")) {
                            vw = f.getInteger("crop-right") - f.getInteger("crop-left") + 1;
                            vh = f.getInteger("crop-bottom") - f.getInteger("crop-top") + 1;
                        }
                        videoW = vw;
                        videoH = vh;
                        ui.post(NativeViewerActivity.this::relayout);
                    }
                }, codec);
                c.configure(fmt, h.getSurface(), null, 0);
                c.start();
                decoder = dec;
                decoderSince = System.nanoTime();
                decoderName = c.getName();
                configuredSps = sps;
                videoW = w;
                videoH = hgt;
                ui.post(this::relayout);
                return true;
            } catch (Exception e) {
                // Algunos decodificadores rechazan las claves de baja
                // latencia: segundo intento sin ellas.
                if (c != null) c.release();
                if (attempt == 1) setStatus("No se pudo crear el decodificador:\n" + e);
            }
        }
        return false;
    }

    /** Claves de baja latencia: la estándar y las de cada fabricante (se ignoran si no aplican). */
    private static void lowLatency(MediaFormat f) {
        if (Build.VERSION.SDK_INT >= 30) f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
        f.setInteger(MediaFormat.KEY_PRIORITY, 0); // tiempo real
        f.setInteger("vendor.qti-ext-dec-low-latency.enable", 1);
        f.setInteger("vendor.qti-ext-dec-picture-order.enable", 1);
        f.setInteger("vendor.rtc-ext-dec-low-latency.enable", 1);
        f.setInteger("vendor.low-latency.enable", 1);
        f.setInteger("vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req", 1);
        f.setInteger("vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy", -1);
    }

    private void resetDecoder(String why) {
        android.util.Log.w("Encaje", why);
        releaseDecoder();
        waitKey = true;
        requestKey();
    }

    private void releaseDecoder() {
        MediaCodec d = decoder;
        decoder = null;
        configuredSps = null;
        freeInputs.clear();
        pending.clear();
        queuedAt.clear();
        if (d != null) {
            try { d.stop(); } catch (Exception ignored) {}
            d.release();
        }
    }

    /** SPS y PPS (sin start code) de una unidad de acceso Annex-B. */
    private static byte[][] paramSets(byte[] d, int off) {
        byte[][] out = new byte[2][];
        int i = off, n = d.length;
        while (i + 3 < n) {
            if (d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) {
                int s = i + 3, type = d[s] & 0x1f, e = s;
                while (e + 2 < n && !(d[e] == 0 && d[e + 1] == 0 && (d[e + 2] == 1 || (d[e + 2] == 0 && e + 3 < n && d[e + 3] == 1)))) e++;
                if (e + 2 >= n) e = n;
                if (type == 7) out[0] = Arrays.copyOfRange(d, s, e);
                else if (type == 8) out[1] = Arrays.copyOfRange(d, s, e);
                else if (type == 5 || type == 1) break; // ya en los datos del frame
                i = e;
            } else {
                i++;
            }
        }
        return out;
    }

    private static byte[] withStartCode(byte[] nal) {
        byte[] b = new byte[nal.length + 4];
        b[3] = 1;
        System.arraycopy(nal, 0, b, 4, nal.length);
        return b;
    }

    // ── Frecuencia adaptativa y medidas (codecThread) ───────────────────────

    private int targetFps() {
        return Math.min(Math.round(displayHz()), fpsCap);
    }

    /** Igual que el visor web: baja si el decodificador hace cola, sube tras un minuto limpio. */
    private void adaptTick() {
        if (!running) return;
        codec.postDelayed(this::adaptTick, 2000);
        // Los primeros 4 s tras crear el decodificador no cuentan: arrancar
        // (y el primer keyframe) siempre hace algo de cola.
        if (decoder == null || winN < 10 || System.nanoTime() - decoderSince < 4_000_000_000L) { resetWindow(); return; }
        int cur = targetFps();
        double avg = winDecodeMs / winN;
        int hz = Math.round(displayHz());
        if (winMaxPending >= 4 || winResyncs > 0 || avg > Math.max(20, 2000.0 / cur)) {
            if (System.nanoTime() - raisedAt < 15_000_000_000L) banned.add(cur);
            for (int r : RATES) {
                if (r < cur - 1) { fpsCap = r; clean = 0; sendSize(); break; }
            }
        } else if (winMaxPending <= 1 && avg < 1000.0 / cur && ++clean >= 30 && fpsCap < hz) {
            clean = 0;
            for (int i = RATES.length - 1; i >= 0; i--) {
                int r = RATES[i];
                if (r > cur + 1 && r <= hz + 1 && !banned.contains(r)) {
                    fpsCap = r >= hz - 1 ? Integer.MAX_VALUE : r;
                    raisedAt = System.nanoTime();
                    sendSize();
                    break;
                }
            }
        }
        resetWindow();
    }

    private void resetWindow() {
        winMaxPending = 0;
        winResyncs = 0;
        winDecodeMs = 0;
        winN = 0;
    }

    private String statsJson() {
        double dt = (System.nanoTime() - statsSince) / 1e9;
        int n = Math.max(decoded, 1);
        return String.format(Locale.ROOT,
                "{\"stats\":{\"fps\":%.1f,\"decode_ms\":%.2f,\"max_queue\":%d,\"resyncs\":%d,\"hz\":%.1f,"
                + "\"target_fps\":%d,\"decoder\":\"%s (nativo)\"}}",
                decoded / dt, decodeMs / n, maxPending, resyncs, displayHz(), targetFps(), decoderName);
    }

    private volatile String lastStats = "–";

    private void reportTick() {
        if (!running) return;
        codec.postDelayed(this::reportTick, 5000);
        String json = statsJson();
        double dt = (System.nanoTime() - statsSince) / 1e9;
        lastStats = String.format(Locale.ROOT, "%dx%d · %.0f fps / %.0f Hz · decodificar %.1f ms",
                videoW, videoH, decoded / dt, displayHz(), decodeMs / Math.max(decoded, 1));
        decoded = 0; decodeMs = 0; maxPending = 0; resyncs = 0; statsSince = System.nanoTime();
        WebSocketClient ws = videoWs;
        if (ws != null) sender.execute(() -> { try { ws.sendText(json); } catch (IOException ignored) {} });
    }

    // ── Pantalla ────────────────────────────────────────────────────────────

    @SuppressWarnings("deprecation")
    private float displayHz() {
        Display d = Build.VERSION.SDK_INT >= 30 ? getDisplay() : getWindowManager().getDefaultDisplay();
        return d == null ? 60f : d.getRefreshRate();
    }

    /** Tamaño pedido al PC: la pantalla en píxeles físicos × la escala elegida. */
    @SuppressWarnings("deprecation")
    private void sendSize() {
        Point p = new Point();
        getWindowManager().getDefaultDisplay().getRealSize(p);
        String scale = prefs.getString("scale", "1");
        float k;
        try { k = Float.parseFloat(scale); } catch (NumberFormatException e) { k = 1f; }
        int w = Math.round(p.x * k), h = Math.round(p.y * k);
        send(String.format(Locale.ROOT, "{\"t\":\"resize\",\"w\":%d,\"h\":%d,\"hz\":%d}", w, h, targetFps()));
    }

    /** El SurfaceView ocupa el área con la proporción del vídeo, centrada. */
    private void relayout() {
        int cw = root.getWidth(), ch = root.getHeight();
        if (cw == 0 || ch == 0 || videoW == 0 || videoH == 0) return;
        float s = Math.min((float) cw / videoW, (float) ch / videoH);
        FrameLayout.LayoutParams lp = (FrameLayout.LayoutParams) surface.getLayoutParams();
        int w = Math.round(videoW * s), h = Math.round(videoH * s);
        if (lp.width != w || lp.height != h) {
            lp.width = w;
            lp.height = h;
            surface.setLayoutParams(lp);
        }
    }

    @Override
    public void onConfigurationChanged(Configuration c) {
        super.onConfigurationChanged(c);
        ui.postDelayed(() -> { relayout(); sendSize(); }, 300);
    }

    @Override public void surfaceCreated(SurfaceHolder h) {
        holder = h;
        codec.post(() -> { waitKey = true; askKeyframe(); });
    }
    @Override public void surfaceChanged(SurfaceHolder h, int f, int w, int hh) { holder = h; }
    @Override public void surfaceDestroyed(SurfaceHolder h) {
        holder = null;
        // Hay que soltar el decodificador antes de que desaparezca la superficie.
        java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
        codec.post(() -> { releaseDecoder(); waitKey = true; done.countDown(); });
        try { done.await(1, java.util.concurrent.TimeUnit.SECONDS); } catch (InterruptedException ignored) {}
    }

    // ── Entrada ─────────────────────────────────────────────────────────────

    private void inputLoop() {
        while (running) {
            try {
                WebSocketClient ws = WebSocketClient.connect(host, port, "/ws/input", 3000);
                inputWs = ws;
                ui.post(this::sendSize);
                ws.read(); // el servidor no manda nada: vuelve al cerrarse
            } catch (IOException e) {
                inputWs = null;
                if (running) sleep(1000);
            }
        }
    }

    private void send(String json) {
        WebSocketClient ws = inputWs;
        if (ws == null) return;
        sender.execute(() -> {
            try { ws.sendText(json); } catch (IOException ignored) {}
        });
    }

    private String xy(float x, float y) {
        float nx = Math.max(0, Math.min(1, x / Math.max(1, surface.getWidth())));
        float ny = Math.max(0, Math.min(1, y / Math.max(1, surface.getHeight())));
        return String.format(Locale.ROOT, "\"x\":%.5f,\"y\":%.5f", nx, ny);
    }

    private boolean onTouch(MotionEvent ev) {
        if (ev.isFromSource(InputDevice.SOURCE_MOUSE)) {
            // Los clics llegan como BUTTON_PRESS/RELEASE (onGeneric); aquí sólo arrastrar.
            if (ev.getActionMasked() == MotionEvent.ACTION_MOVE) send("{\"t\":\"move\"," + xy(ev.getX(), ev.getY()) + "}");
            return true;
        }
        int a = ev.getActionMasked();
        switch (a) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN: {
                int i = ev.getActionIndex();
                send("{\"t\":\"touch_down\",\"id\":" + ev.getPointerId(i) + "," + xy(ev.getX(i), ev.getY(i)) + "}");
                break;
            }
            case MotionEvent.ACTION_MOVE:
                for (int i = 0; i < ev.getPointerCount(); i++) {
                    send("{\"t\":\"touch_move\",\"id\":" + ev.getPointerId(i) + "," + xy(ev.getX(i), ev.getY(i)) + "}");
                }
                break;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_POINTER_UP:
                send("{\"t\":\"touch_up\",\"id\":" + ev.getPointerId(ev.getActionIndex()) + "}");
                break;
            case MotionEvent.ACTION_CANCEL:
                for (int i = 0; i < ev.getPointerCount(); i++) send("{\"t\":\"touch_up\",\"id\":" + ev.getPointerId(i) + "}");
                break;
            default:
                break;
        }
        return true;
    }

    private boolean onGeneric(MotionEvent ev) {
        if (!ev.isFromSource(InputDevice.SOURCE_MOUSE)) return false;
        switch (ev.getActionMasked()) {
            case MotionEvent.ACTION_HOVER_MOVE:
                send("{\"t\":\"move\"," + xy(ev.getX(), ev.getY()) + "}");
                return true;
            case MotionEvent.ACTION_BUTTON_PRESS:
            case MotionEvent.ACTION_BUTTON_RELEASE: {
                int b = ev.getActionButton();
                int code = b == MotionEvent.BUTTON_SECONDARY ? 2 : b == MotionEvent.BUTTON_TERTIARY ? 1 : 0;
                send("{\"t\":\"move\"," + xy(ev.getX(), ev.getY()) + "}");
                send("{\"t\":\"button\",\"b\":" + code + ",\"down\":" + (ev.getActionMasked() == MotionEvent.ACTION_BUTTON_PRESS) + "}");
                return true;
            }
            case MotionEvent.ACTION_SCROLL: {
                float v = ev.getAxisValue(MotionEvent.AXIS_VSCROLL), h = ev.getAxisValue(MotionEvent.AXIS_HSCROLL);
                send("{\"t\":\"wheel\",\"dy\":" + (int) -Math.signum(v) + ",\"dx\":" + (int) Math.signum(h) + "}");
                return true;
            }
            default:
                return false;
        }
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent ev) {
        int code = ev.getKeyCode();
        if (code == KeyEvent.KEYCODE_BACK || code == KeyEvent.KEYCODE_VOLUME_UP
                || code == KeyEvent.KEYCODE_VOLUME_DOWN || code == KeyEvent.KEYCODE_VOLUME_MUTE) {
            return super.dispatchKeyEvent(ev);
        }
        // Teclado en pantalla: lo maneja el campo invisible.
        if (kbd.hasFocus() && ev.getDevice() != null && ev.getDevice().isVirtual()) return super.dispatchKeyEvent(ev);
        if (ev.getAction() == KeyEvent.ACTION_MULTIPLE) return super.dispatchKeyEvent(ev);
        int sym = keysym(ev);
        if (sym == 0) return super.dispatchKeyEvent(ev);
        boolean down = ev.getAction() == KeyEvent.ACTION_DOWN;
        if (down && ev.getRepeatCount() > 0 && isModifier(code)) return true;
        send("{\"t\":\"key\",\"keysym\":" + sym + ",\"down\":" + down + "}");
        return true;
    }

    private static boolean isModifier(int code) {
        return KeyEvent.isModifierKey(code);
    }

    private void tapKey(int sym) {
        if (sym == 0) return;
        send("{\"t\":\"key\",\"keysym\":" + sym + ",\"down\":true}");
        send("{\"t\":\"key\",\"keysym\":" + sym + ",\"down\":false}");
    }

    private static int keysymOfCodePoint(int cp) {
        return cp < 0x100 ? cp : 0x01000000 + cp;
    }

    /** Teclas especiales → keysym de X11 (las mismas que el visor web). */
    private static int keysym(KeyEvent ev) {
        switch (ev.getKeyCode()) {
            case KeyEvent.KEYCODE_ENTER: case KeyEvent.KEYCODE_NUMPAD_ENTER: return 0xff0d;
            case KeyEvent.KEYCODE_DEL: return 0xff08;
            case KeyEvent.KEYCODE_TAB: return 0xff09;
            case KeyEvent.KEYCODE_ESCAPE: return 0xff1b;
            case KeyEvent.KEYCODE_FORWARD_DEL: return 0xffff;
            case KeyEvent.KEYCODE_MOVE_HOME: return 0xff50;
            case KeyEvent.KEYCODE_MOVE_END: return 0xff57;
            case KeyEvent.KEYCODE_PAGE_UP: return 0xff55;
            case KeyEvent.KEYCODE_PAGE_DOWN: return 0xff56;
            case KeyEvent.KEYCODE_DPAD_LEFT: return 0xff51;
            case KeyEvent.KEYCODE_DPAD_UP: return 0xff52;
            case KeyEvent.KEYCODE_DPAD_RIGHT: return 0xff53;
            case KeyEvent.KEYCODE_DPAD_DOWN: return 0xff54;
            case KeyEvent.KEYCODE_SHIFT_LEFT: case KeyEvent.KEYCODE_SHIFT_RIGHT: return 0xffe1;
            case KeyEvent.KEYCODE_CTRL_LEFT: case KeyEvent.KEYCODE_CTRL_RIGHT: return 0xffe3;
            case KeyEvent.KEYCODE_ALT_LEFT: case KeyEvent.KEYCODE_ALT_RIGHT: return 0xffe9;
            case KeyEvent.KEYCODE_META_LEFT: case KeyEvent.KEYCODE_META_RIGHT: return 0xffeb;
            case KeyEvent.KEYCODE_CAPS_LOCK: return 0xffe5;
            case KeyEvent.KEYCODE_INSERT: return 0xff63;
            default:
                if (ev.getKeyCode() >= KeyEvent.KEYCODE_F1 && ev.getKeyCode() <= KeyEvent.KEYCODE_F12) {
                    return 0xffbe + ev.getKeyCode() - KeyEvent.KEYCODE_F1;
                }
                // Carácter con mayúsculas aplicadas, sin Ctrl/Alt (el PC ya
                // recibe esas teclas por separado).
                int cp = ev.getUnicodeChar(ev.getMetaState() & (KeyEvent.META_SHIFT_MASK | KeyEvent.META_CAPS_LOCK_ON));
                return cp > 0 ? keysymOfCodePoint(cp) : 0;
        }
    }

    // ── Menú ────────────────────────────────────────────────────────────────

    private void showMenu(View anchor) {
        PopupMenu pm = new PopupMenu(this, anchor);
        Menu m = pm.getMenu();
        m.add(0, 1, 0, "Teclado");
        String scale = prefs.getString("scale", "1");
        String[][] scales = {{"1", "Resolución nativa"}, {"0.75", "Resolución 75 %"}, {"0.5", "Resolución 50 %"}};
        for (int i = 0; i < scales.length; i++) {
            m.add(1, 10 + i, 1, (scales[i][0].equals(scale) ? "✓ " : "   ") + scales[i][1]);
        }
        m.add(2, 2, 2, "Reconectar");
        m.add(2, 3, 2, "Usar el visor web");
        m.add(3, 4, 3, lastStats).setEnabled(false);
        pm.setOnMenuItemClickListener(item -> {
            int id = item.getItemId();
            if (id == 1) {
                kbd.requestFocus();
                InputMethodManager imm = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                imm.showSoftInput(kbd, InputMethodManager.SHOW_IMPLICIT);
            } else if (id >= 10 && id < 10 + scales.length) {
                prefs.edit().putString("scale", scales[id - 10][0]).apply();
                sendSize();
            } else if (id == 2) {
                WebSocketClient ws = videoWs;
                if (ws != null) ws.close();
            } else if (id == 3) {
                setResult(RESULT_WEB, new Intent().putExtra(EXTRA_HOST, host).putExtra(EXTRA_PORT, port));
                finish();
            }
            return true;
        });
        pm.show();
    }

    // ── Ciclo de vida ───────────────────────────────────────────────────────

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) MainActivity.enterImmersive(this);
    }

    @Override
    protected void onDestroy() {
        running = false;
        WebSocketClient v = videoWs, i = inputWs;
        if (v != null) v.close();
        if (i != null) i.close();
        codec.post(this::releaseDecoder);
        codecThread.quitSafely();
        sender.shutdownNow();
        super.onDestroy();
    }

    private static void sleep(long ms) {
        try { Thread.sleep(ms); } catch (InterruptedException ignored) {}
    }
}
