// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//
/*
 * scx_pylon.bpf.c - Zero-Syscall sched_ext Dispatcher for Pylon
 *
 * Bypasses standard Linux sched_fair (EEVDF) wakeup latency and runqueue lock
 * contention when handing off a physical core (pCPU) from a Donor vCPU thread
 * to a Recipient vCPU thread.
 *
 * It also keeps SMT siblings apart: while a CPU runs a managed vCPU thread,
 * its SMT siblings only run vCPU threads of the same VMM process or nothing.
 * This replaces kernel core scheduling (PR_SCHED_CORE) for managed vCPUs.
 */

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

#define MAX_PCPUS 256
/* Longest SMT sibling ring followed in pylon_smt_next */
#define MAX_SMT 8
/* Most tasks examined per dispatch in the vCPU queue */
#define MAX_VCPU_SCAN 32
/* VM slots in pylon_vm_vcpus, and vCPUs per VM (MAX_TRACKED_VMS and MAX_VCPUS in pylond) */
#define MAX_VM_SLOTS 64
#define MAX_VM_VCPUS 64
#define VM_SLOT_NONE 0xffffU

/* sched_ext DSQ and slice constants */
#define SCX_DSQ_FLAG_BUILTIN (1ULL << 63)
#define SCX_DSQ_LOCAL (SCX_DSQ_FLAG_BUILTIN | 2ULL)
#define SCX_SLICE_DFL 20000000ULL /* 20 ms default slice */
/*
 * The owner of a pCPU gets its CPU back after each slice unless a task that
 * can only run there is waiting. A finite slice lets the kernel run such a
 * task (a per-CPU kworker, say) before the watchdog ejects scx_pylon.
 */
#define PYLON_OWNER_SLICE SCX_SLICE_DFL
/*
 * Slice of a CPU shared between its owner and woken parked vCPUs of the same
 * VM (spec 08 §4.3). Both get it in turns while any of them waits.
 */
#define PYLON_SHARE_SLICE 100000ULL /* 100 us */

/* sched_ext kick flags */
#define SCX_KICK_IDLE (1ULL << 0)
#define SCX_KICK_PREEMPT (1ULL << 1)

/*
 * Custom DSQs. DSQ `cpu` (0..MAX_PCPUS-1) holds the owner of that pCPU,
 * PYLON_VCPU_DSQ holds managed vCPU threads away from their pCPU, and
 * PYLON_HOST_DSQ holds every other task.
 */
#define PYLON_HOST_DSQ 1024ULL
#define PYLON_VCPU_DSQ 1025ULL
/* DSQ PYLON_VM_DSQ + slot holds that VM's parked vCPUs that woke (spec 08 §4.3) */
#define PYLON_VM_DSQ 2048ULL

/*
 * Per-CPU claims: what a CPU runs or is about to run. CLAIM_NONE is idle,
 * CLAIM_HOST is a task that isn't a managed vCPU, CLAIM_PINNED is such a
 * task allowed on only this CPU, and any other value is the tgid of the VMM
 * process whose vCPU thread runs there.
 */
#define CLAIM_NONE 0U
#define CLAIM_HOST 0xffffffffU
#define CLAIM_PINNED 0xfffffffeU

#define BPF_FOR_EACH_ITER (&___it)

/* scx_bpf_exit code when core scheduling turns on (pylon#15) */
#define PYLON_EXIT_CORE_SCHED 1

/* prctl(2) core scheduling commands, from include/uapi/linux/prctl.h */
#define PR_SCHED_CORE 62
#define PR_SCHED_CORE_GET 0

#define EPERM 1
#define ENODEV 19

/* Values of pylon_guard.deny: what a request for a cookie gets */
#define PYLON_DENY_OFF 0
#define PYLON_DENY_EPERM 1
#define PYLON_DENY_ENODEV 2

/* Flags in struct pcpu_owner_entry */
#define PCPU_OWNER_FLAG_ACTIVE (1U << 0)
#define PCPU_OWNER_FLAG_PREEMPT (1U << 1)
/* Bits 16-31 of the flags: the owner's VM slot plus one, or 0 if unknown */
#define PCPU_OWNER_SLOT_SHIFT 16

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
 * SMT sibling ring, written by userspace before attaching: the next CPU on
 * the same core, or the CPU itself when it has no siblings.
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, MAX_PCPUS);
    __type(key, __u32);
    __type(value, __u32);
} pylon_smt_next SEC(".maps");

