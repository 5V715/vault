// SPDX-License-Identifier: MIT
// Pure Kotlin/JVM logic (state machines, validation): no Android dependency, so
// it is unit-tested on the JVM.
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.kotlin.jvm)
}

java {
    sourceCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
    targetCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.fromTarget(libs.versions.jvmTarget.get()))
        allWarningsAsErrors.set(true)
    }
}

dependencies {
    testImplementation(libs.junit)
}

// The binding rules are a port of vault_proto::binding: both sides run the shared
// vectors in vault-proto/tests/vectors (VaultIdentityTest).
val bindingVectors = rootDir.resolve("../vault-proto/tests/vectors")
tasks.test {
    inputs.dir(bindingVectors).withPathSensitivity(PathSensitivity.RELATIVE)
    systemProperty("vault.vectors", bindingVectors.absolutePath)
}
