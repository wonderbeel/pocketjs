//! Links a small C shim (src/compat.c) providing the C23 math symbols
//! (`fmaximum_numf`, `fminimum_numf`, …) that LLVM 19+ lowers `f32::max/min`
//! to but that PocketBook's glibc 2.23 predates. Compiled for the target via
//! the `cc` crate, which cargo-zigbuild routes through zig for the cross-build.
//!
//! Gated to the exact PocketBook target: native builds use the system libm
//! (which already provides these symbols), so compiling the shim there would
//! risk a duplicate-symbol clash.

fn main() {
    println!("cargo:rerun-if-changed=src/compat.c");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "armv7-unknown-linux-gnueabi" {
        cc::Build::new()
            .file("src/compat.c")
            .compile("pocketbook_compat");
    }

    // Bake the resolved build plan's identity + viewport into the binary so the
    // host presents exactly what the bundle was laid out for. The runtime
    // target-id check (framework/src/host.ts::assertNativeHostContract) refuses
    // a bundle whose target disagrees with POCKETJS_TARGET, so one source tree
    // is rebuilt per pocketbook-* target id. The variable names mirror
    // framework hostBuildEnvironment; defaults match the canonical `pocketbook`
    // 3:4 fit profile (contracts/spec/platforms.ts) so a bare `cargo zigbuild`
    // builds the going-forward default.
    let host_target = std::env::var("POCKETJS_TARGET").unwrap_or_else(|_| "pocketbook".into());
    let host_abi = std::env::var("POCKETJS_HOST_ABI").unwrap_or_else(|_| "5".into());
    let logical_width = dimension("POCKETJS_LOGICAL_WIDTH", 375);
    let logical_height = dimension("POCKETJS_LOGICAL_HEIGHT", 500);
    let raster_density = dimension("POCKETJS_RASTER_DENSITY", 4);
    let presentation = std::env::var("POCKETJS_PRESENTATION").unwrap_or_else(|_| "fit".into());

    println!("cargo:rustc-env=POCKETJS_TARGET={host_target}");
    println!("cargo:rustc-env=POCKETJS_HOST_ABI={host_abi}");
    println!("cargo:rustc-env=POCKETJS_LOGICAL_WIDTH={logical_width}");
    println!("cargo:rustc-env=POCKETJS_LOGICAL_HEIGHT={logical_height}");
    println!("cargo:rustc-env=POCKETJS_RASTER_DENSITY={raster_density}");
    println!("cargo:rustc-env=POCKETJS_PRESENTATION={presentation}");

    for var in [
        "POCKETJS_TARGET",
        "POCKETJS_HOST_ABI",
        "POCKETJS_LOGICAL_WIDTH",
        "POCKETJS_LOGICAL_HEIGHT",
        "POCKETJS_RASTER_DENSITY",
        "POCKETJS_PRESENTATION",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

fn dimension(name: &str, default: u32) -> u32 {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("{name} must be a positive integer, got {value:?}")),
        Err(_) => default,
    }
}