/* A registered vCPU thread: its VMM's tgid, its VM's slot and its index. */
struct pylon_vcpu_id {
    __u32 tgid;
    /* Slot in pylon_vm_vcpus, or VM_SLOT_NONE */
    __u16 slot;
    __u16 vcpu;
};

/*
 * vCPU threads of registered VMs, written by pylond, by thread ID. A listed
 * thread is a managed vCPU even before it owns a pCPU, so it is SMT-isolated
 * from the start.
 */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __u32);
    __type(value, struct pylon_vcpu_id);
} pylon_vcpu_dom SEC(".maps");

/*
 * Per-VM vCPU bitmasks, by VM slot (spec 08 §4.3). pylond's engine sets a
 * vCPU's `parked` bit while it is PARKED with no pCPU assigned; scx_pylon sets
 * `woken` while such a vCPU is runnable, for pylond to read.
 */
struct pylon_vm_vcpus {
    __u64 parked;
    __u64 woken;
};

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, MAX_VM_SLOTS);
    __uint(map_flags, BPF_F_MMAPABLE);
    __type(key, __u32);
    __type(value, struct pylon_vm_vcpus);
} pylon_vm_vcpus SEC(".maps");

struct pylon_task_ctx {
    /* tgid of the VMM process once the task has owned a pCPU or pylond has
     * listed it in pylon_vcpu_dom, else 0 */
    __u32 dom;
    /* Last pCPU found owned by the task */
    __s32 owned_cpu;
    /* VM slot and vCPU index from pylon_vcpu_dom; slot is VM_SLOT_NONE until listed */
    __u16 slot;
    __u16 vcpu;
};

struct {
    __uint(type, BPF_MAP_TYPE_TASK_STORAGE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int);
    __type(value, struct pylon_task_ctx);
} pylon_task_ctx SEC(".maps");

/*
 * Core scheduling guard (pylon#15). On kernels without the sched_ext
 * core-scheduling fixes, scx_pylon must not run once any task has a core
 * scheduling cookie.
 */
struct pylon_guard {
    /* tgid of the first task seen asking for a cookie, or 0 */
    __u32 cookie_tgid;
    /* Set once scx_pylon has asked the kernel to disable it */
    __u32 exiting;
    /* PYLON_DENY_*, written by userspace before attaching */
    __u32 deny;
};

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct pylon_guard);
} pylon_guard SEC(".maps");

static char core_sched_fmt[] =
    "core scheduling turned on by pid %llu; this kernel lacks the sched_ext fixes";

__u32 cpu_claim[MAX_PCPUS];
/*
 * Per CPU: set when the owner was the last managed task to run there, so the
 * next turn goes to a woken parked vCPU of its VM (spec 08 §4.3). Host tasks,
 * such as per-CPU kworkers, run between turns and don't change it.
 */
__u32 woken_turn[MAX_PCPUS];
/* Next VM queue that dispatch checks for vCPUs left without a CPU of their VM */
__u32 vm_dsq_cursor;

static __always_inline bool owns_cpu(struct task_struct *p, s32 cpu)
{
    if (cpu < 0 || cpu >= MAX_PCPUS || p->pid == 0) {
        return false;
    }
    __u32 key = (__u32)cpu;
    struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
    return entry && entry->target_tid == (__u32)p->pid;
}

/*
 * Whether @p can't leave its CPU. BPF programs run with migration disabled,
 * so a count of one on the current task is the program's own.
 */
static __always_inline bool is_migration_disabled(const struct task_struct *p)
{
    if (p->migration_disabled == 1) {
        return bpf_get_current_task_btf() != p;
    }
    return p->migration_disabled;
}

static __always_inline __u32 owner_tid(s32 cpu)
{
    if (cpu < 0 || cpu >= MAX_PCPUS) {
        return 0;
    }
    __u32 key = (__u32)cpu;
    struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
    return entry ? entry->target_tid : 0;
}

static __always_inline s32 smt_next(s32 cpu)
{
    __u32 key = (__u32)cpu;
    __u32 *next = bpf_map_lookup_elem(&pylon_smt_next, &key);
    if (!next || *next >= MAX_PCPUS) {
        return cpu;
    }
    return (s32)*next;
}

static __always_inline struct pylon_guard *guard(void)
{
    __u32 key = 0;
    return bpf_map_lookup_elem(&pylon_guard, &key);
}

