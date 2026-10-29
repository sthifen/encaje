package dev.encaje;

/** Ancho y alto (recortados) de un SPS de H.264, para configurar MediaCodec. */
final class SpsSize {
    private final byte[] d;
    private int pos = 8; // tras la cabecera NAL

    private SpsSize(byte[] rbsp) { d = rbsp; }

    private int bit() {
        if (pos / 8 >= d.length) throw new IllegalArgumentException("SPS truncado");
        int b = (d[pos / 8] >> (7 - pos % 8)) & 1;
        pos++;
        return b;
    }

    private int bits(int n) {
        int v = 0;
        for (int i = 0; i < n; i++) v = (v << 1) | bit();
        return v;
    }

    private int ue() {
        int z = 0;
        while (bit() == 0) if (++z > 31) throw new IllegalArgumentException("exp-Golomb");
        return (1 << z) - 1 + bits(z);
    }

    private void se() { ue(); }

    /** {ancho, alto}, o null si el SPS no se entiende. {@code nal} incluye la cabecera NAL. */
    static int[] parse(byte[] nal) {
        // Quitar los bytes de prevención de emulación (00 00 03).
        byte[] r = new byte[nal.length];
        int n = 0, zeros = 0;
        for (byte b : nal) {
            if (zeros >= 2 && b == 3) { zeros = 0; continue; }
            zeros = b == 0 ? zeros + 1 : 0;
            r[n++] = b;
        }
        try {
            return new SpsSize(java.util.Arrays.copyOf(r, n)).size();
        } catch (RuntimeException e) {
            return null;
        }
    }

    private int[] size() {
        int profile = bits(8);
        bits(16);
        ue();
        int chroma = 1;
        if (profile == 100 || profile == 110 || profile == 122 || profile == 244 || profile == 44
                || profile == 83 || profile == 86 || profile == 118 || profile == 128 || profile == 138
                || profile == 139 || profile == 134 || profile == 135) {
            chroma = ue();
            if (chroma == 3) bit();
            ue();
            ue();
            bit();
            if (bit() == 1) {
                for (int i = 0; i < (chroma == 3 ? 12 : 8); i++) {
                    if (bit() == 1) {
                        int last = 8, next = 8, size = i < 6 ? 16 : 64;
                        for (int j = 0; j < size; j++) {
                            if (next != 0) {
                                int k = ue();
                                int delta = (k % 2 == 1) ? (k + 1) / 2 : -(k / 2);
                                next = (last + delta + 256) % 256;
                            }
                            last = next == 0 ? last : next;
                        }
                    }
                }
            }
        }
        ue();
        int poc = ue();
        if (poc == 0) {
            ue();
        } else if (poc == 1) {
            bit();
            se();
            se();
            int c = ue();
            for (int i = 0; i < c; i++) se();
        }
        ue();
        bit();
        int wMbs = ue() + 1, hMap = ue() + 1;
        int frameMbsOnly = bit();
        if (frameMbsOnly == 0) bit();
        bit();
        int w = wMbs * 16, h = (2 - frameMbsOnly) * hMap * 16;
        if (bit() == 1) {
            int l = ue(), rr = ue(), t = ue(), b = ue();
            int cx = chroma == 0 || chroma == 3 ? 1 : 2;
            int cy = (chroma == 1 ? 2 : 1) * (2 - frameMbsOnly);
            w -= (l + rr) * cx;
            h -= (t + b) * cy;
        }
        return new int[] {w, h};
    }
}
