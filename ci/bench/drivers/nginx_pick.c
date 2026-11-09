/* nginx's own selection kernels under the common trace (common.h): round robin, least_conn and
 * `random two least_conn`, built from nginx's sources at the pinned release (ci/bench/README.md).
 *
 * The least_conn and random modules' functions are static, so this file includes their sources
 * (compiled with the same flags as the rest of nginx); every other nginx object is linked as built,
 * nginx.o with its main renamed. The upstream is set up by the modules' own init functions, as
 * nginx's configuration would (`upstream { least_conn; server ...; }`, N servers of weight 1, no
 * shared zone: no locks, nginx's cheapest case), and each request is picked as nginx picks it:
 * the module's per-request init (`us->peer.init`), then `pc->get` through the pointer it set.
 * Only `pc->get` is timed (the init is the request's, not the pick's: leaving it out favours nginx).
 * A request's end is `pc->free` (nginx's round robin free: conns back down). */
#include <ngx_config.h>
#include <ngx_core.h>
#include <ngx_http.h>

#include "ngx_http_upstream_least_conn_module.c"
#include "ngx_http_upstream_random_module.c"

#include "common.h"

typedef struct {
    ngx_pool_t *pool;
    ngx_http_request_t r;
    ngx_http_upstream_t u;
} req_t;

static ngx_http_upstream_srv_conf_t us;
static ngx_http_upstream_rr_peer_t *base;
static ngx_log_t blog;
static ngx_open_file_t blog_file;

static req_t *slot(void) {
    req_t *q = calloc(1, sizeof *q);
    q->pool = ngx_create_pool(4096, &blog);
    q->r.pool = q->pool;
    q->r.upstream = &q->u;
    return q;
}

/* One request's pick: the module's per-request init (untimed), then pc->get (timed by the caller
 * through PICK_GET). Returns the request, its peer chosen in q->u.peer. */
static inline int pick_init(req_t *q) {
    ngx_reset_pool(q->pool);
    ngx_memzero(&q->u.peer, sizeof q->u.peer);
    q->u.peer.log = &blog;
    q->u.peer.data = NULL;
    return us.peer.init(&q->r, &us) == NGX_OK ? 0 : -1;
}

static inline int backend_of(req_t *q) {
    ngx_http_upstream_rr_peer_data_t *rrp = q->u.peer.data;
    return rrp && rrp->current ? (int)(rrp->current - base) : -1;
}

int main(int argc, char **argv) {
    kb_args a;
    if (kb_parse(argc, argv, &a)) return 2;

    ngx_pagesize = getpagesize();
    ngx_pagesize_shift = 0;
    for (ngx_uint_t n = ngx_pagesize; n >>= 1; ngx_pagesize_shift++) {}
    ngx_cacheline_size = 64;
    ngx_time_init();
    blog_file.fd = ngx_stderr;
    blog.file = &blog_file;
    blog.log_level = NGX_LOG_ERR;
    srandom((unsigned)a.seed);

    ngx_pool_t *pool = ngx_create_pool(1 << 20, &blog);
    ngx_conf_t cf;
    ngx_memzero(&cf, sizeof cf);
    cf.pool = pool;
    cf.log = &blog;

    us.servers = ngx_array_create(pool, a.n, sizeof(ngx_http_upstream_server_t));
    for (uint32_t i = 0; i < a.n; i++) {
        ngx_http_upstream_server_t *s = ngx_array_push(us.servers);
        ngx_memzero(s, sizeof *s);
        struct sockaddr_in *sin = ngx_pcalloc(pool, sizeof *sin);
        sin->sin_family = AF_INET;
        sin->sin_port = htons(8000);
        sin->sin_addr.s_addr = htonl(0x0a000000u + i);
        ngx_addr_t *addr = ngx_pcalloc(pool, sizeof *addr);
        addr->sockaddr = (struct sockaddr *)sin;
        addr->socklen = sizeof *sin;
        addr->name.data = ngx_pnalloc(pool, 32);
        addr->name.len = ngx_sprintf(addr->name.data, "10.%ud.%ud.%ud:8000", (i >> 16) & 255, (i >> 8) & 255, i & 255) - addr->name.data;
        s->name = addr->name;
        s->addrs = addr;
        s->naddrs = 1;
        s->weight = 1;
        s->max_fails = 1;
        s->fail_timeout = 10;
    }
    ngx_str_set(&us.host, "bench");

    /* The module's configuration, where ngx_http_conf_upstream_srv_conf finds it. */
    ngx_http_upstream_random_srv_conf_t rcf;
    ngx_memzero(&rcf, sizeof rcf);
    void *srv_conf[1] = {&rcf};
    ngx_http_upstream_random_module.ctx_index = 0;
    us.srv_conf = srv_conf;

    ngx_int_t rc;
    if (!strcmp(a.algo, "round_robin")) {
        rc = ngx_http_upstream_init_round_robin(&cf, &us);
    } else if (!strcmp(a.algo, "least_conn")) {
        rc = ngx_http_upstream_init_least_conn(&cf, &us);
    } else if (!strcmp(a.algo, "random_two_least_conn")) {
        rcf.two = 1;
        rc = ngx_http_upstream_init_random(&cf, &us);
    } else if (!strcmp(a.algo, "random")) {
        rc = ngx_http_upstream_init_random(&cf, &us);
    } else {
        fprintf(stderr, "nginx_pick: unknown algo %s\n", a.algo);
        return 2;
    }
    if (rc != NGX_OK) {
        fprintf(stderr, "nginx_pick: init failed\n");
        return 1;
    }
    base = ((ngx_http_upstream_rr_peers_t *)us.peer.data)->peer;

    /* A pool of request slots: one per request in flight, plus the one being picked. */
    uint64_t k = (uint64_t)llround(a.rho * (double)a.n * (double)a.c);
    req_t **free_slots = calloc(k + 2, sizeof *free_slots);
    uint64_t nfree = 0;
    for (uint64_t i = 0; i < k + 2; i++) free_slots[nfree++] = slot();

/* PREP: the module's per-request init on the slot about to be picked (untimed). PICK: pc->get
 * through the pointer the init set, as ngx_event_connect_peer calls it (timed). */
#define PREP (void)pick_init(free_slots[nfree - 1])
#define PICK                                                                  \
    do {                                                                      \
        req_t *q = free_slots[--nfree];                                       \
        if (q->u.peer.get(&q->u.peer, q->u.peer.data) == NGX_OK) {           \
            kb_b = backend_of(q);                                             \
            kb_h = q;                                                         \
        } else {                                                              \
            free_slots[nfree++] = q;                                          \
        }                                                                     \
    } while (0)
#define FREE                                                                  \
    do {                                                                      \
        req_t *q = kb_r->handle;                                              \
        q->u.peer.free(&q->u.peer, q->u.peer.data, 0);                        \
        free_slots[nfree++] = q;                                              \
    } while (0)

    KB_RUN("nginx-1.31.8", &a, "", PREP, PICK, FREE);
    return 0;
}
