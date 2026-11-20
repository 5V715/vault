// SPDX-License-Identifier: MIT
pluginManagement {
    repositories {
        // Google's repository serves only Google/AndroidX groups (no dependency
        // confusion through it); everything else comes from Maven Central or the
        // plugin portal.
        google {
            content {
                includeGroupByRegex("com\\.android(\\..*)?")
                includeGroupByRegex("com\\.google\\..*")
                includeGroupByRegex("androidx\\..*")
            }
        }
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google {
            content {
                includeGroupByRegex("com\\.android(\\..*)?")
                includeGroupByRegex("com\\.google\\..*")
                includeGroupByRegex("androidx\\..*")
            }
        }
        mavenCentral()
    }
}

rootProject.name = "vault-android"

include(":core", ":ffi", ":app")
