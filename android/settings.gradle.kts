// The Gradle settings for the annotator's Android build.
//
// The project has a single module: the application itself.  The native
// code is not built by Gradle — `scripts/build-android.sh` builds it
// with cargo and drops `libmain.so` into the module's jniLibs
// directory before Gradle packages the APK.

pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "annotator"
include(":app")
