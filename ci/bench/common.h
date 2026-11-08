/*
 * ci/bench/common.h: the workload every C driver of the load-balancer bench
 * shares (bench-spec.md, 2026-11-07). A driver supplies one release and one
 * pick, each calling its competitor's real path; this header owns everything
 * else (the generator, the steps, the warm-up, the timing, the quality counts
 * and the JSON line), so implementations differ only in what they are asked to
 * compare.
 *
 * A run, in order (README.md says why each phase is what it is):
 *   null   the same phases with a no-op release and pick, in this process:
 *          ns_null and ns_null_release, subtracted from this run's numbers;
 *   fill   F' = max(1, floor(rho * N * C)) untimed picks, one per slot;
 *   warm   W untimed steps (W measured once per N by run.py, "trend" mode);
 *   steps  M timed blocks of B steps: ns_step is the median block over B;
 *   qual   M * B untimed steps: oversubscription, refusals, and the
 *          imbalance (CV of in-flight counts) sampled every N steps;
 *   rel    T = M_R * B untimed picks top the state up, then M_R timed blocks
 *          of B release-only steps bring it back to F': ns_release.
 *
 * A step: one in-flight request drawn uniformly by SplitMix64 is released
 * through the implementation's release; then one pick, for a new request
 * whose key (the client, for the hashing algorithms) is the generator's next
 * draw, stored in the slot the release freed. Every implementation sees the
 * same sequence: seed 0, two draws per step (index, then key), one per fill
 * or top-up pick (key), one per release-only step (index).
 */
#ifndef KILN_LB_BENCH_COMMON_H
#define KILN_LB_BENCH_COMMON_H

#include <errno.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__APPLE__)
#include <mach/mach_time.h>
#include <pthread.h>
#include <pthread/qos.h>
#else
#include <time.h>
#endif

#if defined(__cplusplus)
/* This header is C, shared with the C drivers; Envoy's driver includes it under its C++ warnings. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wold-style-cast"
#endif

/* Each backend takes C requests at once (spec: kiln's 8 CPUs, one per run). */
#define LB_C 8u
/* The generator's fixed seed: the sequence every implementation sees. */
#define LB_SEED 0u
/* A block must last at least this many counter ticks (spec: Timing). */
#define LB_MIN_TICKS 1000u
/*
 * At least three blocks per measurement: the smallest count whose median
 * survives one disturbed block (an interrupt, a preemption).
 */
#define LB_MIN_BLOCKS 3u

typedef void (*lb_release_fn)(void *ctx, uint32_t slot);
typedef int32_t (*lb_pick_fn)(void *ctx, uint32_t slot, uint64_t key);
/* After every request is released: 0 if the implementation holds none (its counts are back to zero). */
typedef int (*lb_check_fn)(void *ctx, char *why, size_t len);
/* The backend (0..N-1) the request in <slot> holds, for an implementation whose pick can't say it cheaply. */
typedef int32_t (*lb_index_fn)(void *ctx, uint32_t slot);

typedef struct {
	const char *impl;   /* "nginx", "haproxy", "envoy", "null" */
	const char *algo;   /* the competitor's own name for the algorithm */
	const char *commit; /* the competitor's source commit */
	void *ctx;
	lb_release_fn release; /* release the request held in <slot> */
	lb_pick_fn pick;       /* pick for a new request in <slot>: -1 if refused, else its backend 0..N-1 (or, with index, any value >= 0) */
	lb_check_fn check;     /* optional */
	lb_index_fn index;     /* optional: called only in the untimed quality steps */
} lb_impl;

/* Sets up <algo> for N backends and at most <slots> requests in flight. 0 on success. */
typedef int (*lb_setup_fn)(const char *algo, uint32_t n, uint32_t slots, lb_impl *out);

typedef struct {
	uint32_t n;
	double rho;
	uint32_t f;       /* requests in flight */
	uint64_t b;       /* steps per block */
	uint64_t w;       /* warm-up steps */
	uint32_t run;
	uint32_t m;       /* timed step blocks */
	uint32_t m_r;     /* timed release-only blocks */
	uint64_t horizon; /* trend mode: steps after the fill */
} lb_params;

