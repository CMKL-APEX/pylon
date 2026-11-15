// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//
// The guest side of the overcommit benchmark (see README.md): one load,
// measured from the end of the warm-up to the end of the run. Every
// --report seconds it prints the work done so far; at the end, the work
// done in the window, or the round-trip percentiles for pingpong.
//
//   count     threads count loop iterations, sharing nothing. The control:
//             Pylon has nothing to win here.
//   spinlock  threads take one spinlock in turn, with a short critical
//             section. A preempted lock holder stalls every waiter.
//   barrier   threads do a short piece of work, then wait for each other
//             at a spinning barrier. A preempted thread stalls the round.
//   pipes     pairs of threads pass a byte back and forth over pipes, so
//             every step wakes a thread, often on another vCPU.
//   pingpong  two threads pass a byte back and forth every millisecond;
//             the round-trip time is the result.
//
// Build it static so it runs in any guest:
//   cc -O2 -static -pthread -o pylon-overcommit-load workload.c

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define MAX_THREADS 64
#define CACHELINE 64

struct counter {
    _Alignas(CACHELINE) _Atomic uint64_t n;
};

static struct counter counters[MAX_THREADS];
static atomic_bool stop;
static atomic_bool measuring;
static int threads = 4;

static uint64_t now_ns(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static inline void cpu_relax(void)
{
#if defined(__x86_64__)
    __builtin_ia32_pause();
#elif defined(__aarch64__)
    __asm__ volatile("yield");
#endif
}

// About a nanosecond per iteration on a 3 GHz core: one dependent multiply-add.
static inline uint64_t spin_work(uint64_t x, int iterations)
{
    for (int i = 0; i < iterations; i++)
        x = x * 6364136223846793005ull + 1442695040888963407ull;
    return x;
}

static void add(int id, uint64_t n)
{
    atomic_fetch_add_explicit(&counters[id].n, n, memory_order_relaxed);
}

static uint64_t total(void)
{
    uint64_t sum = 0;

    for (int i = 0; i < MAX_THREADS; i++)
        sum += atomic_load_explicit(&counters[i].n, memory_order_relaxed);
    return sum;
}

static void *count_thread(void *arg)
{
    int id = (int)(intptr_t)arg;
    volatile uint64_t sink = id;

    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        sink = spin_work(sink, 1024);
        add(id, 1024);
    }
    return NULL;
}

static _Alignas(CACHELINE) atomic_int lock;
static _Alignas(CACHELINE) uint64_t shared_data[8];

static void *spinlock_thread(void *arg)
{
    int id = (int)(intptr_t)arg;
    uint64_t x = id;

    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        for (;;) {
            if (!atomic_exchange_explicit(&lock, 1, memory_order_acquire))
                break;
            while (atomic_load_explicit(&lock, memory_order_relaxed))
                cpu_relax();
        }
        // Critical section: a few hundred nanoseconds on shared data.
        for (int i = 0; i < 8; i++)
            shared_data[i] = spin_work(shared_data[i] + 1, 10);
        atomic_store_explicit(&lock, 0, memory_order_release);
        add(id, 1);
        x = spin_work(x, 1000);
    }
    return (void *)(uintptr_t)x;
}

static _Alignas(CACHELINE) atomic_int barrier_arrived;
static _Alignas(CACHELINE) atomic_int barrier_sense;

static void *barrier_thread(void *arg)
{
    int id = (int)(intptr_t)arg;
    int sense = 0;
    uint64_t x = id;

    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        x = spin_work(x, 20000);
        sense = !sense;
        if (atomic_fetch_add(&barrier_arrived, 1) == threads - 1) {
            atomic_store(&barrier_arrived, 0);
            atomic_store_explicit(&barrier_sense, sense, memory_order_release);
        } else {
            while (atomic_load_explicit(&barrier_sense, memory_order_acquire) != sense) {
                // The other threads have stopped: don't wait for a round that won't end.
                if (atomic_load_explicit(&stop, memory_order_relaxed))
                    return NULL;
                cpu_relax();
            }
        }
        if (id == 0)
            add(0, 1);
    }
    return (void *)(uintptr_t)x;
}

struct pair {
    int to_b[2];
    int to_a[2];
    int id;
};

static void *pipe_echo(void *arg)
{
    struct pair *p = arg;
    char c;

    while (read(p->to_b[0], &c, 1) == 1)
        if (write(p->to_a[1], &c, 1) != 1)
            break;
    return NULL;
}

#define MAX_SAMPLES (1u << 20)
static uint32_t samples[MAX_SAMPLES];
static unsigned int nsamples;

