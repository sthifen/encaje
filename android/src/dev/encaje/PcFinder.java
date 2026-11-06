package dev.encaje;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

/**
 * Busca PCs con Encaje en la red de la tablet, para no tener que escribir
 * ninguna dirección: prueba el puerto del servidor en cada equipo de la
 * subred. Usa el mismo puerto TCP que el visor, así que si el visor pasa el
 * cortafuegos, la búsqueda también (y no depende de multicast, que muchos
 * routers filtran).
 */
final class PcFinder {
    private static final int THREADS = 32;
    private static final int CONNECT_MS = 350, READ_MS = 800;

    private final Map<String, JSONObject> found = Collections.synchronizedMap(new LinkedHashMap<>());
    private volatile boolean scanning;

    /** Empieza una pasada, salvo que ya haya una en marcha. */
    synchronized void start(int port) {
        if (scanning) return;
        scanning = true;
        new Thread(() -> {
            try {
                scan(port);
            } finally {
                scanning = false;
            }
        }, "encaje-buscar").start();
    }

    /** {@code {"scanning":bool,"pcs":[{host,port,name,id,width,height}…]}} */
    String state() {
        JSONArray pcs = new JSONArray();
        synchronized (found) {
            for (JSONObject pc : found.values()) pcs.put(pc);
        }
        try {
            return new JSONObject().put("scanning", scanning).put("pcs", pcs).toString();
        } catch (JSONException e) {
            return "{\"scanning\":false,\"pcs\":[]}";
        }
    }

    private void scan(int port) {
        List<String> hosts = candidates();
        Set<String> alive = Collections.synchronizedSet(new LinkedHashSet<>());
        ExecutorService pool = Executors.newFixedThreadPool(THREADS);
        for (String host : hosts) {
            pool.execute(() -> {
                JSONObject pc = probe(host, port);
                if (pc != null) {
                    alive.add(host);
                    found.put(host, pc);
                }
            });
        }
        pool.shutdown();
        try {
            pool.awaitTermination(30, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            pool.shutdownNow();
        }
        // Los que dejaron de responder (el PC se apagó) salen de la lista.
        found.keySet().retainAll(alive);
    }

    /** Las direcciones de la /24 de cada red local, sin la propia. */
    private static List<String> candidates() {
        List<String> out = new ArrayList<>();
        try {
            for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                if (!ni.isUp() || ni.isLoopback()) continue;
                for (InterfaceAddress ia : ni.getInterfaceAddresses()) {
                    InetAddress a = ia.getAddress();
                    if (!(a instanceof Inet4Address) || !a.isSiteLocalAddress()) continue;
                    byte[] b = a.getAddress();
                    String prefix = (b[0] & 0xff) + "." + (b[1] & 0xff) + "." + (b[2] & 0xff) + ".";
                    int self = b[3] & 0xff;
                    for (int i = 1; i < 255; i++) {
                        if (i != self) out.add(prefix + i);
                    }
                }
            }
        } catch (IOException e) {
            android.util.Log.w("Encaje", "buscar PCs: " + e);
        }
        return out;
    }

    /** Pide /info; {@code null} si ahí no hay un servidor de Encaje. */
    private static JSONObject probe(String host, int port) {
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress(host, port), CONNECT_MS);
            s.setSoTimeout(READ_MS);
            OutputStream out = s.getOutputStream();
            out.write(("GET /info HTTP/1.0\r\nHost: " + host + "\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
            out.flush();
            InputStream in = s.getInputStream();
            ByteArrayOutputStream body = new ByteArrayOutputStream();
            byte[] buf = new byte[1024];
            for (int n; body.size() < 8192 && (n = in.read(buf)) > 0; ) body.write(buf, 0, n);
            String text = new String(body.toByteArray(), StandardCharsets.UTF_8);
            int start = text.indexOf("\r\n\r\n");
            if (start < 0) return null;
            JSONObject info = new JSONObject(text.substring(start + 4));
            if (!info.has("auto_size")) return null; // otro servicio en ese puerto
            return new JSONObject()
                    .put("host", host)
                    .put("port", port)
                    .put("name", info.optString("name", host))
                    .put("id", info.optString("id", host))
                    .put("width", info.optInt("width"))
                    .put("height", info.optInt("height"));
        } catch (IOException | JSONException e) {
            return null;
        }
    }
}
