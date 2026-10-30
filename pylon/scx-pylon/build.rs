// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The BPF target architecture name for `__TARGET_ARCH_*` (what libbpf's `bpf_tracing.h` calls
/// it) and the Debian multiarch triplet for the system headers, for a Cargo target architecture.
fn bpf_arch(target_arch: &str) -> Option<(&'static str, &'static str)> {
    match target_arch {
        "x86_64" => Some(("x86", "x86_64-linux-gnu")),
        "aarch64" => Some(("arm64", "aarch64-linux-gnu")),
        _ => None,
    }
}

fn main() {
    let bpf_src = "src/bpf/scx_pylon.bpf.c";
    println!("cargo:rerun-if-changed={bpf_src}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let out_obj = out_dir.join("scx_pylon.bpf.o");

    let mut cmd = Command::new("clang");
    cmd.args([
        "-target",
        "bpf",
        // v3 for the atomic exchange on the per-CPU SMT claims.
        "-mcpu=v3",
        "-O2",
        "-g",
        "-Wall",
        "-Werror",
        "-Wno-missing-declarations",
        "-c",
        bpf_src,
        "-o",
    ])
    .arg(&out_obj);

    if let Ok(bpf_include) = env::var("DEP_BPF_INCLUDE") {
        for dir in env::split_paths(&bpf_include) {
            cmd.arg("-I").arg(dir);
        }
    }
    // The program reads kernel types through CO-RE (`vmlinux.h` with `preserve_access_index`), so
    // one `vmlinux.h` serves every architecture; only these two depend on the target.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not set");
    if let Some((bpf_target_arch, multiarch)) = bpf_arch(&target_arch) {
        cmd.arg(format!("-D__TARGET_ARCH_{bpf_target_arch}"));
        let multiarch_include = PathBuf::from("/usr/include").join(multiarch);
        if multiarch_include.is_dir() {
            cmd.arg("-I").arg(multiarch_include);
        }
    }

    let status = cmd
        .status()
        .expect("Failed to execute clang to compile scx_pylon.bpf.c");
    assert!(
        status.success(),
        "clang failed to compile {bpf_src} with status: {status}"
    );
}
