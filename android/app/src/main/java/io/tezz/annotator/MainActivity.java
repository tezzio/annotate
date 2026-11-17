package io.tezz.annotator;

import android.Manifest;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;

import org.libsdl.app.SDLActivity;

/**
 * The application's activity.
 *
 * SDL's own SDLActivity implements the Activity and the JNI bridge;
 * this subclass exists so that the library list is explicit.
 *
 * SDL2 is a separate shared library here, because its JNI entry points
 * — the ones the shim calls, such as nativeGetVersion — live in SDL2
 * and are reached by loading libSDL2.so and looking the symbols up in
 * it.  The application itself is libmain.so.
 *
 * The order matters: SDL2 has to be loaded first, because the shim
 * calls into it as soon as the libraries are loaded.
 */
public class MainActivity extends SDLActivity {

    private static final int CAMERA_PERMISSION_REQUEST = 1;

    @Override
    protected String[] getLibraries() {
        return new String[] { "SDL2", "main" };
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // The camera needs a permission that has to be granted at run
        // time from Android 6 onwards.  Without it the capture backend
        // finds no cameras, so it is asked for as soon as the activity
        // starts.
        requestCameraPermission();
    }

    /**
     * Ask for the camera permission, if it has not been granted yet.
     */
    private void requestCameraPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            // Before Android 6 the permission is granted at install
            // time.
            return;
        }
        if (checkSelfPermission(Manifest.permission.CAMERA)
                != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(
                    new String[] { Manifest.permission.CAMERA },
                    CAMERA_PERMISSION_REQUEST);
        }
    }
}