/*
 * Asks the kernel to disable scx_pylon once a task has asked for a core
 * scheduling cookie. Dispatch goes on as usual until the kernel does.
 */
static void exit_on_core_sched(void)
{
    struct pylon_guard *g = guard();

    if (!g || !g->cookie_tgid || __atomic_exchange_n(&g->exiting, 1, __ATOMIC_SEQ_CST)) {
        return;
    }
    unsigned long long data[1] = {g->cookie_tgid};
    scx_bpf_exit_bstr(PYLON_EXIT_CORE_SCHED, core_sched_fmt, data, sizeof(data));
}

static __always_inline struct pylon_task_ctx *task_ctx(struct task_struct *p)
{
    return bpf_task_storage_get(&pylon_task_ctx, p, 0, 0);
}

static __always_inline __u32 task_claim(struct task_struct *p)
{
    struct pylon_task_ctx *tctx = task_ctx(p);
    return tctx && tctx->dom ? tctx->dom : CLAIM_HOST;
}

/* Marks @p as a managed vCPU thread that owns @cpu. */
static __always_inline void mark_owner(struct task_struct *p, s32 cpu)
{
    struct pylon_task_ctx *tctx =
        bpf_task_storage_get(&pylon_task_ctx, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
    if (tctx) {
        if (!tctx->dom) {
            tctx->slot = VM_SLOT_NONE;
        }
        tctx->dom = (__u32)p->tgid;
        tctx->owned_cpu = cpu;
    }
}

/*
 * Returns @p's context if @p is a managed vCPU thread, tagging it on first
 * sight when pylond lists it in pylon_vcpu_dom. The tgid check skips a stale
 * entry whose thread ID now belongs to another process.
 */
static struct pylon_task_ctx *vcpu_ctx(struct task_struct *p)
{
    struct pylon_task_ctx *tctx = task_ctx(p);
    if (tctx && tctx->dom && tctx->slot != VM_SLOT_NONE) {
        return tctx;
    }

    /* Not listed yet, or tagged as an owner before pylond listed it. */
    __u32 tid = (__u32)p->pid;
    struct pylon_vcpu_id *id = bpf_map_lookup_elem(&pylon_vcpu_dom, &tid);
    if (!id || id->tgid != (__u32)p->tgid) {
        return tctx && tctx->dom ? tctx : NULL;
    }
    if (!tctx) {
        tctx = bpf_task_storage_get(&pylon_task_ctx, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
        if (!tctx) {
            return NULL;
        }
        tctx->owned_cpu = -1;
    }
    tctx->dom = id->tgid;
    tctx->slot = id->slot;
    tctx->vcpu = id->vcpu;
    return tctx;
}

static __always_inline struct pylon_vm_vcpus *vm_vcpus(__u16 slot)
{
    __u32 key = slot;
    return slot < MAX_VM_SLOTS ? bpf_map_lookup_elem(&pylon_vm_vcpus, &key) : NULL;
}

/* Whether pylond reports @tctx's vCPU as parked with no pCPU assigned. */
static __always_inline bool is_parked(const struct pylon_task_ctx *tctx)
{
    struct pylon_vm_vcpus *v = vm_vcpus(tctx->slot);
    return v && tctx->vcpu < MAX_VM_VCPUS &&
           (*(volatile __u64 *)&v->parked & (1ULL << tctx->vcpu));
}

/* Sets or clears @tctx's vCPU's bit in its VM's woken mask. */
static __always_inline void set_woken(const struct pylon_task_ctx *tctx, bool woken)
{
    struct pylon_vm_vcpus *v = vm_vcpus(tctx->slot);
    if (!v || tctx->vcpu >= MAX_VM_VCPUS) {
        return;
    }
    __u64 bit = 1ULL << tctx->vcpu;
    if (woken) {
        __sync_fetch_and_or(&v->woken, bit);
    } else if (*(volatile __u64 *)&v->woken & bit) {
        __sync_fetch_and_and(&v->woken, ~bit);
    }
}

/* The VM slot of @cpu's owner, from its owner entry, or VM_SLOT_NONE. */
static __always_inline __u16 owner_slot(s32 cpu)
{
    if (cpu < 0 || cpu >= MAX_PCPUS) {
        return VM_SLOT_NONE;
    }
    __u32 key = (__u32)cpu;
    struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
    __u32 slot = entry && entry->target_tid ? entry->flags >> PCPU_OWNER_SLOT_SHIFT : 0;
    return slot ? (__u16)(slot - 1) : VM_SLOT_NONE;
}

/* Whether the next turn on @cpu goes to a woken parked vCPU. */
static __always_inline bool is_woken_turn(s32 cpu)
{
    /* Unsigned, as in read_claim(). */
    __u32 i = (__u32)cpu;

    return i < MAX_PCPUS && *(volatile __u32 *)&woken_turn[i];
}

static __always_inline void set_woken_turn(s32 cpu, bool turn)
{
    __u32 i = (__u32)cpu;

    if (i < MAX_PCPUS) {
        *(volatile __u32 *)&woken_turn[i] = turn;
    }
}

static __always_inline __u32 read_claim(s32 cpu)
{
    /* Unsigned: clang folds a signed check, and the verifier then sees an
     * unbounded index. A negative cpu becomes large and is refused too. */
    __u32 i = (__u32)cpu;

    if (i >= MAX_PCPUS) {
        return CLAIM_NONE;
    }
    return *(volatile __u32 *)&cpu_claim[i];
}

/*
 * Sets @cpu's claim. When a managed claim ends, wakes the SMT siblings it
 * kept idle so they look for work again.
 */
static void set_claim(s32 cpu, __u32 claim)
{
    __u32 i = (__u32)cpu;

    if (i >= MAX_PCPUS) {
        return;
    }

    __u32 old = __atomic_exchange_n(&cpu_claim[i], claim, __ATOMIC_SEQ_CST);
    if (old == CLAIM_NONE || old == CLAIM_HOST || old == claim) {
        return;
    }

    s32 sib = cpu;
    for (int i = 0; i < MAX_SMT; i++) {
        sib = smt_next(sib);
        if (sib == cpu) {
            break;
        }
        scx_bpf_kick_cpu(sib, SCX_KICK_IDLE);
    }
}

/*
 * Whether @claim on @cpu yields to @other on its SMT sibling @sib. A managed
 * claim beats a host claim, and between two managed VMs the lower-numbered
 * CPU wins. A pinned host task can't be moved away, so a managed claim
 * yields to it, unless it is the pCPU's @owner: an owner that yielded to a
 * busy pinned task would wait until the watchdog fired. Host and pinned
 * claims don't conflict. Only `running` sets a pinned claim, without checking
 * siblings, so among claims that are checked the order is total, and two
 * siblings never both back off.
 */
static __always_inline bool yields_to(s32 cpu, __u32 claim, s32 sib, __u32 other, bool owner)
{
    if (other == CLAIM_NONE || other == claim) {
        return false;
    }
    if (claim == CLAIM_HOST || claim == CLAIM_PINNED) {
        return other != CLAIM_HOST && other != CLAIM_PINNED;
    }
    if (other == CLAIM_HOST) {
        return false;
    }
    if (other == CLAIM_PINNED) {
        return !owner;
    }
    return sib < cpu;
}

/* Whether a sibling of @cpu holds a claim that @claim yields to. */
static bool fenced(s32 cpu, __u32 claim, bool owner)
{
    s32 sib = cpu;

    for (int i = 0; i < MAX_SMT; i++) {
        sib = smt_next(sib);
        if (sib == cpu) {
            break;
        }
        if (yields_to(cpu, claim, sib, read_claim(sib), owner)) {
            return true;
        }
    }
    return false;
}

/* Kicks the SMT siblings of @cpu whose claims yield to @claim, so they back off. */
static void kick_siblings(s32 cpu, __u32 claim)
{
    s32 sib = cpu;

    for (int i = 0; i < MAX_SMT; i++) {
        sib = smt_next(sib);
        if (sib == cpu) {
            break;
        }
        if (yields_to(sib, read_claim(sib), cpu, claim, false)) {
            scx_bpf_kick_cpu(sib, SCX_KICK_PREEMPT);
        }
    }
}

/*
 * Publishes @claim for @cpu, then checks its SMT siblings. Publishing before
 * checking means two siblings claiming at once see each other. On success,
 * siblings whose claims yield to @claim are kicked so they back off. On
 * failure the claim is withdrawn. @owner is set when @cpu's owner claims it.
 */
static bool claim_cpu(s32 cpu, __u32 claim, bool owner)
{
    if (cpu < 0 || cpu >= MAX_PCPUS) {
        return true;
    }

    set_claim(cpu, claim);
    if (fenced(cpu, claim, owner)) {
        set_claim(cpu, CLAIM_NONE);
        return false;
    }
    kick_siblings(cpu, claim);
    return true;
}

/* Wakes an idle CPU that may run @p when @p's own CPU can't. */
static void kick_idle_cpu(struct task_struct *p, __u32 claim)
{
    for (int i = 0; i < MAX_SMT; i++) {
        s32 cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
        if (cpu < 0) {
            return;
        }
        scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);
        if (!fenced(cpu, claim, false)) {
            return;
        }
    }
}

/*
 * A CPU that @slot's VM owns and @p may run on, or -1. A woken parked vCPU
 * of that VM shares it with the owner (spec 08 §4.3).
 */
static s32 vm_owned_cpu(struct task_struct *p, __u16 slot)
{
    s32 cpu;

    bpf_for(cpu, 0, MAX_PCPUS) {
        if (owner_slot(cpu) == slot && bpf_cpumask_test_cpu(cpu, p->cpus_ptr)) {
            return cpu;
        }
    }
    return -1;
}

/*
 * scx_ops.select_cpu(p, prev_cpu, wake_flags)
 *
 * If task p->pid matches a target_tid in pylon_pcpu_owners, return that
 * assigned pCPU; enqueue then puts it on that pCPU's owner queue.
 */
SEC("struct_ops/pylon_select_cpu")
s32 BPF_PROG(pylon_select_cpu, struct task_struct *p, s32 prev_cpu, u64 wake_flags)
{
    __u32 task_tid = (__u32)p->pid;

    /* Fast check: does prev_cpu already belong to this vCPU thread? */
    if (owns_cpu(p, prev_cpu)) {
        mark_owner(p, prev_cpu);
        return prev_cpu;
    }

    /* Bounded scan over physical CPU slots to locate assigned pCPU for p->pid */
    #pragma clang loop unroll(disable)
    for (__u32 cpu = 0; cpu < MAX_PCPUS; cpu++) {
        __u32 key = cpu;
        struct pcpu_owner_entry *entry = bpf_map_lookup_elem(&pylon_pcpu_owners, &key);
        if (entry && entry->target_tid == task_tid && task_tid != 0) {
            mark_owner(p, (s32)cpu);
            return (s32)cpu;
        }
    }

    bool is_idle = false;
    return scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
}

/*
 * scx_ops.enqueue(p, enq_flags)
 *
 * The owner of a pCPU goes on that pCPU's queue and preempts whatever runs
 * there. Other managed vCPU threads go on the vCPU queue and wake their own
 * pCPU if they own one. A task that can't migrate stays on its CPU, as does
 * a host task allowed on only one CPU: on the host queue, it would wait
 * behind that CPU's owner, or behind a sibling's claim, until the watchdog
 * fires. Both run whatever the siblings run. Everything else goes on the host
 * queue.
 */
SEC("struct_ops/pylon_enqueue")
void BPF_PROG(pylon_enqueue, struct task_struct *p, u64 enq_flags)
{
    s32 cpu = scx_bpf_task_cpu(p);
    struct pylon_task_ctx *tctx;

    if (is_migration_disabled(p)) {
        /* Short and pinned: it runs whatever the siblings run. */
        scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, enq_flags);
    } else if (owns_cpu(p, cpu)) {
        mark_owner(p, cpu);
        if ((tctx = task_ctx(p))) {
            set_woken(tctx, false);
        }
        scx_bpf_dsq_insert(p, (u64)cpu, PYLON_OWNER_SLICE, enq_flags);
        scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT);
    } else if ((tctx = vcpu_ctx(p)) && !owns_cpu(p, tctx->owned_cpu) && is_parked(tctx)) {
        /*
         * A parked vCPU that KVM woke for an interrupt (spec 08 §3.2). It
         * runs only on a CPU its VM owns, in turns with the owner. Its woken
         * bit tells pylond it wants to run. A VM that owns no CPU has nowhere
         * to share, so its vCPU runs as any other managed vCPU.
         */
        s32 shared = vm_owned_cpu(p, tctx->slot);

        set_woken(tctx, true);
        if (shared >= 0) {
            scx_bpf_dsq_insert(p, PYLON_VM_DSQ + tctx->slot, PYLON_SHARE_SLICE, enq_flags);
            scx_bpf_kick_cpu(shared, SCX_KICK_PREEMPT);
        } else {
            scx_bpf_dsq_insert(p, PYLON_VCPU_DSQ, SCX_SLICE_DFL, enq_flags);
            if (fenced(cpu, tctx->dom, false)) {
                kick_idle_cpu(p, tctx->dom);
            }
        }
    } else if (tctx) {
        scx_bpf_dsq_insert(p, PYLON_VCPU_DSQ, SCX_SLICE_DFL, enq_flags);
        if (owns_cpu(p, tctx->owned_cpu)) {
            scx_bpf_kick_cpu(tctx->owned_cpu, SCX_KICK_PREEMPT);
        } else if (fenced(cpu, tctx->dom, false)) {
            kick_idle_cpu(p, tctx->dom);
        }
    } else if (p->nr_cpus_allowed == 1) {
        scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, enq_flags);
        scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);
    } else {
        scx_bpf_dsq_insert(p, PYLON_HOST_DSQ, SCX_SLICE_DFL, enq_flags);
        if (fenced(cpu, CLAIM_HOST, false)) {
            kick_idle_cpu(p, CLAIM_HOST);
        }
    }

    /*
     * ENQ_LAST: this CPU has already decided to idle with @p runnable.
     * Kick it so it schedules again once idle.
     */
    if (enq_flags & SCX_ENQ_LAST) {
        scx_bpf_kick_cpu(cpu, 0);
    }
}

