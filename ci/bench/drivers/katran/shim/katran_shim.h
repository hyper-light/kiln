/*
 * ci/bench/drivers/katran/shim/katran_shim.h: the fewest shims that let Katran's own lookup code
 * (katran/lib/bpf/balancer.bpf.c at 042d08d, extracted by run.py into lookup.inc) compile as
 * userspace C. Everything here stands in for the kernel; nothing here is Katran's:
 *
 *   - the kernel's integer types (__u8 .. __u64, __be16, __be32);
 *   - three BPF maps, as the lookup path uses them: ch_rings (BPF_MAP_TYPE_ARRAY of real ids, VIP 0's
 *     RING_SIZE entries), reals (ARRAY of struct real_definition) and stats (PERCPU_ARRAY of struct
 *     lb_stats; one CPU here). bpf_map_lookup_elem() on them is a bounds check and an index, which is
 *     what the kernel's verifier inlines an array map lookup to;
 *   - bpf_ktime_get_ns(): the monotonic clock in nanoseconds;
 *   - bpf_map_update_elem(): never reached (the bench passes no LRU map; see katran.c).
 */
#ifndef KILN_LB_KATRAN_SHIM_H
#define KILN_LB_KATRAN_SHIM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#if defined(__APPLE__)
#include <mach/mach_time.h>
#else
#include <time.h>
#endif

typedef uint8_t __u8;
typedef uint16_t __u16;
typedef uint32_t __u32;
typedef uint64_t __u64;
typedef uint16_t __be16;
typedef uint32_t __be32;

#define BPF_ANY 0

#include "katran/lib/bpf/balancer_consts.h"
#include "katran/lib/bpf/balancer_structs.h"
#include "katran/lib/linux_includes/jhash.h"

struct lb_katran_map {
	int id;
};
static struct lb_katran_map ch_rings = {1}, reals = {2}, stats = {3};

static __u32 *lb_katran_ring;
static __u32 lb_katran_ring_len;
static struct real_definition *lb_katran_reals;
static __u32 lb_katran_nreals;
static struct lb_stats lb_katran_stats[STATS_MAP_SIZE];

__attribute__((__always_inline__)) static inline void *bpf_map_lookup_elem(void *map, const void *key)
{
	__u32 k = *(const __u32 *)key;
	if (map == &ch_rings)
		return k < lb_katran_ring_len ? (void *)&lb_katran_ring[k] : NULL;
	if (map == &reals)
		return k < lb_katran_nreals ? (void *)&lb_katran_reals[k] : NULL;
	if (map == &stats)
		return k < STATS_MAP_SIZE ? (void *)&lb_katran_stats[k] : NULL;
	return NULL;
}

__attribute__((__always_inline__)) static inline long bpf_map_update_elem(void *map, const void *key, const void *value, __u64 flags)
{
	(void)map;
	(void)key;
	(void)value;
	(void)flags;
	return -1;
}

static double lb_katran_ns_per_tick;

__attribute__((__always_inline__)) static inline __u64 bpf_ktime_get_ns(void)
{
#if defined(__APPLE__)
	return (__u64)((double)mach_absolute_time() * lb_katran_ns_per_tick);
#else
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (__u64)ts.tv_sec * 1000000000ull + (__u64)ts.tv_nsec;
#endif
}

#endif
