/* HAProxy's own selection kernels under the common trace (common.h): `balance leastconn`
 * (lb_fwlc.c, the eb32 tree keyed by served/weight), `balance roundrobin` (lb_fwrr.c, the
 * weighted round-robin trees) and `balance random(2)` (backend.c's get_server_rnd over
 * lb_chash.c's tree: two draws, the less loaded kept), linked against HAProxy's own objects at
 * the pinned release (ci/bench/README.md), haproxy.o with its main renamed.
 *
 * The backend is HAProxy's own: a configuration (`backend bench`, `balance <algo>`, N `server`
 * lines of weight 1, `nbthread 1`) parsed by parse_cfg() and checked by check_config_validity(),
 * after the initcalls main() runs (STG_PREPARE to STG_INIT: pools, lb_ops registration, keywords),
 * then the post server/proxy checks, the admin states and the addresses, as step_init_2 does.
 * check_config_validity() calls proxy_finalize(), which picks the backend's lb_ops and runs its
 * proxy_init: fwlc_init_server_tree, fwrr_init_server_groups or chash_init_server_tree. Nothing
 * of HAProxy is stubbed; what isn't run is its listeners, pollers, tasks and the per-thread inits
 * (the driver is thread 1 of 1, set with ha_set_thread as run_thread_poll_loop does).
 *
 * Each request is a struct stream of HAProxy's (zeroed; s->be the backend, its server slot
 * initialised by stream_init_srv_conn). A pick is what assign_server() and
 * assign_server_and_queue() do for these algorithms: the lookup (fwlc_get_next_server,
 * fwrr_get_next_server or get_server_rnd), then, no maxconn being set, `_HA_ATOMIC_INC(&srv->served)`
 * and sess_change_server(s, srv): the backend's served, the lb's server_take_conn
 * (fwlc_srv_reposition for leastconn; none for roundrobin and random) and the stream on the
 * server's list. All of that is timed (it is HAProxy's per-assignment work); the stats counters
 * (cum_lbconn) and the target bookkeeping are left out (nginx's get doesn't count either).
 * A request's end is stream_free()'s: sess_change_server(s, NULL) (served down, server_drop_conn)
 * and the queue check.
 *
 * random(2) breaks a tie in load by the servers' sess_per_sec, which only connect_server() feeds
 * (from the clock); the driver connects nothing, so every rate reads 0 and a tie keeps the later
 * draw, which keeps the quality the same run to run for a seed. The locks are HAProxy's real ones
 * (threads built in): lbprm.lock read-locked by the leastconn and random lookups and by
 * fwlc_srv_reposition, the per-group fwrr lock write-locked by roundrobin; one thread, so never
 * contended. */
#include <haproxy/api.h>
#include <haproxy/backend.h>
#include <haproxy/cfgparse.h>
#include <haproxy/clock.h>
#include <haproxy/global.h>
#include <haproxy/init.h>
#include <haproxy/initcall.h>
#include <haproxy/lb_fwlc.h>
#include <haproxy/lb_fwrr.h>
#include <haproxy/proxy.h>
#include <haproxy/queue.h>
#include <haproxy/server.h>
#include <haproxy/stream.h>
#include <haproxy/thread.h>
#include <haproxy/tools.h>

#include "common.h"

enum { ALGO_LC, ALGO_RR, ALGO_RND };

static struct proxy *px;

/* The configuration HAProxy parses: one backend, N servers of weight 1, one thread (the timeouts
 * only keep check_config_validity from warning). */
static char *config(const char *balance, uint32_t n, size_t *len) {
    size_t cap = 256 + (size_t)n * 64, at = 0;
    char *c = malloc(cap);
    at += (size_t)snprintf(c + at, cap - at, "global\n    nbthread 1\n    quiet\n\nbackend bench\n    mode tcp\n    timeout connect 5s\n    timeout server 30s\n    balance %s\n", balance);
    for (uint32_t i = 0; i < n; i++)
        at += (size_t)snprintf(c + at, cap - at, "    server s%u 10.%u.%u.%u:8000 weight 1\n", i, (i >> 16) & 255, (i >> 8) & 255, i & 255);
    *len = at;
    return c;
}

