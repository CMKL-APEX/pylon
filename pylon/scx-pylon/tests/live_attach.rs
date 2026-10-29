// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs;

use scx_pylon::ScxPylonUserspace;
use scx_pylon::coresched::{CookieDenial, GuardMode, KernelCoreSched};

#[test]
#[ignore = "requires CAP_BPF / root"]
fn test_live_attach_and_detach() {
    let guard = KernelCoreSched::detect()
        .needs_guard(GuardMode::Auto)
        .then_some(CookieDenial::Eperm);
    let scx =
        ScxPylonUserspace::load_and_attach(guard).expect("Failed to load and attach scx_pylon");
    let state = fs::read_to_string("/sys/kernel/sched_ext/state").expect("read sched_ext state");
    assert_eq!(state.trim(), "enabled");
    println!("sched_ext state: {}", state.trim());

    // Test writing to owner map
    scx.owner_map()
        .set_owner(2, 12345, scx_pylon::PcpuOwnerFlags::ACTIVE)
        .expect("set owner");
    let entry = scx.owner_map().get_owner(2).expect("get owner");
    assert_eq!(entry.target_tid, 12345);
    assert!(
        scx_pylon::PcpuOwnerFlags::from_bits_retain(entry.flags)
            .contains(scx_pylon::PcpuOwnerFlags::ACTIVE)
    );

    // Drop scx to detach
    drop(scx);

    let state_after =
        fs::read_to_string("/sys/kernel/sched_ext/state").expect("read sched_ext state");
    assert_eq!(state_after.trim(), "disabled");
    println!("sched_ext state after detach: {}", state_after.trim());
}
