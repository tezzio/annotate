// The top-level Gradle build file.
//
// It only declares the Android Gradle Plugin; the application module
// is configured in app/build.gradle.kts.

plugins {
    id("com.android.application") version "8.7.3" apply false
}

tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}
