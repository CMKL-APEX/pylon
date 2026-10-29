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

/* sched_ext DSQ and slice constants */
#define SCX_DSQ_FLAG_BUILTIN (1ULL << 63)
#define SCX_DSQ_LOCAL (SCX_DSQ_FLAG_BUILTIN | 2ULL)
#define SCX_SLICE_DFL 20000000ULL /* 20 ms default slice */
#define SCX_SLICE_INF (~0ULL)     /* Infinite slice for dedicated pCPU owner */

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

/*
 * Per-CPU claims: what a CPU runs or is about to run. CLAIM_NONE is idle,
 * CLAIM_HOST is a task that isn't a managed vCPU, and any other value is the
 * tgid of the VMM process whose vCPU thread runs there.
 */
#define CLAIM_NONE 0U
#define CLAIM_HOST 0xffffffffU

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

struct pylon_task_ctx {
    /* tgid of the VMM process once the task has owned a pCPU, else 0 */
    __u32 dom;
    /* Last pCPU found owned by the task */
    __s32 owned_cpu;
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
        tctx->dom = (__u32)p->tgid;
        tctx->owned_cpu = cpu;
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
 * CPU wins. The order is total, so two siblings never both back off.
 */
static __always_inline bool yields_to(s32 cpu, __u32 claim, s32 sib, __u32 other)
{
    if (other == CLAIM_NONE || other == claim) {
        return false;
    }
    if (claim == CLAIM_HOST) {
        return true;
    }
    if (other == CLAIM_HOST) {
        return false;
    }
    return sib < cpu;
}

/* Whether a sibling of @cpu holds a claim that @claim yields to. */
static bool fenced(s32 cpu, __u32 claim)
{
    s32 sib = cpu;

    for (int i = 0; i < MAX_SMT; i++) {
        sib = smt_next(sib);
        if (sib == cpu) {
            break;
        }
        if (yields_to(cpu, claim, sib, read_claim(sib))) {
            return true;
        }
    }
    return false;
}

/*
 * Publishes @claim for @cpu, then checks its SMT siblings. Publishing before
 * checking means two siblings claiming at once see each other. On success,
 * siblings running something incompatible are kicked so they back off. On
 * failure the claim is withdrawn.
 */
static bool claim_cpu(s32 cpu, __u32 claim)
{
    if (cpu < 0 || cpu >= MAX_PCPUS) {
        return true;
    }

    set_claim(cpu, claim);
    if (fenced(cpu, claim)) {
        set_claim(cpu, CLAIM_NONE);
        return false;
    }

    s32 sib = cpu;
    for (int i = 0; i < MAX_SMT; i++) {
        sib = smt_next(sib);
        if (sib == cpu) {
            break;
        }
        __u32 other = read_claim(sib);
        if (other != CLAIM_NONE && other != claim) {
            scx_bpf_kick_cpu(sib, SCX_KICK_PREEMPT);
        }
    }
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
        if (!fenced(cpu, claim)) {
            return;
        }
    }
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
 * pCPU if they own one. A task that can't migrate stays on its CPU.
 * Everything else goes on the host queue.
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
        scx_bpf_dsq_insert(p, (u64)cpu, SCX_SLICE_INF, enq_flags);
        scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT);
    } else if ((tctx = task_ctx(p)) && tctx->dom) {
        scx_bpf_dsq_insert(p, PYLON_VCPU_DSQ, SCX_SLICE_DFL, enq_flags);
        if (owns_cpu(p, tctx->owned_cpu)) {
            scx_bpf_kick_cpu(tctx->owned_cpu, SCX_KICK_PREEMPT);
        } else if (fenced(cpu, tctx->dom)) {
            kick_idle_cpu(p, tctx->dom);
        }
    } else {
        scx_bpf_dsq_insert(p, PYLON_HOST_DSQ, SCX_SLICE_DFL, enq_flags);
        if (fenced(cpu, CLAIM_HOST)) {
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
 * Dispatches this pCPU's owner if it is queued, and moves any other task
 * left on the owner queue (its ownership changed) to the queue it now
 * belongs on. Returns true if the owner was dispatched.
 */
static bool dispatch_owner(s32 cpu)
{
    struct task_struct *p;
    __u32 tid = owner_tid(cpu);
    bool dispatched = false;

    bpf_for_each(scx_dsq, p, (u64)cpu, 0) {
        if (!dispatched && tid != 0 && (__u32)p->pid == tid) {
            if (claim_cpu(cpu, task_claim(p)) &&
                scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0)) {
                dispatched = true;
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
        if (claim_cpu(cpu, tctx->dom ? tctx->dom : CLAIM_HOST) &&
            scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0)) {
            return true;
        }
    }
    return false;
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
    if (cpu >= 0 && cpu < MAX_PCPUS && dispatch_owner(cpu)) {
        return;
    }
    if (dispatch_vcpu(cpu)) {
        return;
    }
    if (claim_cpu(cpu, CLAIM_HOST) && scx_bpf_dsq_move_to_local(PYLON_HOST_DSQ)) {
        return;
    }
    /* Nothing to run: the CPU goes idle. */
    set_claim(cpu, CLAIM_NONE);
}

/*
 * scx_ops.running(p)
 *
 * Record what this CPU runs, including tasks that the kernel queued locally
 * without asking the scheduler. The claim isn't cleared when a task stops:
 * dispatch has already claimed for the next task by then, and the CPU going
 * idle clears it.
 */
SEC("struct_ops/pylon_running")
void BPF_PROG(pylon_running, struct task_struct *p)
{
    set_claim(scx_bpf_task_cpu(p), task_claim(p));
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
