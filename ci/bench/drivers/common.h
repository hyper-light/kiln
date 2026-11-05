/* The selection kernels' common driver (ci/bench/README.md, "The load model"): every competitor's
 * driver (nginx, HAProxy, Envoy) and kiln's bench (crates/kiln-ci/benches/lb_vs.rs) run the same
 * trace, time the same way and report the same fields, so their numbers compare.
 *
 * The trace: N backends of C slots each; K = round(rho * N * C) requests held in flight. Warm-up
 * picks K requests (untimed). Each step then frees one in-flight request chosen uniformly at random
 * (equal exponential service times: the population stays K) and picks one new request (timed).
 * The departures are drawn by slot index from one seeded splitmix64, so every competitor sees the
 * same departure sequence whatever it picked. rho = 0 is Envoy's stock bench: every backend idle,
 * each request freed right after it is picked.
 *
 * Timing: CLOCK_MONOTONIC (vDSO) around each pick, as Rust's Instant and C++'s steady_clock read
 * it; the median of back-to-back reads (the timer's own cost) is measured first and subtracted.
 *
 * Quality, beside the speed, counted outside the timed region:
 *   - oversubscribed: picks onto a backend already holding C requests (it had no room);
 *   - refused: picks that returned no backend;
 *   - cv: the coefficient of variation of in-flight counts across backends, averaged over steps;
 *   - peak: the most-loaded backend's count over the mean count, averaged over steps.
 *
 * Output: one line of JSON on stdout, and with -o the raw per-pick nanoseconds (u32 LE) for the
 * analysis (ci/bench/analyze.py). */
#ifndef KILN_BENCH_COMMON_H
#define KILN_BENCH_COMMON_H

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef struct {
    const char *algo;
    uint32_t n, c;
    double rho;
    uint64_t steps, seed;
    const char *out;
} kb_args;

