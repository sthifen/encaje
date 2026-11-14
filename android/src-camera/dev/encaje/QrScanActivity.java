package dev.encaje;

import android.Manifest;
import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.ImageFormat;
import android.graphics.Matrix;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.SurfaceTexture;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Size;
import android.view.Gravity;
import android.view.Surface;
import android.view.TextureView;
import android.view.View;
import android.widget.FrameLayout;
import android.widget.TextView;

import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.EnumMap;
import java.util.Map;

import com.google.zxing.BarcodeFormat;
import com.google.zxing.BinaryBitmap;
import com.google.zxing.DecodeHintType;
import com.google.zxing.PlanarYUVLuminanceSource;
import com.google.zxing.ReaderException;
import com.google.zxing.Result;
import com.google.zxing.common.HybridBinarizer;
import com.google.zxing.qrcode.QRCodeReader;

/**
 * Cámara en vivo que busca el QR que enseña Encaje en el PC
 * («encaje://connect?…») y lo devuelve en el extra "link". Necesita Camera2
 * (Android 5): el lanzador no ofrece el botón antes.
 *
 * Lee el QR con ZXing (Apache-2.0).
 */
public final class QrScanActivity extends Activity {
    static final String EXTRA_LINK = "link";
    private static final int ASK_CAMERA = 1;

    private TextureView preview;
    private TextView hint;
    private HandlerThread thread;
    private Handler bg;
    private CameraDevice camera;
    private CameraCaptureSession session;
    private ImageReader reader;
    private Size size;
    private int sensorDegrees;
    private final QRCodeReader qr = new QRCodeReader();
    private final Map<DecodeHintType, Object> hints = new EnumMap<>(DecodeHintType.class);
    private volatile boolean done;
    private long lastWrong;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        hints.put(DecodeHintType.POSSIBLE_FORMATS, Arrays.asList(BarcodeFormat.QR_CODE));
        hints.put(DecodeHintType.TRY_HARDER, Boolean.TRUE);

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        preview = new TextureView(this);
        root.addView(preview, new FrameLayout.LayoutParams(-1, -1));
        root.addView(new Frame(this), new FrameLayout.LayoutParams(-1, -1));

        float d = getResources().getDisplayMetrics().density;
        hint = new TextView(this);
        hint.setText("Apunta al QR que sale en el PC");
        hint.setTextColor(Color.WHITE);
        hint.setTextSize(18);
        hint.setGravity(Gravity.CENTER);
        hint.setShadowLayer(6 * d, 0, 0, Color.BLACK);
        FrameLayout.LayoutParams hp = new FrameLayout.LayoutParams(-1, -2, Gravity.TOP);
        hp.topMargin = (int) (48 * d);
        root.addView(hint, hp);

        TextView close = new TextView(this);
        close.setText("Cancelar");
        close.setTextColor(Color.WHITE);
        close.setTextSize(17);
        close.setPadding((int) (28 * d), (int) (14 * d), (int) (28 * d), (int) (14 * d));
        android.graphics.drawable.GradientDrawable bgShape = new android.graphics.drawable.GradientDrawable();
        bgShape.setColor(0x99000000);
        bgShape.setCornerRadius(28 * d);
        close.setBackground(bgShape);
        close.setOnClickListener(v -> finish());
        FrameLayout.LayoutParams cp = new FrameLayout.LayoutParams(-2, -2, Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL);
        cp.bottomMargin = (int) (40 * d);
        root.addView(close, cp);
        setContentView(root);

