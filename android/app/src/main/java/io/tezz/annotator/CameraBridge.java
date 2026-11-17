package io.tezz.annotator;

import android.Manifest;
import android.content.Context;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CameraMetadata;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.util.Size;
import android.view.Surface;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * The Camera2 bridge.
 *
 * Android has neither V4L2 nor AVFoundation; the camera is reached
 * through Camera2.  The native code cannot call Camera2 itself, so this
 * class does it and hands the frames over through JNI.
 *
 * The native code drives it with three calls:
 *
 *   enumerate()  — the cameras the device has, with their sizes.
 *   open()       — start streaming from one of them.
 *   close()      — stop streaming.
 *
 * Frames arrive through {@link #nativeOnFrame}, which the native code
 * registers with JNI, so the capture thread does not have to poll.
 *
 * The frames are delivered as YUV_420_888, the format every Camera2
 * device is required to support.  It is not compressed, so the native
 * side converts it to RGB itself without needing a JPEG decoder.
 */
public final class CameraBridge {

    private static final String TAG = "annotator-camera";

    /** The camera is open and streaming. */
    public static final int STATUS_STREAMING = 0;
    /** The camera could not be opened — it may be in use elsewhere. */
    public static final int STATUS_FAILED = 1;
    /** The camera was disconnected. */
    public static final int STATUS_DISCONNECTED = 2;

    /** The facing direction of a camera that points at the user. */
    public static final int FACING_FRONT = 0;
    /** The facing direction of a camera that points away from the user. */
    public static final int FACING_BACK = 1;
    /** A camera whose direction could not be read. */
    public static final int FACING_EXTERNAL = 2;

    // The camera that is currently open.
    private static CameraDevice sCamera;
    private static CameraCaptureSession sSession;
    private static ImageReader sReader;
    private static HandlerThread sThread;
    private static Handler sHandler;

    // Whether the camera is still open.
    //
    // The reader is closed when the camera is closed, but its callback
    // may already be running: the image it is holding is released with
    // the reader, and reading it afterwards reads freed memory.
    private static volatile boolean sOpen = false;

    // Held while a frame is being read.
    //
    // The close takes it before releasing the reader, so that a frame
    // is never read from a reader that has been closed.  A flag alone
    // is not enough: it is checked when the callback starts, and the
    // callback may already be reading the image by the time the close
    // clears it.
    private static final Object sLock = new Object();

    /**
     * Called by this class when a frame arrives.
     *
     * Implemented in the native library.  The `data` array holds the
     * Y, U and V planes laid out one after the other, which is the
     * order the native side expects.
     */
    public static native void nativeOnFrame(byte[] luma, byte[] chroma,
                                           int width, int height);

    /**
     * Called by this class when the camera's state changes.
     *
     * Implemented in the native library.  The status is one of the
     * `STATUS_` constants.
     */
    public static native void nativeOnStatus(int status);

    private CameraBridge() {
    }

    /**
     * List the cameras.
     *
     * The result is a flat array of four values per camera: the id as
     * a string, the facing direction as an integer, the width and the
     * height.  A camera whose characteristics cannot be read is
     * skipped.
     */
    public static Object[] enumerate(Context context) {
        List<Object> out = new ArrayList<>();
        CameraManager manager = (CameraManager) context.getSystemService(Context.CAMERA_SERVICE);
        if (manager == null) {
            return out.toArray();
        }

        String[] ids;
        try {
            ids = manager.getCameraIdList();
        } catch (CameraAccessException e) {
            Log.w(TAG, "could not list the cameras", e);
            return out.toArray();
        }

        for (String id : ids) {
            int facing = FACING_EXTERNAL;
            int width = 0;
            int height = 0;
            try {
                CameraCharacteristics chars = manager.getCameraCharacteristics(id);
                Integer f = chars.get(CameraCharacteristics.LENS_FACING);
                if (f != null) {
                    if (f == CameraCharacteristics.LENS_FACING_FRONT) {
                        facing = FACING_FRONT;
                    } else if (f == CameraCharacteristics.LENS_FACING_BACK) {
                        facing = FACING_BACK;
                    }
                }
                Size size = largestSize(chars, ImageFormat.YUV_420_888);
                if (size != null) {
                    width = size.getWidth();
                    height = size.getHeight();
                }
            } catch (CameraAccessException e) {
                Log.w(TAG, "could not read the characteristics of " + id, e);
                continue;
            }
            out.add(id);
            out.add(facing);
            out.add(width);
            out.add(height);
        }
        return out.toArray();
    }

    /**
     * The largest size the camera supports in the given format.
     */
    private static Size largestSize(CameraCharacteristics chars, int format) {
        StreamConfigurationMap map =
                chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
        if (map == null) {
            return null;
        }
        Size[] sizes = map.getOutputSizes(format);
        if (sizes == null || sizes.length == 0) {
            return null;
        }
        Size best = sizes[0];
        for (Size s : sizes) {
            if ((long) s.getWidth() * s.getHeight()
                    > (long) best.getWidth() * best.getHeight()) {
                best = s;
            }
        }
        return best;
    }

    /**
     * The sizes the camera can deliver in the given format.
     *
     * The result is a flat array of two values per size: the width and
     * the height.
     */
    public static int[] sizes(Context context, String cameraId, int format) {
        List<Integer> out = new ArrayList<>();
        CameraManager manager = (CameraManager) context.getSystemService(Context.CAMERA_SERVICE);
        if (manager == null) {
            return toIntArray(out);
        }
        try {
            CameraCharacteristics chars = manager.getCameraCharacteristics(cameraId);
            StreamConfigurationMap map =
                    chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
            if (map != null) {
                Size[] sizes = map.getOutputSizes(format);
                if (sizes != null) {
                    for (Size s : sizes) {
                        out.add(s.getWidth());
                        out.add(s.getHeight());
                    }
                }
            }
        } catch (CameraAccessException e) {
            Log.w(TAG, "could not read the sizes of " + cameraId, e);
        }
        return toIntArray(out);
    }

    private static int[] toIntArray(List<Integer> list) {
        int[] out = new int[list.size()];
        for (int i = 0; i < out.length; i++) {
            out[i] = list.get(i);
        }
        return out;
    }

    /**
     * Open a camera and start streaming from it.
     *
     * The width and the height are the ones the native code asked for.
     * The camera may deliver something else — it rounds up to what it
     * can do — so the native side is told the real size with every
     * frame.
     *
     * Returns true when the camera was opened.  The result arrives
     * later through {@link #nativeOnStatus}, because opening a camera is
     * asynchronous.
     */
    public static boolean open(Context context, String cameraId, int width, int height) {
        final CameraManager manager = (CameraManager) context.getSystemService(Context.CAMERA_SERVICE);
        if (manager == null) {
            return false;
        }

        // Only one camera at a time; opening a second one has to close
        // the first.
        close();

        // The camera's callbacks arrive on its own thread, which is
        // separate from the thread the native code runs on.
        sThread = new HandlerThread("annotator-camera");
        sThread.start();
        sHandler = new Handler(sThread.getLooper());

        final int requestedWidth = width;
        final int requestedHeight = height;

        // The thread the camera's callbacks arrive on.  A callback
        // that arrives after the camera has been closed is ignored: the
        // thread is stopped with `quitSafely`, which lets the messages
        // that are already queued run, and one of them is the "opened"
        // message.
        final HandlerThread thisThread = sThread;

        try {
            manager.openCamera(cameraId, new CameraDevice.StateCallback() {
                @Override
                public void onOpened(CameraDevice camera) {
                    synchronized (sLock) {
                        if (sThread != thisThread) {
                            // The camera was closed while it was being
                            // opened.
                            camera.close();
                            return;
                        }
                        sCamera = camera;
                        sOpen = true;
                    }
                    startPreview(manager, camera, requestedWidth, requestedHeight);
                }

                @Override
                public void onDisconnected(CameraDevice camera) {
                    synchronized (sLock) {
                        if (sThread != thisThread) {
                            camera.close();
                            return;
                        }
                        camera.close();
                        sCamera = null;
                    }
                    reportStatus(STATUS_DISCONNECTED);
                }

                @Override
                public void onError(CameraDevice camera, int error) {
                    synchronized (sLock) {
                        if (sThread != thisThread) {
                            camera.close();
                            return;
                        }
                        camera.close();
                        sCamera = null;
                    }
                    Log.w(TAG, "the camera failed with error " + error);
                    reportStatus(STATUS_FAILED);
                }
            }, sHandler);
            return true;
        } catch (CameraAccessException | SecurityException e) {
            Log.w(TAG, "could not open the camera " + cameraId, e);
            reportStatus(STATUS_FAILED);
            return false;
        }
    }

    /**
     * Configure the camera and start the preview.
     */
    private static void startPreview(CameraManager manager, CameraDevice camera,
                                     int width, int height) {
        synchronized (sLock) {
        try {
            // The reader has to be exactly the size the camera
            // delivers.  A reader of a different size makes the camera
            // abandon its buffers and stop streaming, so the size that
            // was actually chosen is looked up rather than assumed.
            int[] chosen = chooseSize(manager, camera, width, height);
            // The reader needs several buffers: the camera fills them
            // while a frame is being read, and with only two it runs
            // out and stops — "Unable to acquire a buffer item, very
            // likely client tried to acquire more than maxImages
            // buffers".
            sReader = ImageReader.newInstance(
                    chosen[0],
                    chosen[1],
                    ImageFormat.YUV_420_888,
                    4);

            final ImageReader thisReader = sReader;
            thisReader.setOnImageAvailableListener(new ImageReader.OnImageAvailableListener() {
                @Override
                public void onImageAvailable(ImageReader reader) {
                    // The lock is held for as long as the image is
                    // read, so that the close cannot release the
                    // reader underneath it.
                    synchronized (sLock) {
                        // The reader may have been closed while this
                        // callback was waiting to run.
                        if (!sOpen || reader != thisReader) {
                            return;
                        }
                        Image image = null;
                        try {
                            // The *latest* image is taken rather than
                            // the next one: the older frames are
                            // discarded, so the camera never runs out
                            // of buffers while a frame is being read.
                            // A live preview wants the newest frame
                            // anyway.
                            image = reader.acquireLatestImage();
                            if (image == null) {
                                return;
                            }
                            deliver(image);
                        } catch (Throwable t) {
                            // A failure here would take the whole
                            // application down, so it is caught.
                            Log.w(TAG, "could not deliver a frame", t);
                        } finally {
                            if (image != null) {
                                try {
                                    image.close();
                                } catch (Throwable ignored) {
                                    // The image may already have
                                    // been released with the reader.
                                }
                            }
                        }
                    }
                }
            }, sHandler);

            Surface surface = sReader.getSurface();
            CaptureRequest.Builder builder = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW);
            builder.addTarget(surface);
            builder.set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO);
            // The camera is told which size to deliver.  Without this
            // it picks one of its own — 1920x864 rather than the
            // 1920x1440 the reader was created for — and a reader of
            // the wrong size makes the camera abandon its buffers:
            // "BufferQueue has been abandoned", and the feed stops.
            builder.set(
                    CaptureRequest.SCALER_CROP_REGION,
                    new android.graphics.Rect(0, 0, chosen[0], chosen[1]));

            camera.createCaptureSession(Collections.singletonList(surface),
                    new CameraCaptureSession.StateCallback() {
                        @Override
                        public void onConfigured(CameraCaptureSession session) {
                            sSession = session;
                            try {
                                session.setRepeatingRequest(builder.build(), null, sHandler);
                                reportStatus(STATUS_STREAMING);
                            } catch (CameraAccessException e) {
                                Log.w(TAG, "could not start the preview", e);
                                reportStatus(STATUS_FAILED);
                            }
                        }

                        @Override
                        public void onConfigureFailed(CameraCaptureSession session) {
                            Log.w(TAG, "the camera session could not be configured");
                            reportStatus(STATUS_FAILED);
                        }
                    }, sHandler);
        } catch (CameraAccessException e) {
            Log.w(TAG, "could not configure the camera", e);
            reportStatus(STATUS_FAILED);
        }
        }
    }

    /**
     * The size the camera will actually deliver.
     *
     * The camera rounds the requested size up to one it supports, so
     * the size it will really produce is looked up in its
     * configuration.  When nothing matches, the requested size is
     * used — the camera will then pick its own and the frames will
     * carry the real size.
     */
    private static int[] chooseSize(CameraManager manager, CameraDevice camera,
                                    int width, int height) {
        try {
            CameraCharacteristics chars = manager.getCameraCharacteristics(camera.getId());
            StreamConfigurationMap map =
                    chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
            if (map != null) {
                Size[] sizes = map.getOutputSizes(ImageFormat.YUV_420_888);
                if (sizes != null) {
                    // The exact size that was asked for, if the camera
                    // has it.  The reader has to be the size the camera
                    // delivers, and the camera delivers the size it is
                    // told to.
                    for (Size s : sizes) {
                        if (s.getWidth() == width && s.getHeight() == height) {
                            return new int[] { width, height };
                        }
                    }

                    // Otherwise the closest one that is at least as
                    // large, so that the camera can crop down to it.
                    Size best = null;
                    for (Size s : sizes) {
                        if (s.getWidth() >= width && s.getHeight() >= height) {
                            if (best == null
                                    || (long) s.getWidth() * s.getHeight()
                                       < (long) best.getWidth() * best.getHeight()) {
                                best = s;
                            }
                        }
                    }
                    if (best == null && sizes.length > 0) {
                        best = sizes[0];
                    }
                    if (best != null) {
                        return new int[] { best.getWidth(), best.getHeight() };
                    }
                }
            }
        } catch (CameraAccessException e) {
            Log.w(TAG, "could not read the stream configuration", e);
        }
        return new int[] { width, height };
    }

    /**
     * Hand a frame to the native code.
     *
     * The YUV_420_888 image has three planes: the luma one, and two
     * chroma ones that share the same buffer.  They are copied into
     * one array, the luma plane first, then the chroma planes, so that
     * the native side receives them in the order it expects.
     */
    private static void deliver(Image image) {
        try {
            Image.Plane[] planes = image.getPlanes();
            if (planes.length < 3) {
                return;
            }

            int width = image.getWidth();
            int height = image.getHeight();
            int chromaWidth = (width + 1) / 2;
            int chromaHeight = (height + 1) / 2;

            // The luma plane, packed so that every row is exactly one
            // row of pixels with no padding.
            byte[] lumaData = new byte[width * height];
            ByteBuffer luma = planes[0].getBuffer().duplicate();
            int lumaRowStride = planes[0].getRowStride();
            for (int row = 0; row < height; row++) {
                int start = row * lumaRowStride;
                if (start + width > luma.limit()) {
                    break;
                }
                luma.position(start);
                luma.get(lumaData, row * width, width);
            }

            // The chroma planes, packed into interleaved U,V pairs.
            //
            // The two planes usually share one buffer, with the U and
            // the V bytes next to each other — reading them one after
            // the other from that same buffer would run off its end,
            // so each is read on its own.
            byte[] chromaData = new byte[chromaWidth * chromaHeight * 2];
            ByteBuffer chromaU = planes[1].getBuffer().duplicate();
            ByteBuffer chromaV = planes[2].getBuffer().duplicate();
            int uRowStride = planes[1].getRowStride();
            int vRowStride = planes[2].getRowStride();
            int uPixelStride = planes[1].getPixelStride();
            int vPixelStride = planes[2].getPixelStride();

            for (int row = 0; row < chromaHeight; row++) {
                int uStart = row * uRowStride;
                int vStart = row * vRowStride;
                if (uStart + chromaWidth * uPixelStride > chromaU.limit()
                        || vStart + chromaWidth * vPixelStride > chromaV.limit()) {
                    break;
                }
                for (int col = 0; col < chromaWidth; col++) {
                    chromaU.position(uStart + col * uPixelStride);
                    chromaV.position(vStart + col * vPixelStride);
                    int out = (row * chromaWidth + col) * 2;
                    chromaData[out] = chromaU.get();
                    chromaData[out + 1] = chromaV.get();
                }
            }

            nativeOnFrame(lumaData, chromaData, width, height);
        } catch (Exception e) {
            Log.w(TAG, "could not deliver a frame", e);
        }
    }

    /**
     * Report a status change to the native code.
     */
    private static void reportStatus(int status) {
        try {
            nativeOnStatus(status);
        } catch (UnsatisfiedLinkError e) {
            Log.w(TAG, "the native library is not loaded", e);
        }
    }

    /**
     * Close the camera and stop the thread it ran on.
     */
    public static void close() {
        // The lock is taken first: a callback that is already reading
        // a frame finishes with it before anything is released.
        synchronized (sLock) {
            // The flag is cleared, so a callback that has not started
            // yet stops.
            sOpen = false;

            if (sSession != null) {
                try {
                    sSession.close();
                } catch (Exception e) {
                    Log.w(TAG, "could not close the capture session", e);
                }
                sSession = null;
            }
            if (sReader != null) {
                sReader.close();
                sReader = null;
            }
            if (sCamera != null) {
                sCamera.close();
                sCamera = null;
            }
        }

        // The thread is stopped afterwards.  It is not joined before
        // the reader is released, because the callback takes the lock
        // anyway: a callback that is waiting for the lock finds the
        // flag cleared and returns without touching the image.
        HandlerThread thread = sThread;
        if (thread != null) {
            thread.quitSafely();
        }
        sThread = null;
        sHandler = null;
    }

    /**
     * Whether the application has permission to use the camera.
     *
     * The permission has to be granted at run time on Android 6 and
     * later; the native code checks this before asking for a camera.
     */
    public static boolean hasPermission(Context context) {
        return context.checkSelfPermission(Manifest.permission.CAMERA)
                == PackageManager.PERMISSION_GRANTED;
    }
}