/*
 * Dispatches this pCPU's owner with @slice if it is queued, and moves any
 * other task left on the owner queue (its ownership changed) to the queue it
 * now belongs on. Returns true if the owner was dispatched.
 */
static bool dispatch_owner(s32 cpu, u64 slice)
{
    struct task_struct *p;
    __u32 tid = owner_tid(cpu);
    bool dispatched = false;

    bpf_for_each(scx_dsq, p, (u64)cpu, 0) {
        if (!dispatched && tid != 0 && (__u32)p->pid == tid) {
            if (claim_cpu(cpu, task_claim(p), true)) {
                scx_bpf_dsq_move_set_slice(BPF_FOR_EACH_ITER, slice);
                dispatched = scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0);
            }
            continue;
        }
        if ((__u32)p->pid != tid) {
            u64 dsq = task_claim(p) == CLAIM_HOST ? PYLON_HOST_DSQ : PYLON_VCPU_DSQ;
            scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, dsq, 0);
        }
    }
    return dispatched;
}

/*
 * Dispatches the first task on the vCPU queue that may run on @cpu next to
 * its SMT siblings. A thread that owns another pCPU is left for that pCPU,
 * which enqueue has kicked.
 */
static bool dispatch_vcpu(s32 cpu)
{
    struct task_struct *p;
    int scanned = 0;

    bpf_for_each(scx_dsq, p, PYLON_VCPU_DSQ, 0) {
        if (++scanned > MAX_VCPU_SCAN) {
            break;
        }
        if (!bpf_cpumask_test_cpu(cpu, p->cpus_ptr)) {
            continue;
        }
        struct pylon_task_ctx *tctx = task_ctx(p);
        if (!tctx) {
            continue;
        }
        if (tctx->owned_cpu != cpu && owns_cpu(p, tctx->owned_cpu)) {
            continue;
        }
        if (claim_cpu(cpu, tctx->dom ? tctx->dom : CLAIM_HOST, false) &&
            scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0)) {
            return true;
        }
    }
    return false;
}

