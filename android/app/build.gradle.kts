// SPDX-License-Identifier: MIT
plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    // Placeholder identity: the applicationId is permanent once an APK is
    // installed (updates must match it and the signing key); settle it before
    // the first release.
    namespace = "dev.vault.android"
    // `platforms;android-37.0`: AGP 9's minor-API form of the pinned compileSdk.
    val (compileMajor, compileMinor) = libs.versions.compileSdk.get().split('.').map(String::toInt)
    compileSdk {
        version = release(compileMajor) { minorApiLevel = compileMinor }
    }
    buildToolsVersion = libs.versions.buildTools.get()
    // Same NDK the toolchain action installs; AGP's native strip/merge tasks use it.
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        applicationId = "dev.vault.android"
        minSdk = libs.versions.minSdk.get().toInt()
        targetSdk = libs.versions.targetSdk.get().toInt()
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        release {
            // Unsigned here: the owner signs offline (docs, Task 8).
            isMinifyEnabled = false
        }
    }

    buildFeatures {
        compose = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
        targetCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
    }

    // No Google-encrypted dependency metadata block in the APK (sideloaded, not Play).
    dependenciesInfo {
        includeInApk = false
        includeInBundle = false
    }

    lint {
        abortOnError = true
        checkReleaseBuilds = true
    }
}

dependencies {
    implementation(project(":core"))
    implementation(project(":ffi"))

    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.activity.compose)
    implementation(libs.kotlinx.coroutines.android)

    testImplementation(libs.junit)
}
