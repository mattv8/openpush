# Generated Rust libraries

Place the cross-compiled UniFFI library at:

`arm64-v8a/libopenpush_mobile_bindings.so`

The generated Kotlin JNA loader resolves this packaged library by its Rust
library name. Do not replace it with a hand-written JNI bridge or load a
library from a mutable external path. The NDK build and ABI packaging check are
blocked until the orchestrator-owned Android SDK/NDK installation completes.
