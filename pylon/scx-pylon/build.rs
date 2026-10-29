// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::env;
use std::path::PathBuf;
use std::process::Command;

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
    let multiarch_include = PathBuf::from("/usr/include/x86_64-linux-gnu");
    if multiarch_include.is_dir() {
        cmd.arg("-I").arg(multiarch_include);
    }

    let status = cmd
        .status()
        .expect("Failed to execute clang to compile scx_pylon.bpf.c");
    assert!(
        status.success(),
        "clang failed to compile {bpf_src} with status: {status}"
    );
}