/*
 * Dispatches the first woken parked vCPU of @slot's VM to @cpu, which that VM
 * owns, for one PYLON_SHARE_SLICE. They share the VM's domain claim.
 */
static bool dispatch_woken(s32 cpu, __u16 slot)
{
    struct task_struct *p;

    bpf_for_each(scx_dsq, p, PYLON_VM_DSQ + slot, 0) {
        if (!bpf_cpumask_test_cpu(cpu, p->cpus_ptr)) {
            continue;
        }
        return claim_cpu(cpu, task_claim(p), true) &&
               scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0);
    }
    return false;
}

/*
 * Checks one VM queue per call, in turn. If that VM no longer owns a CPU
 * (its last core was revoked, or it has none), no CPU would drain the queue
 * and the watchdog would eject scx_pylon, so its vCPUs move to the vCPU
 * queue and run as in P0.
 */
static void rescue_stranded_vcpus(void)
{
    __u32 slot = __sync_fetch_and_add(&vm_dsq_cursor, 1) % MAX_VM_SLOTS;
    struct task_struct *p;
    s32 cpu;

    if (scx_bpf_dsq_nr_queued(PYLON_VM_DSQ + slot) <= 0) {
        return;
    }
    bpf_for(cpu, 0, MAX_PCPUS) {
        if (owner_slot(cpu) == slot) {
            return;
        }
    }
    bpf_for_each(scx_dsq, p, PYLON_VM_DSQ + slot, 0) {
        scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, PYLON_VCPU_DSQ, 0);
    }
}

