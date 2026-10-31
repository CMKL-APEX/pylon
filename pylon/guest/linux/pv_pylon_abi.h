/* SPDX-License-Identifier: GPL-2.0-only OR BSD-3-Clause */
/*
 * Copyright © 2026 CMKL-APEX
 *
 * The Pylon shared page and the pylon-kick device, as seen by the guest.
 *
 * This mirrors pylon/pylon-abi/src/lib.rs (ABI version 2). That crate is the
 * source of truth: both pin the same offsets with static assertions, so a
 * layout change has to touch both.
 */
#ifndef PV_PYLON_ABI_H
#define PV_PYLON_ABI_H

#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/types.h>

#define PYLON_MAGIC			0x50594C4F4E564D31ULL /* "PYLONVM1" */
#define PYLON_ABI_VERSION		2
#define PYLON_MAX_VCPUS			64
#define PYLON_SHARED_PAGE_SIZE		8192

#define VCPU_STATE_PARKED		0
#define VCPU_STATE_RUNNING		1
#define VCPU_STATE_PARKING		2

#define PYLON_KICK_VENDOR_ID		0x1AF4
#define PYLON_KICK_DEVICE_ID		0x10F0

/* BAR0 registers, all 32 bits wide. */
#define PYLON_KICK_REG_MAGIC		0x00
#define PYLON_KICK_REG_ABI_VERSION	0x04
#define PYLON_KICK_REG_NUM_VCPUS	0x08
#define PYLON_KICK_REG_SHM_GPA_LO	0x0C
#define PYLON_KICK_REG_SHM_GPA_HI	0x10
#define PYLON_KICK_REG_PARK_DOORBELL	0x40

struct pylon_header {
	u64 magic;
	u32 version;
	u32 num_vcpus;
	u64 vm_id;
	u64 tsc_khz;
	u8 reserved[32];
} __aligned(64);

struct pylon_telemetry {
	u32 runnable_threads;
	u32 rx_ring_cons_idx;
	u64 stranded_work_bitmap;
	u64 head_wait_tsc;
	u64 next_timer_tsc;
	u64 guest_heartbeat;
	u8 pad[16];
} __aligned(64);

struct pylon_vcpu_slot {
	/* Written by the guest (state: RUNNING -> PARKING only) and the VMM. */
	u8 state;
	u8 in_critical_section;
	u8 pad_guest[6];
	u64 last_yield_tsc;
	u64 yield_count;
	u64 park_timer_deadline_tsc;
	/* Written by the host. */
	u8 revoke_requested;
	u8 assigned_pcpu;
	u8 park_on_boot;
	u8 pad_host;
	u32 host_tid;
	u64 revoke_deadline_tsc;
	u64 forced_preempt_count;
	u64 guest_tsc_offset;
} __aligned(64);

struct pylon_shared_page {
	struct pylon_header header;
	struct pylon_telemetry telemetry;
	struct pylon_vcpu_slot vcpus[PYLON_MAX_VCPUS];
	u8 tail_pad[3968];
} __aligned(4096);

static_assert(sizeof(struct pylon_header) == 64);
static_assert(sizeof(struct pylon_telemetry) == 64);
static_assert(sizeof(struct pylon_vcpu_slot) == 64);
static_assert(sizeof(struct pylon_shared_page) == PYLON_SHARED_PAGE_SIZE);
static_assert(offsetof(struct pylon_shared_page, telemetry) == 64);
static_assert(offsetof(struct pylon_shared_page, vcpus) == 128);
static_assert(offsetof(struct pylon_telemetry, stranded_work_bitmap) == 8);
static_assert(offsetof(struct pylon_telemetry, guest_heartbeat) == 32);
static_assert(offsetof(struct pylon_vcpu_slot, in_critical_section) == 1);
static_assert(offsetof(struct pylon_vcpu_slot, park_timer_deadline_tsc) == 24);
static_assert(offsetof(struct pylon_vcpu_slot, revoke_requested) == 32);
static_assert(offsetof(struct pylon_vcpu_slot, park_on_boot) == 34);
static_assert(offsetof(struct pylon_vcpu_slot, host_tid) == 36);
static_assert(offsetof(struct pylon_vcpu_slot, revoke_deadline_tsc) == 40);
static_assert(offsetof(struct pylon_vcpu_slot, forced_preempt_count) == 48);
static_assert(offsetof(struct pylon_vcpu_slot, guest_tsc_offset) == 56);

#endif /* PV_PYLON_ABI_H */
