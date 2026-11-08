/*
 * ci/bench/drivers/katran/katran.c: Katran's Maglev lookup (main 042d08d) under common.h's workload.
 *
 * The pick is Katran's own code, compiled as userspace C: run.py copies these functions verbatim
 * from katran/lib/bpf/balancer.bpf.c into lookup.inc (their line numbers are recorded at its top):
 * get_packet_hash() (jhash_2words of the source address and the ports, Katran's jhash.h),
 * is_under_flood(), increment_ch_drop_no_real(), increment_ch_drop_real_0() and get_packet_dst():
 * the new-flow path, ring = jhash % RING_SIZE, ch_rings[RING_SIZE * vip_num + ring] is the real's
 * id, reals[id] its definition. shim/katran_shim.h stands in for the kernel (types, three array maps,
 * the clock). The ring is VIP 0's, built by Katran's MaglevHash (ring.cc), RING_SIZE 65537, Katran's
 * default; N above Katran's default MAX_REALS (4096) is a build with a larger MAX_REALS, as
 * balancer_consts.h allows (#ifndef MAX_REALS).
 *
 * What isn't here: packet parsing (the flow is set from the harness's key: the client's IPv4
 * address and source port, destination port 80, TCP), the connection table (get_packet_dst gets no
 * LRU map: its insert is the kernel's BPF LRU map, not Katran's code; every request is a new flow)
 * and encapsulation. Katran keeps no per-backend state and has no release: the release is a no-op,
 * so ns_release is the harness's own and the pick alone is ns_step - ns_release. The XDP program
 * itself runs only under BPF_PROG_TEST_RUN, as root on Linux (README.md says what that run takes).
 */
#include "katran_shim.h"

#include "lookup.inc"

#include "../../common.h"

#ifndef LB_KATRAN_COMMIT
#define LB_KATRAN_COMMIT "042d08df6677dbde86ed7b127218a8e80445ce41"
#endif

int lb_katran_ring_build(uint32_t n, uint32_t ring_size, uint32_t *out);

static struct vip_meta lb_katran_vip = {0, 0};

static int32_t lb_katran_pick(void *ctx, uint32_t slot, uint64_t key)
{
	struct packet_description pckt;
	struct real_definition *dst = NULL;
	(void)ctx;
	(void)slot;
	memset(&pckt, 0, sizeof(pckt));
	pckt.flow.src = (__be32)(key >> 32);
	pckt.flow.dst = htonl((192u << 24) | (0u << 16) | (2u << 8) | 1u);
	pckt.flow.port16[0] = (__u16)key;
	pckt.flow.port16[1] = htons(80);
	pckt.flow.proto = IPPROTO_TCP;
	if (!get_packet_dst(&dst, &pckt, &lb_katran_vip, false, NULL))
		return -1;
	return (int32_t)pckt.real_index - 1;
}

__attribute__((noinline)) static void lb_katran_release(void *ctx, uint32_t slot)
{
	(void)ctx;
	(void)slot;
	__asm__ volatile("" ::: "memory");
}

static int lb_katran_setup(const char *algo, uint32_t n, uint32_t slots, lb_impl *out)
{
	(void)slots;
	if (strcmp(algo, "maglev")) {
		fprintf(stderr, "katran: the only algorithm is maglev, not %s\n", algo);
		return -1;
	}
	lb_katran_ns_per_tick = lb_ns_per_tick();
	lb_katran_ring_len = RING_SIZE;
	lb_katran_ring = calloc(RING_SIZE, sizeof(*lb_katran_ring));
	lb_katran_nreals = n + 1;
	lb_katran_reals = calloc(n + 1, sizeof(*lb_katran_reals));
	if (!lb_katran_ring || !lb_katran_reals)
		return -1;
	if (lb_katran_ring_build(n, RING_SIZE, lb_katran_ring)) {
		fprintf(stderr, "katran: ring build failed\n");
		return -1;
	}
	for (uint32_t i = 1; i <= n; i++)
		lb_katran_reals[i].dst = htonl((10u << 24) | (i - 1));
	out->impl = "katran";
	out->algo = "maglev";
	out->commit = LB_KATRAN_COMMIT;
	out->ctx = NULL;
	out->release = lb_katran_release;
	out->pick = lb_katran_pick;
	return 0;
}

int main(int argc, char **argv)
{
	return lb_main(argc, argv, lb_katran_setup);
}
