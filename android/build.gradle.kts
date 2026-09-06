/**
 * lumina-video Android Root Build Configuration
 *
 * This is the root build file for the lumina-video Android project.
 * Common configuration is defined here for all modules.
 */

buildscript {
    repositories { mavenCentral() }
    dependencies { classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.4.10") }
}

plugins {
    id("com.android.library") version "9.4.0" apply false
}

// Clean task to remove build directories
tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}
