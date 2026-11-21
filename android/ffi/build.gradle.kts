// SPDX-License-Identifier: MIT
// The UniFFI bindings of vault-ffi plus its native libraries, both built from the
// Rust workspace at the repository root during the Gradle build and never
// committed:
//   - cargoNdk<Variant>: `cargo ndk` builds libvault_ffi.so for each ABI into a
//     generated jniLibs directory (the NDK is the pinned `ndkVersion`);
//   - uniffiBindgen<Variant>: vault-ffi's pinned `uniffi-bindgen` binary reads
//     the built library and writes the Kotlin bindings (package uniffi.vault_ffi)
//     into a generated source directory.
// Both are cacheable: their inputs are the Rust sources, Cargo.lock and the
// toolchain pins, cargo-ndk and NDK versions, .cargo/config.toml and the Rust-affecting
// environment (RUSTFLAGS, CARGO_*, ...). `-Pvault.abis=x86_64` limits the ABIs (emulator runs).
import javax.inject.Inject
import org.gradle.process.ExecOperations

plugins {
    alias(libs.plugins.android.library)
}

android {
    namespace = "dev.vault.ffi"
    // `platforms;android-37.0`: AGP 9's minor-API form of the pinned compileSdk.
    val (compileMajor, compileMinor) = libs.versions.compileSdk.get().split('.').map(String::toInt)
    compileSdk {
        version = release(compileMajor) { minorApiLevel = compileMinor }
    }
    buildToolsVersion = libs.versions.buildTools.get()
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        minSdk = libs.versions.minSdk.get().toInt()
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
        targetCompatibility = JavaVersion.toVersion(libs.versions.jvmTarget.get())
    }

    lint {
        abortOnError = true
        // The generated bindings are UniFFI's code (they probe API 33's
        // java.lang.ref.Cleaner and fall back to JNA's); see also lint.xml.
        checkGeneratedSources = false
        lintConfig = file("lint.xml")
    }
}

/** Builds libvault_ffi.so for each ABI with cargo-ndk into [outputDir]/<abi>/. */
@CacheableTask
abstract class CargoNdkBuild : DefaultTask() {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val rustSources: ConfigurableFileCollection

    @get:Input
    abstract val abis: ListProperty<String>

    @get:Input
    abstract val platform: Property<Int>

    @get:Input
    abstract val ndkVersion: Property<String>

    @get:Internal
    abstract val ndkDir: DirectoryProperty

    // Everything besides the sources that changes the output, so the build cache
    // never hands one build's artifacts to a run with other inputs.
    @get:Input
    abstract val cargoNdkVersion: Property<String>

    @get:Input
    abstract val ndkVersionKey: Property<String>

    @get:Input
    abstract val rustEnv: MapProperty<String, String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NAME_ONLY)
    abstract val cargoConfig: ConfigurableFileCollection

    @get:InputFile
    @get:PathSensitive(PathSensitivity.NAME_ONLY)
    abstract val rustToolchainFile: RegularFileProperty

    @get:Internal
    abstract val workspaceDir: DirectoryProperty

    @get:Input
    abstract val cargo: Property<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val execOps: ExecOperations

    @get:Inject
    abstract val fsOps: FileSystemOperations

    @TaskAction
    fun build() {
        val out = outputDir.get().asFile
        fsOps.delete { delete(out) }
        out.mkdirs()
        val ndk = ndkDir.get().asFile
        check(ndk.resolve("source.properties").isFile) {
            "NDK ${ndkVersion.get()} is not installed at $ndk (install it with sdkmanager \"ndk;${ndkVersion.get()}\")"
        }
        val cmd = mutableListOf("ndk", "--platform", platform.get().toString())
        abis.get().forEach { cmd += listOf("-t", it) }
        cmd += listOf("-o", out.absolutePath, "build", "-p", "vault-ffi", "--release", "--locked")
        execOps.exec {
            workingDir = workspaceDir.get().asFile
            executable = cargo.get()
            args(cmd)
            environment("ANDROID_NDK_HOME", ndk.absolutePath)
        }
        abis.get().forEach { abi ->
            val so = out.resolve("$abi/libvault_ffi.so")
            check(so.isFile && so.length() > 0) { "cargo ndk did not produce $so" }
        }
    }
}

/** Generates the Kotlin bindings from a built libvault_ffi.so into [outputDir]. */
@CacheableTask
abstract class UniffiBindgen : DefaultTask() {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val rustSources: ConfigurableFileCollection

    @get:InputFile
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val library: RegularFileProperty

    // Everything besides the sources that changes the output, so the build cache
    // never hands one build's artifacts to a run with other inputs.
    @get:Input
    abstract val cargoNdkVersion: Property<String>

    @get:Input
    abstract val ndkVersionKey: Property<String>

    @get:Input
    abstract val rustEnv: MapProperty<String, String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NAME_ONLY)
    abstract val cargoConfig: ConfigurableFileCollection

    @get:InputFile
    @get:PathSensitive(PathSensitivity.NAME_ONLY)
    abstract val rustToolchainFile: RegularFileProperty

    @get:Internal
    abstract val workspaceDir: DirectoryProperty

    @get:Input
    abstract val cargo: Property<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val execOps: ExecOperations

    @get:Inject
    abstract val fsOps: FileSystemOperations

    @TaskAction
    fun generate() {
        val out = outputDir.get().asFile
        fsOps.delete { delete(out) }
        out.mkdirs()
        execOps.exec {
            workingDir = workspaceDir.get().asFile
            executable = cargo.get()
            args(
                "run", "-p", "vault-ffi", "--features", "bindgen", "--bin", "uniffi-bindgen", "--locked", "--",
                "generate", "--library", library.get().asFile.absolutePath,
                "--language", "kotlin", "--no-format", "--out-dir", out.absolutePath,
            )
        }
        val kt = out.resolve("uniffi/vault_ffi/vault_ffi.kt")
        check(kt.isFile && kt.length() > 0) { "uniffi-bindgen did not produce $kt" }
    }
}

