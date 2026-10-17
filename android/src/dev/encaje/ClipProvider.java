package dev.encaje;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.Binder;
import android.os.ParcelFileDescriptor;
import android.os.Process;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileNotFoundException;

/**
 * Imagen copiada en el PC (modo ratón y teclado). El portapapeles de Android
 * sólo guarda imágenes como URI content://, así que el servidor del PC
 * (proceso de shell) la escribe aquí con `content write` y pone esta URI en
 * el portapapeles; la app donde pegues la lee de aquí.
 *
 * Sólo shell puede escribir. Leer puede cualquiera, como cualquier cosa que
 * esté en el portapapeles. Se guarda sólo la última imagen.
 */
public final class ClipProvider extends ContentProvider {
    private static final int SHELL_UID = 2000;

    @Override
    public boolean onCreate() {
        return true;
    }

    private File file(Uri uri) throws FileNotFoundException {
        String name = uri.getLastPathSegment();
        if (name == null || !name.matches("clip-[0-9-]+\\.png")) throw new FileNotFoundException(uri.toString());
        return new File(dir(), name);
    }

    private File dir() {
        File d = new File(getContext().getCacheDir(), "clip");
        d.mkdirs();
        return d;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        File f = file(uri);
        if (mode.contains("w")) {
            int uid = Binder.getCallingUid();
            if (uid != SHELL_UID && uid != Process.ROOT_UID) throw new SecurityException("sólo el PC (shell) puede escribir");
            File[] old = dir().listFiles();
            if (old != null) for (File o : old) if (!o.equals(f)) o.delete();
            return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_WRITE_ONLY
                | ParcelFileDescriptor.MODE_CREATE | ParcelFileDescriptor.MODE_TRUNCATE);
        }
        if (!f.exists()) throw new FileNotFoundException(uri.toString());
        return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY);
    }

    @Override
    public String getType(Uri uri) {
        return "image/png";
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String sort) {
        File f;
        try {
            f = file(uri);
        } catch (FileNotFoundException e) {
            return null;
        }
        String[] cols = projection != null ? projection : new String[] {OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE};
        Object[] row = new Object[cols.length];
        for (int i = 0; i < cols.length; i++) {
            if (OpenableColumns.DISPLAY_NAME.equals(cols[i])) row[i] = f.getName();
            else if (OpenableColumns.SIZE.equals(cols[i])) row[i] = f.length();
        }
        MatrixCursor c = new MatrixCursor(cols, 1);
        c.addRow(row);
        return c;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        return null;
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] args) {
        return 0;
    }

    @Override
    public int delete(Uri uri, String selection, String[] args) {
        return 0;
    }
}