/*
 * scx_ops.dispatch(cpu, prev)
 *
 * Run the pCPU's owner if it is queued. Otherwise run a vCPU thread or a
 * host task, but only what the SMT siblings' claims allow; if nothing fits,
 * the CPU idles.
 */
SEC("struct_ops/pylon_dispatch")
void BPF_PROG(pylon_dispatch, s32 cpu, struct task_struct *prev)
{
    exit_on_core_sched();
    rescue_stranded_vcpus();

    /*
     * While woken parked vCPUs of the owner's VM wait, the owner and they
     * take PYLON_SHARE_SLICE turns: a woken vCPU goes first after the owner
     * ran, and the owner after a woken vCPU (spec 08 §4.3). `running` keeps
     * track of whose turn it is. @prev can't tell: a per-CPU kworker that
     * runs between turns would hide the owner, and the owner would keep the
     * CPU until the watchdog fired.
     */
    __u16 slot = owner_slot(cpu);
    bool share = slot < MAX_VM_SLOTS && scx_bpf_dsq_nr_queued(PYLON_VM_DSQ + slot) > 0;

    if (share && is_woken_turn(cpu) && dispatch_woken(cpu, slot)) {
        return;
    }
    if (cpu >= 0 && cpu < MAX_PCPUS &&
        dispatch_owner(cpu, share ? PYLON_SHARE_SLICE : PYLON_OWNER_SLICE)) {
        return;
    }
    if (share && dispatch_woken(cpu, slot)) {
        return;
    }
    if (dispatch_vcpu(cpu)) {
        return;
    }
    if (claim_cpu(cpu, CLAIM_HOST, false) && scx_bpf_dsq_move_to_local(PYLON_HOST_DSQ)) {
        return;
    }
    /* Nothing to run: the CPU goes idle. */
    set_claim(cpu, CLAIM_NONE);
}

