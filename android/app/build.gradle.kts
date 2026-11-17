// The application module.
//
// There is no Java or Kotlin code of our own: the application is the
// native library, and SDL's own Java shim (`org.libsdl.app`) provides
// the Activity that hosts it.  The shim's classes were copied from
// the SDL2 sources by scripts/build-android.sh.

plugins {
    id("com.android.application")
}

android {
    namespace = "org.libsdl.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.tezz.annotator"
        minSdk = 24
        targetSdk = 35
        versionCode = 1
        versionName = "1.0"

        // The native library is built by scripts/build-android.sh and
        // dropped into jniLibs before Gradle packages the APK.  Every
        // ABI that was built is packaged into the one APK; Android
        // picks the right one when the application is installed.
        ndk {
            abiFilters += (System.getenv("ANNOTATOR_ANDROID_ABIS")
                ?: "arm64-v8a").split(",").map { it.trim() }.filter { it.isNotEmpty() }
        }
    }

    // The native library carries its own fonts and has no other assets.
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    // The SDL Java shim is a library of plain Java; the lint checks
    // that would complain about it are not useful here.
    lint {
        abortOnError = false
    }
}

dependencies {
    // SDL's shim uses AndroidX for the immersive-mode helpers.
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.core:core:1.13.1")

    // The Kotlin standard library is split across several artifacts
    // in older versions, and the AndroidX dependencies pull in the
    // old `kotlin-stdlib-jdk7`/`-jdk8` ones alongside the modern
    // `kotlin-stdlib`.  The classes are in both, so the build fails
    // with "Duplicate class kotlin.internal.jdk7...".
    //
    // Aligning on one version resolves it.  The BOM keeps the Kotlin
    // version consistent across every dependency.
    implementation(platform("org.jetbrains.kotlin:kotlin-bom:1.9.24"))
}