        preview.setSurfaceTextureListener(new TextureView.SurfaceTextureListener() {
            @Override
            public void onSurfaceTextureAvailable(SurfaceTexture st, int w, int h) {
                start();
            }

            @Override
            public void onSurfaceTextureSizeChanged(SurfaceTexture st, int w, int h) {
                transform();
            }

            @Override
            public boolean onSurfaceTextureDestroyed(SurfaceTexture st) {
                return true;
            }

            @Override
            public void onSurfaceTextureUpdated(SurfaceTexture st) {}
        });
    }

    @Override
    protected void onResume() {
        super.onResume();
        thread = new HandlerThread("qr");
        thread.start();
        bg = new Handler(thread.getLooper());
        if (!cameraAllowed()) {
            requestPermissions(new String[] {Manifest.permission.CAMERA}, ASK_CAMERA);
        } else if (preview.isAvailable()) {
            start();
        }
    }

    @Override
    protected void onPause() {
        stop();
        if (thread != null) thread.quitSafely();
        thread = null;
        bg = null;
        super.onPause();
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] results) {
        if (code != ASK_CAMERA) return;
        if (results.length > 0 && results[0] == PackageManager.PERMISSION_GRANTED) {
            if (preview.isAvailable()) start();
        } else {
            toast("Sin permiso de cámara no se puede leer el QR");
            finish();
        }
    }

    private void toast(String text) {
        android.widget.Toast.makeText(this, text, android.widget.Toast.LENGTH_LONG).show();
    }

    /** checkSelfPermission es de Android 6; antes el permiso llega al instalar. */
    private boolean cameraAllowed() {
        return Build.VERSION.SDK_INT < 23 || checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED;
    }

    private void start() {
        if (camera != null || bg == null || !cameraAllowed()) return;
        CameraManager m = (CameraManager) getSystemService(Context.CAMERA_SERVICE);
        try {
            String id = null;
            for (String c : m.getCameraIdList()) {
                Integer facing = m.getCameraCharacteristics(c).get(CameraCharacteristics.LENS_FACING);
                if (id == null || (facing != null && facing == CameraCharacteristics.LENS_FACING_BACK && !isBack(m, id))) id = c;
            }
            if (id == null) {
                toast("Este aparato no tiene cámara");
                finish();
                return;
            }
            CameraCharacteristics ch = m.getCameraCharacteristics(id);
            Integer so = ch.get(CameraCharacteristics.SENSOR_ORIENTATION);
            sensorDegrees = so == null ? 0 : so;
            StreamConfigurationMap map = ch.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
            size = pick(map == null ? new Size[0] : map.getOutputSizes(ImageFormat.YUV_420_888));
            reader = ImageReader.newInstance(size.getWidth(), size.getHeight(), ImageFormat.YUV_420_888, 2);
            reader.setOnImageAvailableListener(this::onFrame, bg);
            m.openCamera(id, new CameraDevice.StateCallback() {
                @Override
                public void onOpened(CameraDevice c) {
                    camera = c;
                    startSession();
                }

                @Override
                public void onDisconnected(CameraDevice c) {
                    c.close();
                    camera = null;
                }

                @Override
                public void onError(CameraDevice c, int error) {
                    c.close();
                    camera = null;
                    runOnUiThread(() -> {
                        toast("No se pudo abrir la cámara");
                        finish();
                    });
                }
            }, bg);
        } catch (CameraAccessException | SecurityException e) {
            toast("No se pudo abrir la cámara");
            finish();
        }
    }

    private static boolean isBack(CameraManager m, String id) throws CameraAccessException {
        Integer f = m.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING);
        return f != null && f == CameraCharacteristics.LENS_FACING_BACK;
    }

    /** Lo más cerca de 1280×720 sin pasarse: basta para leer y no cansa al procesador. */
    private static Size pick(Size[] sizes) {
        Size best = new Size(640, 480);
        long bestScore = Long.MAX_VALUE;
        for (Size s : sizes) {
            long px = (long) s.getWidth() * s.getHeight();
            if (px > 1280L * 960) continue;
            long score = Math.abs(px - 1280L * 720);
            if (score < bestScore) {
                bestScore = score;
                best = s;
            }
        }
        return best;
    }

    private void startSession() {
        try {
            SurfaceTexture st = preview.getSurfaceTexture();
            if (st == null || camera == null) return;
            st.setDefaultBufferSize(size.getWidth(), size.getHeight());
            Surface surface = new Surface(st);
            runOnUiThread(this::transform);
            camera.createCaptureSession(Arrays.asList(surface, reader.getSurface()), new CameraCaptureSession.StateCallback() {
                @Override
                public void onConfigured(CameraCaptureSession s) {
                    if (camera == null) return;
                    session = s;
                    try {
                        CaptureRequest.Builder b = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW);
                        b.addTarget(surface);
                        b.addTarget(reader.getSurface());
                        b.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE);
                        s.setRepeatingRequest(b.build(), null, bg);
                    } catch (CameraAccessException | IllegalStateException e) {
                        android.util.Log.w("Encaje", "cámara", e);
                    }
                }

                @Override
                public void onConfigureFailed(CameraCaptureSession s) {
                    runOnUiThread(() -> {
                        toast("No se pudo usar la cámara");
                        finish();
                    });
                }
            }, bg);
        } catch (CameraAccessException | IllegalStateException e) {
            android.util.Log.w("Encaje", "cámara", e);
        }
    }

    /** Endereza y recorta la imagen de la cámara para que llene la pantalla sin deformarse. */
    private void transform() {
        if (size == null) return;
        int w = preview.getWidth(), h = preview.getHeight();
        if (w == 0 || h == 0) return;
        // La vista previa de Camera2 ya llega girada a la orientación natural
        // de la pantalla (la del sensor aplicada): sólo falta girar lo que
        // esté rotada la pantalla ahora y recortar sin deformar.
        int display = getWindowManager().getDefaultDisplay().getRotation() * 90;
        float pw = size.getWidth(), ph = size.getHeight();
        boolean natSwap = sensorDegrees % 180 != 0;
        float nw = natSwap ? ph : pw, nh = natSwap ? pw : ph;
        float bw = display % 180 == 0 ? nw : nh, bh = display % 180 == 0 ? nh : nw;
        float cx = w / 2f, cy = h / 2f;
        Matrix m = new Matrix();
        m.postScale(nw / w, nh / h, cx, cy);
        m.postRotate(-display, cx, cy);
        float s = Math.max(w / bw, h / bh);
        m.postScale(s, s, cx, cy);
        preview.setTransform(m);
    }

    private void onFrame(ImageReader r) {
        Image img = r.acquireLatestImage();
        if (img == null) return;
        try {
            if (done) return;
            Image.Plane y = img.getPlanes()[0];
            int w = img.getWidth(), h = img.getHeight(), stride = y.getRowStride();
            ByteBuffer buf = y.getBuffer();
            byte[] data = new byte[w * h];
            for (int row = 0; row < h; row++) {
                buf.position(row * stride);
                buf.get(data, row * w, w);
            }
            PlanarYUVLuminanceSource src = new PlanarYUVLuminanceSource(data, w, h, 0, 0, w, h, false);
            try {
                Result res = qr.decode(new BinaryBitmap(new HybridBinarizer(src)), hints);
                found(res.getText());
            } catch (ReaderException e) {
                // Aún no se ve un QR: el siguiente fotograma.
            } finally {
                qr.reset();
            }
        } catch (RuntimeException e) {
            android.util.Log.w("Encaje", "QR", e);
        } finally {
            img.close();
        }
    }

    private void found(String text) {
        if (text != null && (text.startsWith("encaje://connect") || text.matches("(?s)https?://[^/#]+/?#encaje.*"))) {
            done = true;
            runOnUiThread(() -> {
                hint.setText("¡Leído!");
                setResult(RESULT_OK, new Intent().putExtra(EXTRA_LINK, text));
                finish();
            });
        } else if (System.currentTimeMillis() - lastWrong > 3000) {
            lastWrong = System.currentTimeMillis();
            runOnUiThread(() -> hint.setText("Ese QR no es de Encaje. Apunta al que sale en el PC."));
        }
    }

    private void stop() {
        try {
            if (session != null) session.close();
        } catch (RuntimeException ignored) {
        }
        session = null;
        if (camera != null) camera.close();
        camera = null;
        if (reader != null) reader.close();
        reader = null;
    }

    /** El recuadro donde poner el QR, con el resto oscurecido. */
    private static final class Frame extends View {
        private final Paint shade = new Paint(), line = new Paint(Paint.ANTI_ALIAS_FLAG);

        Frame(Context c) {
            super(c);
            shade.setColor(0x88000000);
            line.setColor(Color.WHITE);
            line.setStyle(Paint.Style.STROKE);
            line.setStrokeWidth(4 * c.getResources().getDisplayMetrics().density);
            line.setStrokeCap(Paint.Cap.ROUND);
        }

        @Override
        protected void onDraw(Canvas c) {
            float w = getWidth(), h = getHeight(), side = Math.min(w, h) * 0.62f, d = getResources().getDisplayMetrics().density;
            RectF box = new RectF((w - side) / 2, (h - side) / 2, (w + side) / 2, (h + side) / 2);
            Path p = new Path();
            p.setFillType(Path.FillType.EVEN_ODD);
            p.addRect(0, 0, w, h, Path.Direction.CW);
            p.addRoundRect(box, 24 * d, 24 * d, Path.Direction.CW);
            c.drawPath(p, shade);
            float k = side * 0.16f, rr = 24 * d;
            // Las cuatro esquinas, como en las apps de cámara.
            c.drawArc(new RectF(box.left, box.top, box.left + 2 * rr, box.top + 2 * rr), 180, 90, false, line);
            c.drawLine(box.left + rr, box.top, box.left + k, box.top, line);
            c.drawLine(box.left, box.top + rr, box.left, box.top + k, line);
            c.drawArc(new RectF(box.right - 2 * rr, box.top, box.right, box.top + 2 * rr), 270, 90, false, line);
            c.drawLine(box.right - rr, box.top, box.right - k, box.top, line);
            c.drawLine(box.right, box.top + rr, box.right, box.top + k, line);
            c.drawArc(new RectF(box.left, box.bottom - 2 * rr, box.left + 2 * rr, box.bottom), 90, 90, false, line);
            c.drawLine(box.left + rr, box.bottom, box.left + k, box.bottom, line);
            c.drawLine(box.left, box.bottom - rr, box.left, box.bottom - k, line);
            c.drawArc(new RectF(box.right - 2 * rr, box.bottom - 2 * rr, box.right, box.bottom), 0, 90, false, line);
            c.drawLine(box.right - rr, box.bottom, box.right - k, box.bottom, line);
            c.drawLine(box.right, box.bottom - rr, box.right, box.bottom - k, line);
        }
    }
}
