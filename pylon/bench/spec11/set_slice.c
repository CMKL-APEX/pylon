// SPDX-License-Identifier: Apache-2.0
// Sets the fair-scheduler slice (sched_attr.sched_runtime, Linux 6.12+) of the given threads:
// set_slice <ns> <tid>... (spec 11 prototype, M3).
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

struct sched_attr {
    uint32_t size, sched_policy;
    uint64_t sched_flags;
    int32_t sched_nice;
    uint32_t sched_priority;
    uint64_t sched_runtime, sched_deadline, sched_period;
    uint32_t sched_util_min, sched_util_max;
};

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s <slice ns> <tid>...\n", argv[0]);
        return 2;
    }
    int rc = 0;
    for (int i = 2; i < argc; i++) {
        struct sched_attr attr = { .size = sizeof(attr), .sched_policy = 0,
                                   .sched_runtime = strtoull(argv[1], NULL, 0) };
        if (syscall(SYS_sched_setattr, atoi(argv[i]), &attr, 0)) {
            perror(argv[i]);
            rc = 1;
        }
    }
    return rc;
}