static void *pipe_send(void *arg)
{
    struct pair *p = arg;
    int pingpong = p->id < 0;
    char c = 0;

    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        uint64_t start = now_ns();

        if (write(p->to_b[1], &c, 1) != 1 || read(p->to_a[0], &c, 1) != 1)
            break;
        if (!pingpong) {
            add(p->id, 1);
            continue;
        }
        if (atomic_load_explicit(&measuring, memory_order_relaxed) && nsamples < MAX_SAMPLES)
            samples[nsamples++] = (uint32_t)((now_ns() - start) / 1000);
        struct timespec gap = { 0, 1000000 };
        nanosleep(&gap, NULL);
    }
    close(p->to_b[1]);
    return NULL;
}

static int cmp_u32(const void *a, const void *b)
{
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;

    return (x > y) - (x < y);
}

static void start_pair(struct pair *p, int id, pthread_t *a, pthread_t *b)
{
    if (pipe(p->to_b) || pipe(p->to_a)) {
        perror("pipe");
        exit(1);
    }
    p->id = id;
    pthread_create(b, NULL, pipe_echo, p);
    pthread_create(a, NULL, pipe_send, p);
}

static void usage(const char *prog)
{
    fprintf(stderr,
            "Usage: %s count|spinlock|barrier|pipes|pingpong [--threads N] [--seconds S]\n"
            "       [--warmup S] [--report S]\n"
            "--threads is the number of pairs for pipes; pingpong uses one pair.\n",
            prog);
    exit(2);
}

int main(int argc, char **argv)
{
    int seconds = 150, warmup = 40, report = 10;
    void *(*fn)(void *) = NULL;
    const char *mode;

    if (argc < 2)
        usage(argv[0]);
    mode = argv[1];
    for (int i = 2; i + 1 < argc; i += 2) {
        if (!strcmp(argv[i], "--threads"))
            threads = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "--seconds"))
            seconds = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "--warmup"))
            warmup = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "--report"))
            report = atoi(argv[i + 1]);
        else
            usage(argv[0]);
    }
    if (threads < 1 || threads > MAX_THREADS / 2 || warmup >= seconds || report < 1)
        usage(argv[0]);

    if (!strcmp(mode, "count"))
        fn = count_thread;
    else if (!strcmp(mode, "spinlock"))
        fn = spinlock_thread;
    else if (!strcmp(mode, "barrier"))
        fn = barrier_thread;
    else if (strcmp(mode, "pipes") && strcmp(mode, "pingpong"))
        usage(argv[0]);

    pthread_t tids[2 * MAX_THREADS];
    struct pair pairs[MAX_THREADS];
    int ntids = 0;

    if (fn) {
        for (int i = 0; i < threads; i++)
            pthread_create(&tids[ntids++], NULL, fn, (void *)(intptr_t)i);
    } else {
        int npairs = strcmp(mode, "pingpong") ? threads : 1;

        for (int i = 0; i < npairs; i++) {
            start_pair(&pairs[i], npairs == 1 && !strcmp(mode, "pingpong") ? -1 : i,
                       &tids[ntids], &tids[ntids + 1]);
            ntids += 2;
        }
    }

    uint64_t start = now_ns(), at_warmup = 0, warmup_ns = 0;

    for (int t = report; t <= seconds; t += report) {
        uint64_t deadline = start + (uint64_t)t * 1000000000ull;

        for (uint64_t now = now_ns(); now < deadline; now = now_ns()) {
            uint64_t left = deadline - now;
            struct timespec ts = { left / 1000000000ull, left % 1000000000ull };

            nanosleep(&ts, NULL);
        }
        if (t >= warmup && !warmup_ns) {
            at_warmup = total();
            warmup_ns = now_ns();
            atomic_store(&measuring, true);
        }
        printf("%s t=%d total=%llu\n", mode, t, (unsigned long long)total());
        fflush(stdout);
    }

    uint64_t end_total = total(), end_ns = now_ns();

    atomic_store(&stop, true);
    if (!strcmp(mode, "pingpong")) {
        unsigned int n = nsamples;

        qsort(samples, n, sizeof(samples[0]), cmp_u32);
        if (n == 0) {
            printf("%s result samples=0\n", mode);
        } else {
            printf("%s result samples=%u p50_us=%u p90_us=%u p99_us=%u p999_us=%u max_us=%u\n",
                   mode, n, samples[n / 2], samples[(uint64_t)n * 90 / 100],
                   samples[(uint64_t)n * 99 / 100], samples[(uint64_t)n * 999 / 1000],
                   samples[n - 1]);
        }
    } else {
        double secs = (double)(end_ns - warmup_ns) / 1e9;

        printf("%s result threads=%d window_s=%.1f ops=%llu ops_per_s=%.0f\n", mode, threads,
               secs, (unsigned long long)(end_total - at_warmup),
               (double)(end_total - at_warmup) / secs);
    }
    fflush(stdout);
    // The pipe threads may be blocked in read; exiting the process ends them.
    _exit(0);
}
