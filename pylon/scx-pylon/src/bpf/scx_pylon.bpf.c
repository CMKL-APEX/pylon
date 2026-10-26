// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
/*
 * scx_pylon.bpf.c - Zero-Syscall sched_ext Dispatcher for Pylon
 *
 * Bypasses standard Linux sched_fair (EEVDF) wakeup latency and runqueue lock
 * contention when handing off a physical core (pCPU) from a Donor vCPU thread
 * to a Recipient vCPU thread.
 */

#include <linux/types.h>
#include <linux/bpf.h>

#define SEC(name) __attribute__((section(name), used))
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name
#define __ksym __attribute__((section(".ksyms")))
#define __weak __attribute__((weak))

#ifndef BPF_F_MMAPABLE
#define BPF_F_MMAPABLE (1U << 10)
#endif

#define MAX_PCPUS 256

/* sched_ext DSQ and slice constants */
#define SCX_DSQ_FLAG_BUILTIN (1ULL << 63)
#define SCX_DSQ_LOCAL (SCX_DSQ_FLAG_BUILTIN | 2ULL)
#define SCX_SLICE_DFL 20000000ULL /* 20 ms default slice */
#define SCX_SLICE_INF (~0ULL)     /* Infinite slice for dedicated pCPU owner */

/* sched_ext kick flags */
#define SCX_KICK_IDLE (1ULL << 0)
#define SCX_KICK_PREEMPT (1ULL << 1)

/* Flags in struct pcpu_owner_entry */
#define PCPU_OWNER_FLAG_ACTIVE (1U << 0)
#define PCPU_OWNER_FLAG_PREEMPT (1U << 1)

/* Minimal task_struct representation for BPF CO-RE / sched_ext hooks */
struct task_struct {
    int pid;
    int tgid;
    unsigned int flags;
} __attribute__((preserve_access_index));

/* BPF helper declarations */
static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)BPF_FUNC_map_lookup_elem;

/* sched_ext kfunc declarations (weak ksym externs for BPF compilation & BTF emission) */
extern void scx_bpf_dsq_insert(struct task_struct *p, __u64 dsq_id, __u64 slice, __u64 enq_flags) __weak __ksym;
extern void scx_bpf_kick_cpu(__s32 cpu, __u64 flags) __weak __ksym;
extern struct task_struct *bpf_task_from_pid(__s32 pid) __weak __ksym;
extern void bpf_task_release(struct task_struct *p) __weak __ksym;

struct pcpu_owner_entry {
    __u32 target_tid;
    __u32 flags;
};

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 256); // Indexed by physical CPU ID
    __uint(map_flags, BPF_F_MMAPABLE);
    __type(key, __u32);
    __type(value, struct pcpu_owner_entry);
} pylon_pcpu_owners SEC(".maps");

/*
 * scx_ops.select_cpu(p, prev_cpu, wake_flags)
 *
 * If task p->pid matches a target_tid in pylon_pcpu_owners, return that
 * assigned pCPU and insert directly into SCX_DSQ_LOCAL.
 */
SEC("struct_ops/pylon_select_cpu")
__s32 pylon_select_cpu(struct task_struct *p, __s32 prev_cpu, __u64 wake_flags)
{
    __u32 task_tid = (__u32)p->pid;

    /* Fast check: does prev_cpu already belong to this vCPU thread? */
    if (prev_cpu >= 0 && prev_cpu < MAX_PCPUS) {
        __u32 key = (__u32)prev_cpu;
        struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
        if (entry && entry->target_tid == task_tid && task_tid != 0) {
            scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_INF, wake_flags);
            return prev_cpu;
        }
    }

    /* Bounded scan over physical CPU slots to locate assigned pCPU for p->pid */
    #pragma clang loop unroll(disable)
    for (__u32 cpu = 0; cpu < MAX_PCPUS; cpu++) {
        __u32 key = cpu;
        struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
        if (entry && entry->target_tid == task_tid && task_tid != 0) {
            scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_INF, wake_flags);
            if ((__s32)cpu != prev_cpu) {
                scx_bpf_kick_cpu((__s32)cpu, SCX_KICK_PREEMPT);
            }
            return (__s32)cpu;
        }
    }

    return prev_cpu;
}

/*
 * scx_ops.dispatch(cpu, prev)
 *
 * Look up pylon_pcpu_owners[cpu]. If target_tid != 0, dispatch the matching
 * vCPU task directly to the local CPU queue; if prev is a donor vCPU whose
 * target_tid was replaced (e.g., deadline expired), preempt prev immediately.
 */
SEC("struct_ops/pylon_dispatch")
void pylon_dispatch(__s32 cpu, struct task_struct *prev)
{
    if (cpu < 0 || cpu >= MAX_PCPUS) {
        return;
    }

    __u32 key = (__u32)cpu;
    struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
    if (!entry || entry->target_tid == 0) {
        return;
    }

    __u32 target_tid = entry->target_tid;

    /*
     * If prev was running on this CPU and its TID no longer matches the
     * current owner target_tid (e.g., donor vCPU deadline expired and pylond
     * reassigned the pCPU to a recipient vCPU), preempt prev immediately.
     */
    if (prev && (__u32)prev->pid != target_tid) {
        scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT);
    }

    /* Acquire the target vCPU task by kernel TID (pid) and dispatch to SCX_DSQ_LOCAL */
    struct task_struct *target = bpf_task_from_pid((__s32)target_tid);
    if (target) {
        scx_bpf_dsq_insert(target, SCX_DSQ_LOCAL, SCX_SLICE_INF, 0);
        bpf_task_release(target);
    }
}

struct sched_ext_ops {
    __s32 (*select_cpu)(struct task_struct *p, __s32 prev_cpu, __u64 wake_flags);
    void (*dispatch)(__s32 cpu, struct task_struct *prev);
    __u64 flags;
    char name[128];
};

SEC(".struct_ops.link")
struct sched_ext_ops pylon_ops = {
    .select_cpu = (void *)pylon_select_cpu,
    .dispatch = (void *)pylon_dispatch,
    .name = "pylon",
};

char _license[] SEC("license") = "GPL";
