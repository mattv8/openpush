plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "dev.openpush.mobile"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.openpush.mobile"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // The packaged Rust library is arm64-only; do not advertise other ABIs.
        ndk { abiFilters += "arm64-v8a" }
    }

    buildFeatures {
        compose = true
        // BuildConfig.DEBUG gates the loopback-only cleartext development origin.
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    sourceSets {
        getByName("main").jniLibs.srcDir("src/main/jniLibs")
    }

    testOptions {
        unitTests {
            isIncludeAndroidResources = true
            all { test ->
                // Robolectric tests exercise the real generated bindings against the host build of
                // the same Rust crate (SQLCipher + libsodium); there is no mock message store.
                test.systemProperty(
                    "uniffi.component.openpush_mobile_bindings.libraryOverride",
                    rootProject.projectDir.resolve(
                        "../../target/debug/" + System.mapLibraryName("openpush_mobile_bindings")
                    ).canonicalPath,
                )
            }
        }
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.activity:activity-compose:1.10.0")
    implementation("androidx.work:work-runtime-ktx:2.10.0")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")
    // UniFFI-generated Kotlin uses JNA to load libopenpush_mobile_bindings.so
    // from this APK's jniLibs directory; never from an arbitrary filesystem path.
    implementation("net.java.dev.jna:jna:5.14.0@aar")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.14.1")
    testImplementation("androidx.test:core:1.6.1")
    testImplementation("androidx.work:work-testing:2.10.0")
    // Desktop JNA jar (with host jnidispatch) for the JVM-hosted Robolectric runtime.
    testImplementation("net.java.dev.jna:jna:5.14.0")

    androidTestImplementation("androidx.test:core:1.6.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
}