// The Rust workspace is the repository root (android/..).
val workspace: Directory = rootProject.layout.projectDirectory.dir("..")
val crateDirs = workspace.asFile.listFiles { f -> f.isDirectory && f.name.startsWith("vault-") }.orEmpty().sortedBy { it.name }
val rustInputs: FileCollection = files(
    workspace.file("Cargo.toml"),
    workspace.file("Cargo.lock"),
    workspace.file("rust-toolchain.toml"),
    crateDirs.map { dir -> fileTree(dir) { include("Cargo.toml", "build.rs", "src/**") } },
)
val targetAbis: Provider<List<String>> = providers.gradleProperty("vault.abis")
    .orElse("arm64-v8a,armeabi-v7a,x86_64")
    .map { list -> list.split(',').map(String::trim).filter(String::isNotEmpty) }
// Environment that can change cargo's/rustc's/cc's output (not machine-local
// paths, credentials, or terminal/network knobs). Read lazily: configuration-cache safe.
val rustEnvIgnored = Regex(
    "^(CARGO_HOME|CARGO_TARGET_DIR|CARGO_INCREMENTAL|CARGO_TERM_.*|CARGO_NET_.*|CARGO_HTTP_.*|" +
        "CARGO_REGISTR(Y|IES).*|CARGO_BUILD_JOBS|CARGO_LOG.*|RUSTUP_HOME|RUSTUP_(DIST|UPDATE).*|RUST_BACKTRACE|RUST_LOG)$",
)
val rustEnvPrefixes = listOf("CARGO_", "RUST", "CC_", "CXX_", "AR_", "CFLAGS", "CXXFLAGS", "BINDGEN_")
val rustEnvironment: Provider<Map<String, String>> = rustEnvPrefixes
    .map { prefix -> providers.environmentVariablesPrefixedBy(prefix) }
    .reduce { acc, next -> acc.zip(next) { a, b -> a + b } }
    .map { env -> env.filterKeys { key -> !rustEnvIgnored.matches(key) }.toSortedMap() }
// cargo reads config.toml (or the legacy `config`) from the workspace root and `.cargo/` below it.
val cargoConfigFiles: FileCollection = files(
    workspace.file(".cargo/config.toml").asFile,
    workspace.file(".cargo/config").asFile,
    rootProject.layout.projectDirectory.file(".cargo/config.toml").asFile,
    rootProject.layout.projectDirectory.file(".cargo/config").asFile,
)
val pinnedCargoNdk = libs.versions.cargoNdk.get()
val cargoCommand: Provider<String> = providers.gradleProperty("vault.cargo").orElse("cargo")
val minSdkLevel = libs.versions.minSdk.get().toInt()
val pinnedNdk = libs.versions.ndk.get()

androidComponents {
    val ndkDirectory = sdkComponents.ndkDirectory
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar { it.uppercase() }
        val native = tasks.register<CargoNdkBuild>("cargoNdk$suffix") {
            description = "Builds libvault_ffi.so (cargo-ndk, release profile) for ${variant.name}."
            rustSources.from(rustInputs)
            abis.set(targetAbis)
            platform.set(minSdkLevel)
            ndkVersion.set(pinnedNdk)
            ndkDir.set(ndkDirectory)
            workspaceDir.set(workspace)
            cargo.set(cargoCommand)
            cargoNdkVersion.set(pinnedCargoNdk)
            ndkVersionKey.set(pinnedNdk)
            rustEnv.set(rustEnvironment)
            cargoConfig.from(cargoConfigFiles)
            rustToolchainFile.set(workspace.file("rust-toolchain.toml"))
        }
        val bindings = tasks.register<UniffiBindgen>("uniffiBindgen$suffix") {
            description = "Generates the vault-ffi Kotlin bindings for ${variant.name}."
            rustSources.from(rustInputs)
            library.set(native.flatMap { t -> t.outputDir.file(targetAbis.map { "${it.first()}/libvault_ffi.so" }) })
            workspaceDir.set(workspace)
            cargo.set(cargoCommand)
            cargoNdkVersion.set(pinnedCargoNdk)
            ndkVersionKey.set(pinnedNdk)
            rustEnv.set(rustEnvironment)
            cargoConfig.from(cargoConfigFiles)
            rustToolchainFile.set(workspace.file("rust-toolchain.toml"))
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(native, CargoNdkBuild::outputDir)
        variant.sources.kotlin?.addGeneratedSourceDirectory(bindings, UniffiBindgen::outputDir)
    }
}

dependencies {
    // UniFFI's runtime: JNA as the Android AAR (with libjnidispatch per ABI);
    // `api` because the generated public classes expose JNA's Pointer.
    api(libs.jna) {
        artifact { type = "aar" }
    }
    api(libs.kotlinx.coroutines.core)

    androidTestImplementation(libs.junit)
    androidTestImplementation(libs.androidx.test.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.ext.junit)
}
