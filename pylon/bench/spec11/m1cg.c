// Spec 11 M1: cost of one cgroup-v2 cpuset.cpus write for a cgroup holding this process and
// N other threads, sleeping or spinning. Run as root.
// Usage: m1cg <cgroup dir> <n threads> <spin 0|1> <cpus A> <cpus B> <iterations>
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static atomic_int stop;
static int spin;

static long ns(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000000L + t.tv_nsec;
}

static void *worker(void *a)
{
    (void)a;
    while (!stop) {
        if (!spin)
            usleep(100000);
    }
    return NULL;
}

static int cmp(const void *a, const void *b)
{
    long x = *(const long *)a, y = *(const long *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv)
{
    const char *dir = argv[1];
    int nt = atoi(argv[2]), n = atoi(argv[6]);
    spin = atoi(argv[3]);
    char path[512], pid[32];
    long *t = calloc(n, sizeof(long));

    snprintf(path, sizeof(path), "%s/cgroup.procs", dir);
    int fd = open(path, O_WRONLY);
    snprintf(pid, sizeof(pid), "%d", getpid());
    if (fd < 0 || write(fd, pid, strlen(pid)) < 0) {
        perror(path);
        return 1;
    }
    close(fd);
    pthread_t th[64];
    for (int i = 0; i < nt; i++)
        pthread_create(&th[i], NULL, worker, NULL);
    usleep(50000);

    snprintf(path, sizeof(path), "%s/cpuset.cpus", dir);
    fd = open(path, O_WRONLY);
    for (int i = 0; i < n; i++) {
        const char *c = i & 1 ? argv[4] : argv[5];
        long t0 = ns();
        if (pwrite(fd, c, strlen(c), 0) < 0) {
            perror("cpuset.cpus");
            return 1;
        }
        t[i] = ns() - t0;
        usleep(2000);
    }
    qsort(t, n, sizeof(long), cmp);
    printf("cpuset write, %2d %s threads  p50 %6.1f  p90 %6.1f  p99 %6.1f  max %7.1f us\n", nt,
           spin ? "spinning" : "sleeping", t[n / 2] / 1e3, t[n * 9 / 10] / 1e3,
           t[n * 99 / 100] / 1e3, t[n - 1] / 1e3);
    stop = 1;
    for (int i = 0; i < nt; i++)
        pthread_join(th[i], NULL);
    return 0;
}