/*
 * scx_ops.running(p)
 *
 * Record what this CPU runs, including tasks that the kernel queued locally
 * without asking the scheduler. A pinned host task runs without checking
 * its siblings, so it kicks the vCPUs that yield to it. Any other task
 * checks its siblings again, and backs off if one claimed in the meantime.
 * The claim isn't cleared when a task stops:
 * dispatch has already claimed for the next task by then, and the CPU going
 * idle clears it.
 */
SEC("struct_ops/pylon_running")
void BPF_PROG(pylon_running, struct task_struct *p)
{
    __u32 claim = task_claim(p);

    s32 cpu = scx_bpf_task_cpu(p);

    /* The owner hands the next turn to a woken vCPU, and that vCPU back. */
    if (claim != CLAIM_HOST) {
        set_woken_turn(cpu, owns_cpu(p, cpu));
    }

    if (claim == CLAIM_HOST && p->nr_cpus_allowed == 1) {
        claim = CLAIM_PINNED;
    }
    set_claim(cpu, claim);
    if (claim == CLAIM_PINNED) {
        kick_siblings(cpu, claim);
    } else if (claim != CLAIM_NONE && !is_migration_disabled(p) &&
               fenced(cpu, claim, owns_cpu(p, cpu))) {
        /*
         * A sibling claimed after dispatch checked, and its kick landed
         * before @p was current, so it preempted nothing. Without this, @p
         * would run its whole slice next to the sibling.
         */
        scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT);
    }
}

/*
 * scx_ops.quiescent(p, deq_flags)
 *
 * A vCPU that stops being runnable (it halted again, or was parked again)
 * no longer counts as woken.
 */