typedef struct {
	uint64_t rng;
	uint32_t f;
	int32_t *sb; /* backend of the request in each slot, -1 if none */
} lb_state;

/* ---- generator ------------------------------------------------------------ */

static inline uint64_t lb_next(uint64_t *s)
{
	uint64_t z = (*s += 0x9e3779b97f4a7c15ULL);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	return z ^ (z >> 31);
}

/* Uniform in [0, n): the high word of x * n (Lemire's multiply-shift). */
static inline uint32_t lb_uniform(uint64_t x, uint32_t n)
{
	return (uint32_t)(((unsigned __int128)x * n) >> 64);
}

/* ---- clock ---------------------------------------------------------------- */

static inline uint64_t lb_ticks(void)
{
#if defined(__APPLE__)
	return mach_absolute_time();
#else
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC_RAW, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
#endif
}

static double lb_ns_per_tick(void)
{
#if defined(__APPLE__)
	mach_timebase_info_data_t tb;
	if (mach_timebase_info(&tb) != KERN_SUCCESS || tb.denom == 0)
		return 0.0;
	return (double)tb.numer / (double)tb.denom;
#else
	return 1.0;
#endif
}

/* ---- the null implementation: the harness's bookkeeping alone ------------- */

__attribute__((noinline)) static void lb_null_release(void *ctx, uint32_t slot)
{
	(void)ctx;
	(void)slot;
	__asm__ volatile("" ::: "memory");
}

__attribute__((noinline)) static int32_t lb_null_pick(void *ctx, uint32_t slot, uint64_t key)
{
	(void)ctx;
	(void)slot;
	(void)key;
	__asm__ volatile("" ::: "memory");
	return 0;
}

static const lb_impl lb_null_impl = {"null", "null", "-", NULL, lb_null_release, lb_null_pick, NULL, NULL};

/* ---- phases ------------------------------------------------------------- */

/*
 * The function pointers are read through volatile so no compiler can inline
 * the null's no-ops into its loop: every implementation, the null included,
 * pays the same indirect call.
 */
#define LB_LOAD_FNS(im)                                                         \
	lb_release_fn release = *(lb_release_fn volatile const *)&(im)->release; \
	lb_pick_fn pick = *(lb_pick_fn volatile const *)&(im)->pick;             \
	void *ctx = (im)->ctx

__attribute__((noinline)) static void lb_fill(lb_state *st, const lb_impl *im, uint32_t from, uint32_t to)
{
	LB_LOAD_FNS(im);
	(void)release;
	uint64_t rng = st->rng;
	int32_t *sb = st->sb;
	for (uint32_t s = from; s < to; s++)
		sb[s] = pick(ctx, s, lb_next(&rng));
	st->rng = rng;
}

/* The step, untimed or timed alike: release one drawn uniformly, then pick into its slot. */
__attribute__((noinline)) static void lb_steps(lb_state *st, const lb_impl *im, uint64_t steps)
{
	LB_LOAD_FNS(im);
	uint64_t rng = st->rng;
	int32_t *sb = st->sb;
	uint32_t f = st->f;
	for (uint64_t i = 0; i < steps; i++) {
		uint32_t s = lb_uniform(lb_next(&rng), f);
		uint64_t key = lb_next(&rng);
		if (sb[s] >= 0)
			release(ctx, s);
		sb[s] = pick(ctx, s, key);
	}
	st->rng = rng;
}

typedef struct {
	uint64_t picks, oversub, refusals, bad, samples;
	double cv_sum;
	double *series; /* trend mode: every sample, or NULL */
	uint64_t series_cap;
} lb_quality;

