// Spec 11 M1: cost of changing a thread's CPU affinity, by the thread's state.
//   blocked: the thread sleeps (futex), as a vCPU halted in KVM sleeps in kvm_vcpu_block
//   running-keep: the thread spins on a CPU that stays in its new mask
//   running-move: the thread spins on a CPU that leaves its new mask (the kernel migrates it)
// For running-move it also reports how long until the thread runs on an allowed CPU.
// Usage: m1 <cpuA> <cpuB> <iterations>
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int tid_g, cpu_now = -1, stop;
static int futex_word;

static long ns(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000000L + t.tv_nsec;
}

static void *sleeper(void *a)
{
    (void)a;
    tid_g = syscall(SYS_gettid);
    while (!stop)
        syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, NULL, NULL, 0);
    return NULL;
}

static void *spinner(void *a)
{
    (void)a;
    tid_g = syscall(SYS_gettid);
    while (!stop)
        cpu_now = sched_getcpu();
    return NULL;
}

static int cmp(const void *a, const void *b)
{
    long x = *(const long *)a, y = *(const long *)b;
    return (x > y) - (x < y);
}

static void report(const char *what, long *v, int n)
{
    qsort(v, n, sizeof(long), cmp);
    printf("%-28s p50 %6.1f  p90 %6.1f  p99 %6.1f  max %7.1f us\n", what, v[n / 2] / 1e3,
           v[n * 9 / 10] / 1e3, v[n * 99 / 100] / 1e3, v[n - 1] / 1e3);
}

static void set(int tid, int c1, int c2)
{
    cpu_set_t s;
    CPU_ZERO(&s);
    CPU_SET(c1, &s);
    if (c2 >= 0)
        CPU_SET(c2, &s);
    sched_setaffinity(tid, sizeof(s), &s);
}

int main(int argc, char **argv)
{
    int a = atoi(argv[1]), b = atoi(argv[2]), n = atoi(argv[3]);
    long *t = calloc(n, sizeof(long)), *m = calloc(n, sizeof(long));
    pthread_t th;

    /* Blocked. */
    tid_g = 0;
    pthread_create(&th, NULL, sleeper, NULL);
    while (!tid_g)
        ;
    usleep(10000);
    for (int i = 0; i < n; i++) {
        long t0 = ns();
        set(tid_g, i & 1 ? a : b, -1);
        t[i] = ns() - t0;
    }
    report("blocked", t, n);
    stop = 1;
    syscall(SYS_futex, &futex_word, FUTEX_WAKE, 1, NULL, NULL, 0);
    pthread_join(th, NULL);
    stop = 0;

    /* Running, its CPU stays allowed. */
    tid_g = 0;
    pthread_create(&th, NULL, spinner, NULL);
    while (!tid_g)
        ;
    set(tid_g, a, -1);
    usleep(10000);
    for (int i = 0; i < n; i++) {
        long t0 = ns();
        set(tid_g, a, i & 1 ? b : -1);
        t[i] = ns() - t0;
    }
    report("running, CPU kept", t, n);

    /* Running, its CPU removed: the kernel migrates it. */
    for (int i = 0; i < n; i++) {
        int from = i & 1 ? b : a, to = i & 1 ? a : b;
        set(tid_g, from, -1);
        while (cpu_now != from)
            ;
        long t0 = ns();
        set(tid_g, to, -1);
        long t1 = ns();
        while (cpu_now != to)
            ;
        t[i] = t1 - t0;
        m[i] = ns() - t0;
    }
    report("running, CPU removed: call", t, n);
    report("  ... until it runs on new", m, n);
    stop = 1;
    pthread_join(th, NULL);
    return 0;
}
