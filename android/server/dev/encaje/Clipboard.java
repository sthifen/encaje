package dev.encaje;

import android.content.ClipData;
import android.content.ClipDescription;
import android.net.Uri;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Method;

/**
 * Portapapeles de Android desde el proceso de shell, con la API interna
 * (IClipboard), como hace scrcpy: shell puede leerlo en segundo plano.
 *
 * Las imágenes viajan como URI content://. Las de Android se leen con
 * `content read` (al pedir el portapapeles, Android concede a shell permiso
 * sobre sus URI). Las del PC se guardan en el proveedor de la app
 * (ClipProvider, sólo shell puede escribir en él) y se pone su URI.
 */
final class Clipboard {
    interface Listener {
        void changed();
    }

    static final int TEXT = 0;
    static final int PNG = 1;

    /** Contenido del portapapeles: tipo y bytes (UTF-8 o PNG). */
    static final class Content {
        final int kind;
        final byte[] data;

        Content(int kind, byte[] data) {
            this.kind = kind;
            this.data = data;
        }
    }

    private static final String PACKAGE = "com.android.shell";
    private static final String PROVIDER = "content://dev.encaje.clip/";
    private static final int MAX_IMAGE = 32 << 20;

    private final Object service;
    private final Method get;
    private final Method set;
    private final Method addListener;
    /** URI que pusimos nosotros (para no devolverle al PC su propia imagen). */
    private String ownUri;
    private int serial;

    Clipboard() throws Exception {
        IBinder b = (IBinder) Class.forName("android.os.ServiceManager")
            .getMethod("getService", String.class).invoke(null, "clipboard");
        service = Class.forName("android.content.IClipboard$Stub")
            .getMethod("asInterface", IBinder.class).invoke(null, b);
        Method g = null, s = null, l = null;
        for (Method m : service.getClass().getMethods()) {
            switch (m.getName()) {
                case "getPrimaryClip": g = m; break;
                case "setPrimaryClip": s = m; break;
                case "addPrimaryClipChangedListener": l = m; break;
                default:
            }
        }
        if (g == null || s == null) throw new Exception("IClipboard sin getPrimaryClip/setPrimaryClip");
        get = g;
        set = s;
        addListener = l;
    }

    /**
     * Paquete y usuario según la versión de Android:
     * (pkg) · (pkg, user) · (pkg, tag, user) · (pkg, tag, user, device).
     */
    private static Object[] pkgArgs(int count) {
        Object[] a = new Object[count];
        a[0] = PACKAGE;
        if (count == 2) a[1] = 0;
        if (count >= 3) a[2] = 0;
        if (count >= 4) a[3] = 0;
        return a;
    }

    /** Un primer argumento y después paquete y usuario. */
    private static Object[] args(Object first, int count) {
        Object[] a = new Object[count];
        a[0] = first;
        System.arraycopy(pkgArgs(count - 1), 0, a, 1, count - 1);
        return a;
    }

    void listen(Listener listener) throws Exception {
        if (addListener == null) throw new Exception("IClipboard sin addPrimaryClipChangedListener");
        // IOnPrimaryClipChangedListener es oneway y tiene un solo método:
        // basta un Binder que responda a cualquier transacción.
        Binder binder = new Binder() {
            @Override
            protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
                if (code >= FIRST_CALL_TRANSACTION && code <= LAST_CALL_TRANSACTION) {
                    listener.changed();
                    return true;
                }
                return false;
            }
        };
        Object proxy = Class.forName("android.content.IOnPrimaryClipChangedListener$Stub")
            .getMethod("asInterface", IBinder.class).invoke(null, binder);
        addListener.invoke(service, args(proxy, addListener.getParameterCount()));
    }

    /** Lo que hay ahora en el portapapeles, o null si está vacío o es nuestro. */
    Content read() throws Exception {
        ClipData clip = (ClipData) get.invoke(service, pkgArgs(get.getParameterCount()));
        if (clip == null || clip.getItemCount() == 0) return null;
        ClipData.Item item = clip.getItemAt(0);
        Uri uri = item.getUri();
        ClipDescription desc = clip.getDescription();
        if (uri != null && desc != null && desc.getMimeTypeCount() > 0 && desc.getMimeType(0).startsWith("image/")) {
            if (uri.toString().equals(ownUri)) return null;
            byte[] data = readUri(uri.toString());
            if (data != null && data.length > 0) return new Content(PNG, data);
        }
        CharSequence text = item.getText();
        if (text == null && item.getHtmlText() != null) text = item.getHtmlText();
        if (text == null) return null;
        return new Content(TEXT, text.toString().getBytes("UTF-8"));
    }

    void writeText(String text) throws Exception {
        ClipData clip = ClipData.newPlainText("Encaje", text);
        set.invoke(service, args(clip, set.getParameterCount()));
    }

    /** Guarda la imagen en el proveedor de la app y pone su URI. */
    void writeImage(byte[] png) throws Exception {
        String name = "clip-" + (++serial) + "-" + System.currentTimeMillis() + ".png";
        String uri = PROVIDER + name;
        Process p = Runtime.getRuntime().exec(new String[] {"content", "write", "--uri", uri});
        try (OutputStream out = p.getOutputStream()) {
            out.write(png);
        }
        String err = drain(p.getErrorStream());
        if (p.waitFor() != 0 || err.contains("Exception")) {
            throw new Exception("no se pudo guardar la imagen (¿está instalada la app actualizada?): " + err.trim());
        }
        ClipDescription desc = new ClipDescription("Encaje", new String[] {"image/png"});
        ClipData clip = new ClipData(desc, new ClipData.Item(Uri.parse(uri)));
        ownUri = uri;
        set.invoke(service, args(clip, set.getParameterCount()));
    }

    private static byte[] readUri(String uri) throws Exception {
        Process p = Runtime.getRuntime().exec(new String[] {"content", "read", "--uri", uri});
        p.getOutputStream().close();
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        try (InputStream in = p.getInputStream()) {
            byte[] chunk = new byte[65536];
            int n;
            while ((n = in.read(chunk)) > 0) {
                buf.write(chunk, 0, n);
                if (buf.size() > MAX_IMAGE) {
                    p.destroy();
                    return null;
                }
            }
        }
        p.waitFor();
        byte[] data = buf.toByteArray();
        // `content read` escribe los errores en stdout en algunas versiones.
        boolean png = data.length > 8 && (data[0] & 0xFF) == 0x89 && data[1] == 'P';
        boolean jpeg = data.length > 3 && (data[0] & 0xFF) == 0xFF && (data[1] & 0xFF) == 0xD8;
        boolean webp = data.length > 12 && data[8] == 'W' && data[9] == 'E';
        return png || jpeg || webp ? data : null;
    }

    private static String drain(InputStream in) throws Exception {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        byte[] chunk = new byte[4096];
        int n;
        while ((n = in.read(chunk)) > 0) buf.write(chunk, 0, n);
        return buf.toString("UTF-8");
    }
}