static double lb_cv(const uint32_t *counts, uint32_t n)
{
	double sum = 0.0, sq = 0.0;
	for (uint32_t i = 0; i < n; i++) {
		double c = (double)counts[i];
		sum += c;
		sq += c * c;
	}
	if (sum <= 0.0)
		return 0.0;
	double mean = sum / (double)n;
	double var = sq / (double)n - mean * mean;
	return var > 0.0 ? sqrt(var) / mean : 0.0;
}

/* Untimed steps that also count: the step above plus the quality bookkeeping. */
__attribute__((noinline)) static void lb_quality_steps(lb_state *st, const lb_impl *im, uint32_t n, uint64_t steps, uint32_t *counts, lb_quality *q)
{
	LB_LOAD_FNS(im);
	uint64_t rng = st->rng;
	int32_t *sb = st->sb;
	uint32_t f = st->f;
	lb_index_fn index = im->index;
	memset(counts, 0, sizeof(*counts) * n);
	for (uint32_t s = 0; s < f; s++) {
		if (sb[s] >= 0 && index)
			sb[s] = index(ctx, s);
		if (sb[s] >= 0 && (uint32_t)sb[s] < n)
			counts[sb[s]]++;
	}
	for (uint64_t i = 0; i < steps; i++) {
		uint32_t s = lb_uniform(lb_next(&rng), f);
		uint64_t key = lb_next(&rng);
		if (sb[s] >= 0) {
			release(ctx, s);
			counts[sb[s]]--;
		}
		int32_t b = pick(ctx, s, key);
		if (b >= 0 && index)
			b = index(ctx, s);
		q->picks++;
		if (b < 0 || (uint32_t)b >= n) {
			if (b >= 0)
				q->bad++; /* a backend that doesn't exist: a driver bug, reported */
			q->refusals++;
			b = -1;
		} else {
			if (counts[b] >= LB_C)
				q->oversub++;
			counts[b]++;
		}
		sb[s] = b;
		if ((i + 1) % n == 0) {
			double cv = lb_cv(counts, n);
			q->cv_sum += cv;
			if (q->series && q->samples < q->series_cap)
				q->series[q->samples] = cv;
			q->samples++;
		}
	}
	st->rng = rng;
}

/* Release-only steps on the topped-up state: draw one of the <*live> in flight, release it. */
__attribute__((noinline)) static void lb_release_steps(lb_state *st, const lb_impl *im, uint32_t *live, uint32_t *nlive, uint64_t steps)
{
	LB_LOAD_FNS(im);
	(void)pick;
	uint64_t rng = st->rng;
	int32_t *sb = st->sb;
	uint32_t cnt = *nlive;
	for (uint64_t i = 0; i < steps; i++) {
		uint32_t j = lb_uniform(lb_next(&rng), cnt);
		uint32_t s = live[j];
		live[j] = live[--cnt];
		if (sb[s] >= 0)
			release(ctx, s);
	}
	*nlive = cnt;
	st->rng = rng;
}

static int lb_cmp_u64(const void *a, const void *b)
{
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return (x > y) - (x < y);
}

static uint64_t lb_median_u64(const uint64_t *v, uint32_t k)
{
	uint64_t tmp[64];
	uint32_t m = k < 64 ? k : 64;
	if (m == 0)
		return 0;
	memcpy(tmp, v, sizeof(*v) * m);
	qsort(tmp, m, sizeof(*tmp), lb_cmp_u64);
	return (m % 2) ? tmp[m / 2] : (tmp[m / 2 - 1] + tmp[m / 2]) / 2;
}

/* Everything one implementation's measurement yields. */
typedef struct {
	uint64_t step_ticks[64], rel_ticks[64];
	double ns_step, ns_release;
	uint64_t min_step_ticks, min_rel_ticks;
	lb_quality q;
	int check;
	char check_why[160];
} lb_result;

