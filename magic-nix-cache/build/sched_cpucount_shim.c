#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <sched.h>
#include <stddef.h>

// glibc exports this symbol and `liblzma` uses it when available.
// musl doesn't provide it, so we implement a small, compatible version.
int __sched_cpucount(size_t setsize, const cpu_set_t *setp) {
    const unsigned char *p = (const unsigned char *)setp;
    int count = 0;

    for (size_t i = 0; i < setsize; ++i) {
        count += __builtin_popcount((unsigned)p[i]);
    }

    return count;
}
