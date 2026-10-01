plugins {
    id("org.jetbrains.kotlin.jvm")
    application
}

kotlin { jvmToolchain(17) }

sourceSets {
    main {
        kotlin.srcDir("../app/src/main/java/uniffi")
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.14.0")
}

application {
    mainClass.set("dev.openpush.mobile.smoke.MainKt")
    applicationDefaultJvmArgs = listOf(
        "-Duniffi.component.openpush_mobile_bindings.libraryOverride=" +
            rootProject.projectDir.resolve("../../target/debug/" +
                System.mapLibraryName("openpush_mobile_bindings")).canonicalPath
    )
}