static int lb_measure(const lb_impl *im, const lb_params *p, lb_result *r)
{
	uint32_t slots = p->f + (uint32_t)(p->m_r * p->b);
	lb_state st = {LB_SEED, p->f, NULL};
	st.sb = (int32_t *)malloc(sizeof(*st.sb) * slots);
	uint32_t *counts = (uint32_t *)calloc(p->n, sizeof(*counts));
	uint32_t *live = (uint32_t *)malloc(sizeof(*live) * slots);
	if (!st.sb || !counts || !live) {
		free(st.sb);
		free(counts);
		free(live);
		return -1;
	}
	for (uint32_t s = 0; s < slots; s++)
		st.sb[s] = -1;
	memset(r, 0, sizeof(*r));

	lb_fill(&st, im, 0, p->f);
	lb_steps(&st, im, p->w);

	for (uint32_t k = 0; k < p->m && k < 64; k++) {
		uint64_t t0 = lb_ticks();
		lb_steps(&st, im, p->b);
		uint64_t t1 = lb_ticks();
		r->step_ticks[k] = t1 - t0;
	}

	lb_quality_steps(&st, im, p->n, (uint64_t)p->m * p->b, counts, &r->q);

	/* Top up so the release-only blocks leave the state at F' rather than drain it. */
	lb_fill(&st, im, p->f, slots);
	uint32_t nlive = 0;
	for (uint32_t s = 0; s < slots; s++)
		live[nlive++] = s;
	for (uint32_t k = 0; k < p->m_r && k < 64; k++) {
		uint64_t t0 = lb_ticks();
		lb_release_steps(&st, im, live, &nlive, p->b);
		uint64_t t1 = lb_ticks();
		r->rel_ticks[k] = t1 - t0;
	}
	/* Release what is left, so the implementation ends empty, and check that it does. */
	lb_release_steps(&st, im, live, &nlive, nlive);
	r->check = 0;
	if (im->check)
		r->check = im->check(im->ctx, r->check_why, sizeof(r->check_why));

	double npt = lb_ns_per_tick();
	r->ns_step = (double)lb_median_u64(r->step_ticks, p->m) * npt / (double)p->b;
	r->ns_release = (double)lb_median_u64(r->rel_ticks, p->m_r) * npt / (double)p->b;
	r->min_step_ticks = UINT64_MAX;
	for (uint32_t k = 0; k < p->m && k < 64; k++)
		if (r->step_ticks[k] < r->min_step_ticks)
			r->min_step_ticks = r->step_ticks[k];
	r->min_rel_ticks = UINT64_MAX;
	for (uint32_t k = 0; k < p->m_r && k < 64; k++)
		if (r->rel_ticks[k] < r->min_rel_ticks)
			r->min_rel_ticks = r->rel_ticks[k];
	free(st.sb);
	free(counts);
	free(live);
	return 0;
}

/* ---- the command line ----------------------------------------------------- */

static void lb_usage(const char *prog)
{
	fprintf(stderr,
	        "usage: %s ALGO --n N --rho R [--mode measure|calibrate|trend] [--b B] [--w W]\n"
	        "          [--run K] [--horizon H]\n"
	        "  measure   one run: one JSON line (bench-spec.md's fields and a few more)\n"
	        "  calibrate the null alone: the smallest power-of-two B whose block takes\n"
	        "            at least %u counter ticks\n"
	        "  trend     after the fill, H steps; the imbalance sampled every N steps\n",
	        prog, LB_MIN_TICKS);
}

static int lb_parse_u64(const char *s, uint64_t *out)
{
	char *end = NULL;
	errno = 0;
	unsigned long long v = strtoull(s, &end, 10);
	if (errno || !end || *end || end == s)
		return -1;
	*out = v;
	return 0;
}

static void lb_print_ticks(const char *name, const uint64_t *v, uint32_t k, double npt, uint64_t b)
{
	printf(",\"%s\":[", name);
	for (uint32_t i = 0; i < k && i < 64; i++)
		printf("%s%.4f", i ? "," : "", (double)v[i] * npt / (double)b);
	printf("]");
}

static uint32_t lb_inflight(uint32_t n, double rho)
{
	double f = floor(rho * (double)n * (double)LB_C);
	return f < 1.0 ? 1u : (uint32_t)f;
}

