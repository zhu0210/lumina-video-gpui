/**
 * lumina-video Android Project
 *
 * Root settings for the lumina-video Android bridge used by native-frame.
 */

pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "lumina-video"
include(":lumina-video-bridge")
