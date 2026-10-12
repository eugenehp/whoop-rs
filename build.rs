fn main() {
    // ── macOS: embed Info.plist so CoreBluetooth grants Bluetooth access ──────
    //
    // On macOS, CBCentralManager silently refuses to scan (state stays
    // "unauthorised") unless the running binary has an embedded Info.plist
    // containing NSBluetoothAlwaysUsageDescription.
    //
    // The standard trick for CLI tools is to stick the plist into the
    //   __TEXT,__info_plist
    // section of the Mach-O binary via the linker `-sectcreate` flag.
    // macOS reads that section exactly as it would an App Bundle's Info.plist.
    //
    // Note: `CARGO_CFG_TARGET_OS` reflects the *target* (not the host), so
    // cross-compilation from Linux → macOS is handled correctly too.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");

        let plist = format!("{dir}/Info.plist");

        // Each `cargo:rustc-link-arg` call appends one argument to the final
        // linker invocation, so these four together produce:
        //   ld … -sectcreate __TEXT __info_plist /path/to/Info.plist …
        println!("cargo:rustc-link-arg=-sectcreate");
        println!("cargo:rustc-link-arg=__TEXT");
        println!("cargo:rustc-link-arg=__info_plist");
        println!("cargo:rustc-link-arg={plist}");

        // Re-run if the plist changes
        println!("cargo:rerun-if-changed=Info.plist");
    }
}