/*
 * The driver's main: parses the line, runs the null and then the
 * implementation through the same phases, prints one JSON line on stdout.
 */
static int lb_main(int argc, char **argv, lb_setup_fn setup)
{
	if (argc < 2) {
		lb_usage(argv[0]);
		return 2;
	}
	const char *algo = argv[1];
	const char *mode = "measure";
	lb_params p;
	memset(&p, 0, sizeof(p));
	p.rho = -1.0;
	for (int i = 2; i < argc; i += 2) {
		const char *a = argv[i];
		const char *v = i + 1 < argc ? argv[i + 1] : NULL;
		uint64_t x = 0;
		if (!v) {
			lb_usage(argv[0]);
			return 2;
		}
		if (!strcmp(a, "--n") && !lb_parse_u64(v, &x) && x > 0 && x <= 1000000)
			p.n = (uint32_t)x;
		else if (!strcmp(a, "--rho")) {
			char *end = NULL;
			p.rho = strtod(v, &end);
			if (!end || *end || p.rho < 0.0 || p.rho >= 1.0) {
				fprintf(stderr, "--rho must be in [0, 1)\n");
				return 2;
			}
		} else if (!strcmp(a, "--b") && !lb_parse_u64(v, &x) && x > 0)
			p.b = x;
		else if (!strcmp(a, "--w") && !lb_parse_u64(v, &x))
			p.w = x;
		else if (!strcmp(a, "--run") && !lb_parse_u64(v, &x))
			p.run = (uint32_t)x;
		else if (!strcmp(a, "--horizon") && !lb_parse_u64(v, &x))
			p.horizon = x;
		else if (!strcmp(a, "--mode"))
			mode = v;
		else {
			fprintf(stderr, "bad argument: %s %s\n", a, v);
			lb_usage(argv[0]);
			return 2;
		}
	}
	if (!p.n || p.rho < 0.0) {
		lb_usage(argv[0]);
		return 2;
	}
	p.f = lb_inflight(p.n, p.rho);

#if defined(__APPLE__)
	/* Spec: QoS user-interactive, so the scheduler keeps the run on a performance core. */
	if (pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) != 0)
		fprintf(stderr, "warning: could not set QoS user-interactive\n");