SEC("struct_ops/pylon_quiescent")
void BPF_PROG(pylon_quiescent, struct task_struct *p, u64 deq_flags)
{
    struct pylon_task_ctx *tctx = task_ctx(p);

    if (tctx && tctx->dom) {
        set_woken(tctx, false);
    }
}

/*
 * scx_ops.update_idle(cpu, idle)
 *
 * An idle CPU claims nothing.
 */
SEC("struct_ops/pylon_update_idle")
void BPF_PROG(pylon_update_idle, s32 cpu, bool idle)
{
    if (idle) {
        set_claim(cpu, CLAIM_NONE);
    }
}

SEC("struct_ops.s/pylon_init")
s32 BPF_PROG(pylon_init)
{
    u32 nr_cpus = scx_bpf_nr_cpu_ids();
    s32 ret;

    if (nr_cpus > MAX_PCPUS) {
        nr_cpus = MAX_PCPUS;
    }
    for (u32 cpu = 0; cpu < nr_cpus && cpu < MAX_PCPUS; cpu++) {
        ret = scx_bpf_create_dsq(cpu, -1);
        if (ret) {
            return ret;
        }
    }
    for (u32 slot = 0; slot < MAX_VM_SLOTS; slot++) {
        ret = scx_bpf_create_dsq(PYLON_VM_DSQ + slot, -1);
        if (ret) {
            return ret;
        }
    }
    ret = scx_bpf_create_dsq(PYLON_VCPU_DSQ, -1);
    if (ret) {
        return ret;
    }
    return scx_bpf_create_dsq(PYLON_HOST_DSQ, -1);
}

SEC("struct_ops/pylon_exit")
void BPF_PROG(pylon_exit, struct scx_exit_info *info)
{
}

/*
 * Loaded only with the guard. sched_core_get() runs whenever a task gets a
 * new cookie, before the first one turns core scheduling on. Disabling
 * scx_pylon then takes until the next dispatch and the kernel's disable
 * work, so this narrows the window but doesn't close it.
 */
SEC("fentry/sched_core_get")
int BPF_PROG(pylon_sched_core_get)
{
    struct pylon_guard *g = guard();

    if (g) {
        __sync_val_compare_and_swap(&g->cookie_tgid, 0, (__u32)(bpf_get_current_pid_tgid() >> 32));
    }
    return 0;
}

/*
 * Loaded only with the guard and a denial. Refuses every PR_SCHED_CORE
 * command except GET, so no task gets a cookie while scx_pylon is attached.
 * task_prctl programs run as modify-return: 0 lets the prctl go on.
 */
SEC("lsm/task_prctl")
int BPF_PROG(pylon_task_prctl, int option, unsigned long arg2, unsigned long arg3,
             unsigned long arg4, unsigned long arg5, int ret)
{
    if (ret) {
        return ret;
    }
    if (option != PR_SCHED_CORE || arg2 == PR_SCHED_CORE_GET) {
        return 0;
    }
    struct pylon_guard *g = guard();
    if (!g) {
        return 0;
    }
    switch (g->deny) {
    case PYLON_DENY_EPERM:
        return -EPERM;
    case PYLON_DENY_ENODEV:
        return -ENODEV;
    default:
        return 0;
    }
}

SEC(".struct_ops.link")
struct sched_ext_ops pylon_ops = {
    .select_cpu = (void *)pylon_select_cpu,
    .enqueue = (void *)pylon_enqueue,
    .dispatch = (void *)pylon_dispatch,
    .running = (void *)pylon_running,
    .quiescent = (void *)pylon_quiescent,
    .update_idle = (void *)pylon_update_idle,
    .init = (void *)pylon_init,
    .exit = (void *)pylon_exit,
    /*
     * Without ENQ_LAST, a CPU with nothing else to run keeps its previous
     * task even when a sibling's claim forbids it. ENQ_EXITING and
     * ENQ_MIGRATION_DISABLED route those tasks through enqueue too, which
     * kicks the CPU; kernels without upstream 7a9df7667963 otherwise leave
     * them stranded on an idle CPU. KEEP_BUILTIN_IDLE keeps the kernel's
     * idle tracking alongside update_idle.
     */
    .flags = SCX_OPS_KEEP_BUILTIN_IDLE | SCX_OPS_ENQ_LAST | SCX_OPS_ENQ_EXITING |
             SCX_OPS_ENQ_MIGRATION_DISABLED,
    .name = "pylon",
};

char _license[] SEC("license") = "GPL";