static inline uint64_t kb_splitmix(uint64_t *s) {
    uint64_t z = (*s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static inline uint64_t kb_now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}

static int kb_cmp64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

/* The timer's own cost: the median of back-to-back reads. */
static inline uint64_t kb_overhead(void) {
    enum { R = 100001 };
    static uint64_t d[R];
    for (int i = 0; i < R; i++) {
        uint64_t a = kb_now(), b = kb_now();
        d[i] = b - a;
    }
    qsort(d, R, sizeof d[0], kb_cmp64);
    return d[R / 2];
}

static inline int kb_parse(int argc, char **argv, kb_args *a) {
    memset(a, 0, sizeof *a);
    a->c = 8;
    a->steps = 200000;
    a->seed = 1;
    for (int i = 1; i + 1 < argc; i += 2) {
        if (!strcmp(argv[i], "-a")) a->algo = argv[i + 1];
        else if (!strcmp(argv[i], "-n")) a->n = (uint32_t)strtoul(argv[i + 1], 0, 10);
        else if (!strcmp(argv[i], "-c")) a->c = (uint32_t)strtoul(argv[i + 1], 0, 10);
        else if (!strcmp(argv[i], "-r")) a->rho = strtod(argv[i + 1], 0);
        else if (!strcmp(argv[i], "-s")) a->steps = strtoull(argv[i + 1], 0, 10);
        else if (!strcmp(argv[i], "-S")) a->seed = strtoull(argv[i + 1], 0, 10);
        else if (!strcmp(argv[i], "-o")) a->out = argv[i + 1];
        else return -1;
    }
    if (!a->algo || a->n < 2 || a->c < 1 || a->rho < 0 || a->rho >= 1.0 + 1e-9) {
        fprintf(stderr, "usage: -a algo -n N [-c C] [-r rho in [0,1]] [-s steps] [-S seed] [-o raw]\n");
        return -1;
    }
    return 0;
}

/* The quality bookkeeping: in-flight count per backend, the sum of squares and the count of
 * backends at each level, so each step's CV and peak cost O(1). Outside the timed region. */
typedef struct {
    uint32_t n, c, max;
    uint32_t *load;   /* per backend */
    uint32_t *level;  /* backends at each count, up to the population */
    uint64_t total, sumsq;
    double cv, peak;
    uint64_t samples, over, refused;
} kb_quality;

static inline void kb_quality_init(kb_quality *q, uint32_t n, uint32_t c, uint64_t k) {
    memset(q, 0, sizeof *q);
    q->n = n, q->c = c;
    q->load = (uint32_t *)calloc(n, sizeof *q->load);
    q->level = (uint32_t *)calloc(k + 2, sizeof *q->level);
    q->level[0] = n;
}

static inline void kb_quality_add(kb_quality *q, uint32_t b) {
    uint32_t l = q->load[b];
    if (l >= q->c) q->over++;
    q->level[l]--, q->level[l + 1]++;
    q->load[b] = l + 1;
    q->total++;
    q->sumsq += 2ULL * l + 1;
    if (l + 1 > q->max) q->max = l + 1;
}

static inline void kb_quality_drop(kb_quality *q, uint32_t b) {
    uint32_t l = q->load[b];
    q->level[l]--, q->level[l - 1]++;
    q->load[b] = l - 1;
    q->total--;
    q->sumsq -= 2ULL * l - 1;
    while (q->max > 0 && q->level[q->max] == 0) q->max--;
}

static inline void kb_quality_sample(kb_quality *q) {
    if (q->total == 0) return;
    double mean = (double)q->total / q->n;
    double var = (double)q->sumsq / q->n - mean * mean;
    q->cv += sqrt(var > 0 ? var : 0) / mean;
    q->peak += q->max / mean;
    q->samples++;
}

/* The in-flight population: slot i holds a request's backend and the driver's own handle. */
typedef struct {
    uint32_t backend;
    void *handle;
} kb_req;

typedef struct {
    uint64_t *ns;
    uint64_t picks, overhead;
} kb_times;

static inline void kb_report(const char *who, const kb_args *a, uint64_t k, kb_times *t, kb_quality *q, const char *extra) {
    uint64_t sum = 0;
    for (uint64_t i = 0; i < t->picks; i++) sum += t->ns[i];
    if (a->out) {
        FILE *f = fopen(a->out, "wb");
        if (f) {
            for (uint64_t i = 0; i < t->picks; i++) {
                uint32_t v = t->ns[i] > 0xffffffffULL ? 0xffffffffU : (uint32_t)t->ns[i];
                fwrite(&v, sizeof v, 1, f);
            }
            fclose(f);
        }
    }
    uint64_t *s = (uint64_t *)malloc((t->picks ? t->picks : 1) * sizeof *s);
    memcpy(s, t->ns, t->picks * sizeof *s);
    qsort(s, t->picks, sizeof *s, kb_cmp64);
    uint64_t p50 = t->picks ? s[(t->picks - 1) * 50 / 100] : 0, p99 = t->picks ? s[(t->picks - 1) * 99 / 100] : 0;
    double mean = t->picks ? (double)sum / t->picks : 0;
    printf("{\"who\":\"%s\",\"algo\":\"%s\",\"n\":%u,\"c\":%u,\"rho\":%.3f,\"d\":1,\"k\":%llu,\"steps\":%llu,\"seed\":%llu,"
           "\"overhead_ns\":%llu,\"p50_ns\":%llu,\"p99_ns\":%llu,\"mean_ns\":%.2f,\"per_s\":%.0f,"
           "\"over\":%llu,\"refused\":%llu,\"cv\":%.5f,\"peak\":%.5f%s%s}\n",
           who, a->algo, a->n, a->c, a->rho, (unsigned long long)k, (unsigned long long)t->picks, (unsigned long long)a->seed,
           (unsigned long long)t->overhead, (unsigned long long)p50, (unsigned long long)p99, mean, mean > 0 ? 1e9 / mean : 0.0,
           (unsigned long long)q->over, (unsigned long long)q->refused, q->samples ? q->cv / q->samples : 0.0,
           q->samples ? q->peak / q->samples : 0.0, extra && *extra ? "," : "", extra ? extra : "");
    free(s);
}

/* One timed interval, the timer's cost taken off (never below 0). */
static inline uint64_t kb_less(uint64_t a, uint64_t b, uint64_t overhead) {
    uint64_t d = b - a;
    return d > overhead ? d - overhead : 0;
}

/* The trace, as a macro so the competitor's pick is called directly inside the timed region (no
 * indirect call of the driver's own to inflate it). PREP runs before each pick, untimed (a
 * request's own setup); PICK sets `int kb_b` (the backend, or -1 for a refusal) and `void *kb_h`
 * (the driver's handle for the request); POST runs after the clock is read, untimed (KB_RUN2
 * alone: to find the backend's index from the handle when the competitor has no cheap one); FREE
 * releases `kb_r`. */
#define KB_RUN(who, a, extra_fmt, PREP, PICK, FREE) KB_RUN2(who, a, extra_fmt, PREP, PICK, (void)0, FREE)
#define KB_RUN2(who, a, extra_fmt, PREP, PICK, POST, FREE)                                                         \
    do {                                                                                             \
        uint64_t kb_k = (uint64_t)llround((a)->rho * (double)(a)->n * (double)(a)->c);               \
        kb_quality kb_q;                                                                             \
        kb_quality_init(&kb_q, (a)->n, (a)->c, kb_k + 1);                                            \
        kb_req *kb_reqs = (kb_req *)calloc(kb_k + 1, sizeof *kb_reqs);                                         \
        kb_times kb_t = {(uint64_t *)calloc((a)->steps, sizeof(uint64_t)), 0, kb_overhead()};                    \
        uint64_t kb_seed = (a)->seed, kb_live = 0;                                                   \
        for (uint64_t kb_i = 0; kb_i < kb_k; kb_i++) {                                               \
            int kb_b = -1;                                                                           \
            void *kb_h = NULL;                                                                       \
            PREP;                                                                                    \
            PICK;                                                                                    \
            POST;                                                                                    \
            if (kb_b < 0) { kb_q.refused++; continue; }                                              \
            kb_quality_add(&kb_q, (uint32_t)kb_b);                                                   \
            kb_reqs[kb_live].backend = (uint32_t)kb_b, kb_reqs[kb_live++].handle = kb_h;                                     \
        }                                                                                            \
        kb_q.over = 0, kb_q.refused = 0;                                                             \
        for (uint64_t kb_s = 0; kb_s < (a)->steps; kb_s++) {                                         \
            if (kb_live > 0 && kb_k > 0) {                                                           \
                uint64_t kb_j = kb_splitmix(&kb_seed) % kb_live;                                     \
                kb_req *kb_r = &kb_reqs[kb_j];                                                       \
                FREE;                                                                                \
                kb_quality_drop(&kb_q, kb_r->backend);                                               \
                kb_reqs[kb_j] = kb_reqs[--kb_live];                                                  \
            }                                                                                        \
            int kb_b = -1;                                                                           \
            void *kb_h = NULL;                                                                       \
            PREP;                                                                                    \
            uint64_t kb_t0 = kb_now();                                                               \
            PICK;                                                                                    \
            uint64_t kb_t1 = kb_now();                                                               \
            kb_t.ns[kb_t.picks++] = kb_less(kb_t0, kb_t1, kb_t.overhead);                            \
            POST;                                                                                    \
            if (kb_b < 0) { kb_q.refused++; continue; }                                              \
            kb_quality_add(&kb_q, (uint32_t)kb_b);                                                   \
            kb_quality_sample(&kb_q);                                                                \
            if (kb_k == 0) {                                                                         \
                kb_req kb_one; kb_one.backend = (uint32_t)kb_b, kb_one.handle = kb_h; kb_req *kb_r = &kb_one;                             \
                FREE;                                                                                \
                kb_quality_drop(&kb_q, kb_r->backend);                                               \
            } else {                                                                                 \
                kb_reqs[kb_live].backend = (uint32_t)kb_b, kb_reqs[kb_live++].handle = kb_h;                                 \
            }                                                                                        \
        }                                                                                            \
        char kb_extra[256];                                                                          \
        snprintf(kb_extra, sizeof kb_extra, "%s", extra_fmt);                                        \
        kb_report(who, a, kb_k, &kb_t, &kb_q, kb_extra);                                             \
    } while (0)

#endif