#endif
	double npt = lb_ns_per_tick();

	if (!strcmp(mode, "calibrate")) {
		/* B: the smallest power of two whose every block (of LB_MIN_BLOCKS) of null steps lasts LB_MIN_TICKS. */
		p.m = LB_MIN_BLOCKS;
		p.m_r = 0;
		for (uint64_t b = 1; b <= (1ull << 30); b <<= 1) {
			lb_result r;
			p.b = b;
			if (lb_measure(&lb_null_impl, &p, &r))
				return 1;
			if (r.min_step_ticks >= LB_MIN_TICKS) {
				printf("{\"n\":%u,\"rho\":%.2f,\"f\":%u,\"b\":%llu,\"min_ticks\":%llu,\"ns_null\":%.4f}\n", p.n, p.rho, p.f, (unsigned long long)b,
				       (unsigned long long)r.min_step_ticks, r.ns_step);
				return 0;
			}
		}
		return 1;
	}

	if (!p.b) {
		fprintf(stderr, "--b is required (run.py calibrates it)\n");
		return 2;
	}
	/*
	 * Timed steps: at least LB_MIN_BLOCKS blocks, and at least F' steps (by
	 * Little's law a request's mean stay is F' steps, so the window sees the
	 * in-flight population turn over once).
	 */
	uint64_t need = (p.f + p.b - 1) / p.b;
	p.m = need > LB_MIN_BLOCKS ? (uint32_t)need : LB_MIN_BLOCKS;
	if (p.m > 64)
		p.m = 64;
	p.m_r = LB_MIN_BLOCKS;
	uint32_t slots = p.f + (uint32_t)(p.m_r * p.b);

	lb_impl im;
	memset(&im, 0, sizeof(im));
	if (!strcmp(algo, "null"))
		im = lb_null_impl;
	else if (setup(algo, p.n, !strcmp(mode, "trend") ? p.f : slots, &im) != 0) {
		fprintf(stderr, "setup failed for %s\n", algo);
		return 1;
	}

	if (!strcmp(mode, "trend")) {
		if (!p.horizon) {
			fprintf(stderr, "--horizon is required in trend mode\n");
			return 2;
		}
		lb_state st = {LB_SEED, p.f, (int32_t *)malloc(sizeof(int32_t) * p.f)};
		uint32_t *counts = (uint32_t *)calloc(p.n, sizeof(*counts));
		lb_quality q;
		memset(&q, 0, sizeof(q));
		q.series_cap = p.horizon / p.n;
		q.series = (double *)calloc(q.series_cap ? q.series_cap : 1, sizeof(double));
		if (!st.sb || !counts || !q.series)
			return 1;
		lb_fill(&st, &im, 0, p.f);
		lb_quality_steps(&st, &im, p.n, p.horizon, counts, &q);
		printf("{\"impl\":\"%s\",\"algo\":\"%s\",\"n\":%u,\"rho\":%.2f,\"f\":%u,\"every\":%u,\"oversub\":%llu,\"refusals\":%llu,\"picks\":%llu,\"cv_series\":[", im.impl, im.algo,
		       p.n, p.rho, p.f, p.n, (unsigned long long)q.oversub, (unsigned long long)q.refusals, (unsigned long long)q.picks);
		for (uint64_t i = 0; i < q.samples && i < q.series_cap; i++)
			printf("%s%.6f", i ? "," : "", q.series[i]);
		printf("]}\n");
		return 0;
	}

	if (strcmp(mode, "measure")) {
		lb_usage(argv[0]);
		return 2;
	}

	lb_result nr, r;
	if (lb_measure(&lb_null_impl, &p, &nr) || lb_measure(&im, &p, &r)) {
		fprintf(stderr, "out of memory\n");
		return 1;
	}
	if (r.q.bad || r.check) {
		fprintf(stderr, "%s %s: %llu picks of a backend that doesn't exist; check: %s\n", im.impl, im.algo, (unsigned long long)r.q.bad,
		        r.check ? r.check_why : "ok");
		return 3;
	}
	double cv = r.q.samples ? r.q.cv_sum / (double)r.q.samples : 0.0;
	printf("{\"impl\":\"%s\",\"algo\":\"%s\",\"n\":%u,\"rho\":%.2f,\"d\":1,\"run\":%u,\"b\":%llu,"
	       "\"ns_step\":%.4f,\"ns_release\":%.4f,\"ns_null\":%.4f,\"ns_null_release\":%.4f,"
	       "\"oversub\":%llu,\"refusals\":%llu,\"picks\":%llu,\"cv\":%.6f,\"cv_samples\":%llu,"
	       "\"f\":%u,\"w\":%llu,\"m\":%u,\"m_r\":%u,\"min_step_ticks\":%llu,\"min_release_ticks\":%llu,\"commit\":\"%s\"",
	       im.impl, im.algo, p.n, p.rho, p.run, (unsigned long long)p.b, r.ns_step, r.ns_release, nr.ns_step, nr.ns_release, (unsigned long long)r.q.oversub,
	       (unsigned long long)r.q.refusals, (unsigned long long)r.q.picks, cv, (unsigned long long)r.q.samples, p.f, (unsigned long long)p.w, p.m, p.m_r,
	       (unsigned long long)r.min_step_ticks, (unsigned long long)r.min_rel_ticks, im.commit);
	lb_print_ticks("step_blocks", r.step_ticks, p.m, npt, p.b);
	lb_print_ticks("release_blocks", r.rel_ticks, p.m_r, npt, p.b);
	lb_print_ticks("null_blocks", nr.step_ticks, p.m, npt, p.b);
	printf("}\n");
	fflush(stdout);
	return 0;
}

#if defined(__cplusplus)
#pragma GCC diagnostic pop
#endif

#endif /* KILN_LB_BENCH_COMMON_H */
