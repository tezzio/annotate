package io.tezz.annotator;

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
    @Override
    protected String[] getLibraries() {
        return new String[] { "SDL2", "main" };
    }
}
