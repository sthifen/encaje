package dev.encaje;

import android.annotation.SuppressLint;
import android.app.Activity;
import android.content.Intent;
import android.content.SharedPreferences;
import android.webkit.JavascriptInterface;
import android.graphics.Color;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.view.Display;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;

/**
 * Visor a pantalla completa. Toda la lógica de vídeo vive en el visor web que
 * sirve el PC; la app aporta pantalla completa inmersiva, pantalla siempre
 * encendida y un lanzador que detecta el PC por USB (adb reverse → localhost)
 * o por WiFi.
 */
public class MainActivity extends Activity {
    private static final String LAUNCHER = "file:///android_asset/launcher.html";
    /** Android 5: primera versión con un WebView actualizable. */
    static final int WEB_VIEWER_MIN_SDK = Build.VERSION_CODES.LOLLIPOP;
    private final PcFinder finder = new PcFinder();
    private WebView web;

    @SuppressLint("SetJavaScriptEnabled")
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            getWindow().getAttributes().layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        }

        preferHighestRefreshRate(this);

        web = new WebView(this);
        web.setBackgroundColor(Color.BLACK);
        WebSettings s = web.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setAllowFileAccess(true);
        s.setMediaPlaybackRequiresUserGesture(false);
        s.setCacheMode(WebSettings.LOAD_NO_CACHE);
        s.setSupportZoom(false);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            // El proceso que decodifica y dibuja el vídeo, con prioridad alta
            // aunque Android ande justo de memoria.
            web.setRendererPriorityPolicy(WebView.RENDERER_PRIORITY_IMPORTANT, false);
        }
        web.setWebViewClient(new WebViewClient() {
            @Override
            public void onReceivedError(WebView view, WebResourceRequest req, WebResourceError err) {
                // Si el PC no responde, volver al lanzador, que reintenta solo.
                if (req.isForMainFrame() && !req.getUrl().toString().startsWith("file:")) {
                    view.loadUrl(LAUNCHER + "#error");
                }
            }

            /** Lo mismo antes de Android 6, que sólo avisa de la página principal. */
            @SuppressWarnings("deprecation")
            @Override
            public void onReceivedError(WebView view, int code, String description, String url) {
                if (url != null && !url.startsWith("file:")) view.loadUrl(LAUNCHER + "#error");
            }
        });
        web.addJavascriptInterface(new NativeBridge(), "EncajeNative");
        setContentView(web);

        if (state != null) {
            web.restoreState(state);
        } else {
            web.loadUrl(LAUNCHER);
        }
    }

    /**
     * Pide el modo de pantalla con más hercios a la resolución actual. Sin
     * esto, muchas tablets (Samsung con "fluidez adaptable", por ejemplo)
     * dejan las apps WebView a 60 Hz aunque la pantalla dé 90 o 120; el visor
     * mide la frecuencia y el PC transmite a ese ritmo.
     */
    @SuppressWarnings("deprecation")
    static void preferHighestRefreshRate(Activity a) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return;
        Display d = a.getWindowManager().getDefaultDisplay();
        Display.Mode best = d.getMode();
        for (Display.Mode m : d.getSupportedModes()) {
            if (m.getPhysicalWidth() == best.getPhysicalWidth()
                    && m.getPhysicalHeight() == best.getPhysicalHeight()
                    && m.getRefreshRate() > best.getRefreshRate()) {
                best = m;
            }
        }
        WindowManager.LayoutParams lp = a.getWindow().getAttributes();
        lp.preferredDisplayModeId = best.getModeId();
        lp.preferredRefreshRate = best.getRefreshRate();
        a.getWindow().setAttributes(lp);
    }

    /**
     * El lanzador (launcher.html) pregunta por aquí si abrir el visor nativo
     * (por defecto) o el web. La preferencia se guarda en la app.
     */
    private final class NativeBridge {
        private SharedPreferences prefs() {
            return getSharedPreferences("encaje", MODE_PRIVATE);
        }

        /** El visor web necesita WebCodecs o WebRTC: sin ellos, sólo el nativo. */
        @JavascriptInterface
        public boolean webViewerWorks() {
            return Build.VERSION.SDK_INT >= WEB_VIEWER_MIN_SDK;
        }

        /** {@code adb reverse} no existe antes de Android 5. */
        @JavascriptInterface
        public boolean usbWorks() {
            return Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP;
        }

        @JavascriptInterface
        public boolean isEnabled() {
            return !webViewerWorks() || prefs().getBoolean("native", true);
        }

        /** Para el visor web: quién soy ante el PC. */
        @JavascriptInterface
        public String clientId() {
            return MainActivity.clientId(prefs());
        }

        @JavascriptInterface
        public String deviceName() {
            return Build.MODEL;
        }

        /** Para el visor web: el monitor lo usa otra tablet; se avisa en el lanzador. */
        @JavascriptInterface
        public void taken(String kind, String by, boolean extra) {
            runOnUiThread(() -> {
                Uri page = Uri.parse(web.getUrl());
                showTaken(kind, by, page.getHost(), page.getPort(), extra);
            });
        }

        /** Para el visor web: lo mismo que "Desconectar" en el nativo. */
        @JavascriptInterface
        public void disconnect() {
            prefs().edit().remove("last_pc").apply();
            runOnUiThread(() -> web.loadUrl(LAUNCHER + "#manual"));
        }

        /** Último PC con el que se conectó ("" si ninguno). Por su id: su IP cambia. */
        @JavascriptInterface
        public String lastPc() {
            return prefs().getString("last_pc", "");
        }

        @JavascriptInterface
        public void rememberPc(String id) {
            prefs().edit().putString("last_pc", id).apply();
        }

        @JavascriptInterface
        public void scan(int port) {
            finder.start(port);
        }

        @JavascriptInterface
        public String scanState() {
            return finder.state();
        }

        @JavascriptInterface
        public void setEnabled(boolean on) {
            prefs().edit().putBoolean("native", on).apply();
        }

        /**
         * Abre el visor nativo; {@code false} si se prefiere el web.
         * {@code mode}: "auto", "ask" (lo eligió la persona: si está ocupado,
         * se le pregunta), "pick" (quitárselo a quien lo use) o "extra" (otro monitor).
         */
        @JavascriptInterface
        public boolean open(String host, int port, String mode) {
            if (!isEnabled()) return false;
            runOnUiThread(() -> startActivityForResult(new Intent(MainActivity.this, NativeViewerActivity.class)
                    .putExtra(NativeViewerActivity.EXTRA_HOST, host)
                    .putExtra(NativeViewerActivity.EXTRA_PORT, port)
                    .putExtra(NativeViewerActivity.EXTRA_MODE, mode)
                    .putExtra(NativeViewerActivity.EXTRA_SYNC_DECODER,
                            getIntent().getBooleanExtra(NativeViewerActivity.EXTRA_SYNC_DECODER, false)),
                    REQUEST_NATIVE));
            return true;
        }
    }

    private static final int REQUEST_NATIVE = 1;

    private void showTaken(String kind, String by, String host, int port, boolean extra) {
        web.loadUrl(LAUNCHER + "#" + kind + "|" + Uri.encode(by) + "|" + host + "|" + port + "|" + (extra ? 1 : 0));
    }

    /** Identifica a esta tablet ante el PC. */
    static String clientId(SharedPreferences prefs) {
        String id = prefs.getString("client_id", null);
        if (id == null) {
            id = java.util.UUID.randomUUID().toString();
            prefs.edit().putString("client_id", id).apply();
        }
        return id;
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != REQUEST_NATIVE) return;
        if (result == NativeViewerActivity.RESULT_WEB && data != null) {
            // "Usar el visor web" desde el menú del nativo: sólo esta vez.
            web.loadUrl("http://" + data.getStringExtra(NativeViewerActivity.EXTRA_HOST) + ":"
                    + data.getIntExtra(NativeViewerActivity.EXTRA_PORT, 8787) + "/");
        } else if (result == NativeViewerActivity.RESULT_TAKEN && data != null) {
            showTaken(data.getStringExtra(NativeViewerActivity.EXTRA_TAKEN),
                    data.getStringExtra(NativeViewerActivity.EXTRA_BY),
                    data.getStringExtra(NativeViewerActivity.EXTRA_HOST),
                    data.getIntExtra(NativeViewerActivity.EXTRA_PORT, 8787),
                    data.getBooleanExtra(NativeViewerActivity.EXTRA_CAN_EXTEND, false));
        } else {
            web.loadUrl(LAUNCHER + "#manual"); // volvió con "atrás": no reconectar solo
        }
    }

    @Override
    protected void onSaveInstanceState(Bundle out) {
        super.onSaveInstanceState(out);
        web.saveState(out);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) enterImmersive(this);
    }

    @SuppressWarnings("deprecation")
    static void enterImmersive(Activity a) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            a.getWindow().setDecorFitsSystemWindows(false);
            WindowInsetsController c = a.getWindow().getInsetsController();
            if (c != null) {
                c.hide(WindowInsets.Type.systemBars());
                c.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
        } else {
            a.getWindow().getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                    | View.SYSTEM_UI_FLAG_FULLSCREEN
                    | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                    | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                    | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                    | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN);
        }
    }

    @SuppressWarnings("deprecation")
    @Override
    public void onBackPressed() {
        String url = web.getUrl();
        if (url != null && !url.startsWith(LAUNCHER)) {
            web.loadUrl(LAUNCHER + "#manual");
        } else {
            super.onBackPressed();
        }
    }

    @Override
    protected void onPause() {
        super.onPause();
        web.onPause();
    }

    @Override
    protected void onResume() {
        super.onResume();
        web.onResume();
    }

    @Override
    protected void onDestroy() {
        web.destroy();
        super.onDestroy();
    }
}