/* HAProxy's start-up, as far as a backend needs it: main()'s initcalls and step_init_2's checks. */
static int haproxy_init(const char *balance, uint32_t n, uint64_t seed) {
    struct pre_check_fct *prcf;
    struct post_server_check_fct *pscf;
    struct post_proxy_check_fct *ppcf;
    struct proxy *p;
    struct server *srv;
    int err = 0;

    /* ha_random_boot's seed, made the run's own so the random draws repeat. */
    ha_random_seed((const unsigned char *)&seed, sizeof seed);
    RUN_INITCALLS(STG_PREPARE);
    RUN_INITCALLS(STG_LOCK);
    RUN_INITCALLS(STG_REGISTER);
    /* init_early's part that matters here: the mode and the clock. */
    global.mode = MODE_STARTING;
    clock_init_process_date();
    RUN_INITCALLS(STG_ALLOC);
    RUN_INITCALLS(STG_POOL);
    RUN_INITCALLS(STG_INIT);

    struct cfgfile cfg = {.filename = "haproxy_pick.cfg"};
    size_t len;
    cfg.content = config(balance, n, &len);
    cfg.size = (ssize_t)len;
    err = parse_cfg(&cfg);
    if (err & (ERR_ABORT | ERR_FATAL)) return -1;

    /* step_init_2, in order, up to the server addresses. */
    list_for_each_entry(prcf, &pre_check_list, list) {
        err |= prcf->fct();
        if (err & (ERR_ABORT | ERR_FATAL)) return -1;
    }
    clock_update_date(0, 1);
    clock_adjust_now_offset();
    thread_detect_count();
    err |= check_config_validity();
    if (err & (ERR_ABORT | ERR_FATAL)) return -1;
    list_for_each_entry(p, &proxies, global_list) {
        if (p->flags & (PR_FL_DISABLED | PR_FL_STOPPED)) continue;
        list_for_each_entry(pscf, &post_server_check_list, list) {
            for (srv = p->srv; srv; srv = srv->next) {
                err |= pscf->fct(srv);
                if (err & (ERR_ABORT | ERR_FATAL)) return -1;
            }
        }
        list_for_each_entry(ppcf, &post_proxy_check_list, list) {
            err |= ppcf->fct(p);
            if (err & (ERR_ABORT | ERR_FATAL)) return -1;
        }
        p->flags |= PR_FL_CHECKED;
    }
    for (p = proxies_list; p; p = p->next) srv_compute_all_admin_states(p);
    err |= srv_init_addr();
    if (err & (ERR_ABORT | ERR_FATAL)) return -1;

    /* This is thread 1 of 1, as run_thread_poll_loop sets it; its statistical PRNG (which
     * init_tools_per_thread would seed from ha_random32) is seeded from the run's seed. */
    ha_set_thread(&ha_thread_info[0]);
    statistical_prng_state = (unsigned int)(seed * 2654435761u) | 1u;

    px = proxy_be_by_name("bench");
    if (!px || !px->lbprm.ops) return -1;
    return 0;
}

/* The backend index of a request's server: its puid, numbered 1..N in configuration order by
 * proxy_finalize (checked at start). Read in POST, after the clock. */
static inline int backend_of(const struct stream *s) {
    return s && s->srv_conn ? (int)s->srv_conn->puid - 1 : -1;
}

int main(int argc, char **argv) {
    kb_args a;
    if (kb_parse(argc, argv, &a)) return 2;

    int algo;
    const char *balance;
    if (!strcmp(a.algo, "leastconn")) algo = ALGO_LC, balance = "leastconn";
    else if (!strcmp(a.algo, "roundrobin")) algo = ALGO_RR, balance = "roundrobin";
    else if (!strcmp(a.algo, "random")) algo = ALGO_RND, balance = "random(2)";
    else {
        fprintf(stderr, "haproxy_pick: unknown algo %s\n", a.algo);
        return 2;
    }
    if (haproxy_init(balance, a.n, a.seed)) {
        fprintf(stderr, "haproxy_pick: HAProxy's configuration failed\n");
        return 1;
    }
    uint32_t i = 0;
    for (struct server *srv = px->srv; srv; srv = srv->next, i++) {
        if (srv->puid != (int)i + 1 || !srv_currently_usable(srv)) {
            fprintf(stderr, "haproxy_pick: server %s is not #%u or not up\n", srv->id, i + 1);
            return 1;
        }
    }
    if (i != a.n || px->lbprm.tot_weight == 0 || (algo == ALGO_RND && px->lbprm.arg_opt1 != 2)) {
        fprintf(stderr, "haproxy_pick: backend has %u servers, weight %u, draws %d\n", i, px->lbprm.tot_weight, px->lbprm.arg_opt1);
        return 1;
    }

    /* A pool of streams: one per request in flight, plus the one being picked. */
    uint64_t k = (uint64_t)llround(a.rho * (double)a.n * (double)a.c);
    struct stream **free_slots = calloc(k + 2, sizeof *free_slots);
    uint64_t nfree = 0;
    for (uint64_t j = 0; j < k + 2; j++) {
        struct stream *s = calloc(1, sizeof *s);
        s->obj_type = OBJ_TYPE_STREAM;
        s->be = px;
        stream_init_srv_conn(s);
        free_slots[nfree++] = s;
    }

/* PREP: nothing (a stream's own setup isn't the pick's). PICK: the lookup assign_server()
 * makes for the algorithm, then assign_server_and_queue()'s reservation of the slot (timed).
 * POST: the server's index (untimed). FREE: stream_free()'s release (untimed). */
#define PREP (void)0
#define PICK                                                                  \
    do {                                                                      \
        struct stream *s = free_slots[--nfree];                               \
        struct server *srv;                                                   \
        if (algo == ALGO_LC) srv = fwlc_get_next_server(px, NULL);            \
        else if (algo == ALGO_RR) srv = fwrr_get_next_server(px, NULL);       \
        else srv = get_server_rnd(s, NULL);                                   \
        if (srv) {                                                            \
            _HA_ATOMIC_INC(&srv->served);                                     \
            sess_change_server(s, srv);                                       \
            kb_h = s;                                                         \
        } else {                                                              \
            free_slots[nfree++] = s;                                          \
        }                                                                     \
    } while (0)
#define POST                                                                  \
    do {                                                                      \
        if (kb_h) kb_b = backend_of(kb_h);                                    \
    } while (0)
#define FREE                                                                  \
    do {                                                                      \
        struct stream *s = kb_r->handle;                                      \
        struct server *old = s->srv_conn;                                     \
        sess_change_server(s, NULL);                                          \
        if (may_dequeue_tasks(old, s->be)) process_srv_queue(old);            \
        free_slots[nfree++] = s;                                              \
    } while (0)

    KB_RUN2("haproxy-3.4.7", &a, "", PREP, PICK, POST, FREE);
    return 0;
}
