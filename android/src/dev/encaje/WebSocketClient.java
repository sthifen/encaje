package dev.encaje;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.Closeable;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.security.SecureRandom;
import android.util.Base64;

/**
 * Cliente WebSocket mínimo (RFC 6455) para el visor nativo: sin
 * dependencias, sin Nagle y con lecturas bloqueantes desde un hilo propio.
 * Sólo lo que usa el servidor de Encaje: texto, binario, ping y cierre.
 */
final class WebSocketClient implements Closeable {
    static final int TEXT = 1, BINARY = 2;

    /** Un mensaje recibido. {@code data} es nuevo en cada mensaje. */
    static final class Message {
        final int opcode;
        final byte[] data;
        Message(int opcode, byte[] data) { this.opcode = opcode; this.data = data; }
        String text() { return new String(data, StandardCharsets.UTF_8); }
    }

    private final Socket socket;
    private final DataInputStream in;
    private final OutputStream out;

    private WebSocketClient(Socket socket, DataInputStream in, OutputStream out) {
        this.socket = socket;
        this.in = in;
        this.out = out;
    }

    static WebSocketClient connect(String host, int port, String path, int timeoutMs) throws IOException {
        Socket s = new Socket();
        s.setTcpNoDelay(true);
        s.connect(new InetSocketAddress(host, port), timeoutMs);
        s.setSoTimeout(timeoutMs);
        byte[] nonce = new byte[16];
        new SecureRandom().nextBytes(nonce);
        String key = Base64.encodeToString(nonce, Base64.NO_WRAP);
        OutputStream out = s.getOutputStream();
        out.write(("GET " + path + " HTTP/1.1\r\nHost: " + host + ":" + port + "\r\n"
                + "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + key + "\r\n"
                + "Sec-WebSocket-Version: 13\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
        out.flush();
        DataInputStream in = new DataInputStream(new BufferedInputStream(s.getInputStream(), 1 << 16));
        // Cabeceras de la respuesta hasta la línea vacía.
        StringBuilder status = new StringBuilder();
        int prev3 = 0, prev2 = 0, prev1 = 0, c;
        boolean firstLine = true;
        while ((c = in.read()) != -1) {
            if (firstLine) {
                if (c == '\r') firstLine = false; else status.append((char) c);
            }
            if (prev3 == '\r' && prev2 == '\n' && prev1 == '\r' && c == '\n') break;
            prev3 = prev2; prev2 = prev1; prev1 = c;
        }
        if (!status.toString().contains(" 101")) {
            s.close();
            throw new IOException("respuesta inesperada: " + status);
        }
        s.setSoTimeout(0);
        // Con búfer: cabecera y datos salen juntos en un solo segmento TCP.
        return new WebSocketClient(s, in, new BufferedOutputStream(out, 1 << 14));
    }

    /** Siguiente mensaje de datos (responde a los ping). Lanza al cerrarse. */
    Message read() throws IOException {
        while (true) {
            int b0 = in.readUnsignedByte(), b1 = in.readUnsignedByte();
            long n = b1 & 0x7f;
            if (n == 126) n = in.readUnsignedShort();
            else if (n == 127) n = in.readLong();
            if (n < 0 || n > 64L << 20) throw new IOException("mensaje demasiado grande: " + n);
            byte[] mask = null;
            if ((b1 & 0x80) != 0) { mask = new byte[4]; in.readFully(mask); }
            byte[] data = new byte[(int) n];
            in.readFully(data);
            if (mask != null) for (int i = 0; i < data.length; i++) data[i] ^= mask[i & 3];
            int op = b0 & 0x0f;
            if (op == 0x9) { send(0xA, data); continue; }
            if (op == 0x8) throw new IOException("cerrado por el servidor");
            if (op == TEXT || op == BINARY) return new Message(op, data);
        }
    }

    void sendText(String s) throws IOException {
        send(TEXT, s.getBytes(StandardCharsets.UTF_8));
    }

    /**
     * Los clientes deben enmascarar; con la máscara a cero el XOR no cambia
     * nada y no hay que recorrer el mensaje (el servidor es de confianza:
     * la máscara sólo protege a proxies HTTP intermedios).
     */
    private synchronized void send(int opcode, byte[] payload) throws IOException {
        int n = payload.length;
        byte[] head;
        if (n < 126) {
            head = new byte[] {(byte) (0x80 | opcode), (byte) (0x80 | n), 0, 0, 0, 0};
        } else if (n < 65536) {
            head = new byte[] {(byte) (0x80 | opcode), (byte) (0x80 | 126), (byte) (n >> 8), (byte) n, 0, 0, 0, 0};
        } else {
            head = new byte[14];
            head[0] = (byte) (0x80 | opcode);
            head[1] = (byte) (0x80 | 127);
            for (int i = 0; i < 8; i++) head[2 + i] = (byte) ((long) n >> (56 - 8 * i));
        }
        out.write(head);
        out.write(payload);
        out.flush();
    }

    @Override
    public void close() {
        try { socket.close(); } catch (IOException ignored) {}
    }
}
