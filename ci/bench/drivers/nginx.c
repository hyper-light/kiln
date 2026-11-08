/*
 * ci/bench/drivers/nginx.c: nginx's balancers under common.h's workload.
 *
 * Linked against nginx's own objects (release-1.31.8, 140aa942), nginx.c
 * among them with its main() renamed. The driver starts nginx as nginx's main()
 * does, up to ngx_init_cycle(), on a generated nginx.conf whose one upstream
 * block holds the N servers and the algorithm's directive: the peers are
 * nginx's, built by the algorithm's own init_upstream. No "zone": nginx's
 * default, so the peers are the worker's own and take no lock.
 *
 * Per request nginx's upstream module calls us->peer.init(r, us) (the
 * algorithm's per-request setup: the tried bitmap, the hash of the key) and
 * then pc->get(pc, pc->data); when the request ends, pc->free(pc, pc->data,
 * state) and the request's pool goes. In 1.31.8 upstream keepalive is on by
 * default (32 cached connections, ngx_http_upstream_keepalive_init_main_conf),
 * so us->peer.init, pc->get and pc->free are keepalive's wrappers around the
 * algorithm's: nginx's real default path, run as it is (its cache stays empty:
 * no connection is ever made). The bench's pick is that init and get;
 * its release is that free, then ngx_reset_pool() on the request's pool (nginx
 * destroys it; the bench keeps its memory for the slot's next request).
 *
 * Requests share one ngx_http_request_t, ngx_http_upstream_t and connection
 * (only one is being picked at a time); each slot keeps what nginx keeps per
 * request for its release: pc->data, pc->free and its pool. The client of each
 * request (ip_hash, hash $binary_remote_addr) is the harness's key: its high
 * 32 bits as an IPv4 address.
 */
#include <ngx_config.h>
#include <ngx_core.h>
#include <ngx_http.h>

#include <sys/stat.h>
#include <unistd.h>

#include "../common.h"

#ifndef LB_NGINX_COMMIT
#define LB_NGINX_COMMIT "140aa942711868a8daeb95810bc9a81f272ada72"
#endif

static const struct {
	const char *algo;
	const char *directive;
} lb_nginx_algos[] = {
	{"round_robin", ""},
	{"least_conn", "least_conn;"},
	{"random", "random;"},
	{"random_two_least_conn", "random two least_conn;"},
	{"hash", "hash $binary_remote_addr;"},
	{"hash_consistent", "hash $binary_remote_addr consistent;"},
	{"ip_hash", "ip_hash;"},
	{"least_time", "least_time header;"},
};

typedef struct {
	void *data;                  /* pc->data: the request's balancer state, in its pool */
	ngx_event_free_peer_pt free; /* pc->free */
	ngx_pool_t *pool;            /* the request's pool */
} lb_nginx_slot;

typedef struct {
	ngx_http_upstream_srv_conf_t *us;
	ngx_http_upstream_rr_peer_t *peers; /* the peers, contiguous: index = peer - peers */
	uint32_t n;
	ngx_http_request_t r;
	ngx_http_upstream_t u;
	ngx_connection_t c;
	struct sockaddr_in sin;
	ngx_http_upstream_state_t state;
	lb_nginx_slot *slots;
} lb_nginx;

static lb_nginx lb_ngx;
static ngx_cycle_t lb_init_cycle;

static int32_t lb_nginx_pick(void *ctx, uint32_t slot, uint64_t key)
{
	lb_nginx *b = ctx;
	lb_nginx_slot *sl = &b->slots[slot];

	b->sin.sin_addr.s_addr = (in_addr_t)(key >> 32);
	b->r.pool = sl->pool;
	b->u.peer.data = NULL; /* a new request's upstream holds no balancer state yet */

	if (b->us->peer.init(&b->r, b->us) != NGX_OK || b->u.peer.get(&b->u.peer, b->u.peer.data) != NGX_OK) {
		ngx_reset_pool(sl->pool);
		return -1;
	}
	sl->data = b->u.peer.data;
	sl->free = b->u.peer.free;
	/* The balancer's get sets pc->name to &peer->name (under keepalive's wrapper too): the peer is around it. */

	return (int32_t)((ngx_http_upstream_rr_peer_t *)((u_char *)b->u.peer.name - offsetof(ngx_http_upstream_rr_peer_t, name)) - b->peers);
}

static void lb_nginx_release(void *ctx, uint32_t slot)
{
	lb_nginx *b = ctx;
	lb_nginx_slot *sl = &b->slots[slot];

	sl->free(&b->u.peer, sl->data, 0);
	ngx_reset_pool(sl->pool);
}

static int lb_nginx_check(void *ctx, char *why, size_t len)
{
	lb_nginx *b = ctx;
	for (uint32_t i = 0; i < b->n; i++)
		if (b->peers[i].conns != 0) {
			snprintf(why, len, "peer %u holds %lu connections after every release", i, (unsigned long)b->peers[i].conns);
			return 1;
		}
	return 0;
}

static int lb_nginx_write_conf(const char *path, const char *directive, uint32_t n, unsigned max_conns)
{
	FILE *f = fopen(path, "w");
	char server_kw[32] = "";
	if (!f)
		return -1;
	if (max_conns)
		snprintf(server_kw, sizeof(server_kw), " max_conns=%u", max_conns);
	fprintf(f, "daemon off;\nmaster_process off;\nerror_log stderr warn;\nevents {}\nhttp {\n    upstream u {\n        %s\n", directive);
	for (uint32_t i = 0; i < n; i++)
		fprintf(f, "        server 10.%u.%u.%u:80%s;\n", (i >> 16) & 255u, (i >> 8) & 255u, i & 255u, server_kw);
	fprintf(f, "    }\n}\n");
	return fclose(f);
}

/* nginx's main() up to ngx_init_cycle(), with the options it would have parsed set directly. */
static ngx_cycle_t *lb_nginx_start(char *prefix, char *conf)
{
	static char *argv0[] = {"nginx", NULL};
	ngx_log_t *log;
	ngx_cycle_t *cycle;

	if (ngx_strerror_init() != NGX_OK)
		return NULL;
	ngx_max_sockets = -1;
	ngx_time_init();
	ngx_pid = ngx_getpid();
	ngx_parent = ngx_getppid();
	log = ngx_log_init((u_char *)prefix, (u_char *)"");
	if (log == NULL)
		return NULL;
	ngx_memzero(&lb_init_cycle, sizeof(ngx_cycle_t));
	lb_init_cycle.log = log;
	ngx_cycle = &lb_init_cycle;
	lb_init_cycle.pool = ngx_create_pool(1024, log);
	if (lb_init_cycle.pool == NULL)
		return NULL;

	/* ngx_save_argv() */
	ngx_os_argv = argv0;
	ngx_argc = 1;
	ngx_argv = argv0;

	/* ngx_process_options(): -p prefix -c conf -e stderr */
	lb_init_cycle.prefix.len = ngx_strlen(prefix);
	lb_init_cycle.prefix.data = (u_char *)prefix;
	lb_init_cycle.conf_prefix = lb_init_cycle.prefix;
	lb_init_cycle.conf_file.len = ngx_strlen(conf);
	lb_init_cycle.conf_file.data = (u_char *)conf;
	ngx_str_set(&lb_init_cycle.error_log, "stderr");

	if (ngx_os_init(log) != NGX_OK)
		return NULL;
	if (ngx_crc32_table_init() != NGX_OK)
		return NULL;
	ngx_slab_sizes_init();
	/* ngx_add_inherited_sockets() is skipped: it imports sockets passed on a binary upgrade, and there are none. */
	if (ngx_preinit_modules() != NGX_OK)
		return NULL;
	cycle = ngx_init_cycle(&lb_init_cycle);
	if (cycle == NULL)
		return NULL;
	ngx_cycle = cycle;
	return cycle;
}

/*
 * ALGO is nginx's algorithm, optionally "/max_conns=K": every server then has
 * max_conns=K, nginx's admission (a full peer is skipped; none left is NGX_BUSY,
 * a refusal). Without it, nginx's default: no limit.
 */
static int lb_nginx_setup(const char *algo, uint32_t n, uint32_t slots, lb_impl *out)
{
	const char *directive = NULL;
	char base_algo[64];
	unsigned max_conns = 0;
	const char *slash = strchr(algo, '/');
	snprintf(base_algo, sizeof(base_algo), "%.*s", slash ? (int)(slash - algo) : (int)strlen(algo), algo);
	if (slash && (sscanf(slash, "/max_conns=%u", &max_conns) != 1 || !max_conns)) {
		fprintf(stderr, "nginx: %s: the only variant is /max_conns=K\n", algo);
		return -1;
	}
	for (size_t i = 0; i < sizeof(lb_nginx_algos) / sizeof(lb_nginx_algos[0]); i++)
		if (!strcmp(base_algo, lb_nginx_algos[i].algo))
			directive = lb_nginx_algos[i].directive;
	if (!directive) {
		fprintf(stderr, "nginx: unknown algorithm %s (have:", algo);
		for (size_t i = 0; i < sizeof(lb_nginx_algos) / sizeof(lb_nginx_algos[0]); i++)
			fprintf(stderr, " %s", lb_nginx_algos[i].algo);
		fprintf(stderr, ")\n");
		return -1;
	}

	/* nginx's prefix: one directory per (algorithm, N) under the bench's build tree, rewritten each run. */
	const char *home = getenv("HOME");
	char base[1024], prefix[1100], conf[1200];
	if (!home)
		return -1;
	snprintf(base, sizeof(base), "%s/.cache/kiln-lb/bench/build/nginx-run", home);
	mkdir(base, 0755);
	snprintf(prefix, sizeof(prefix), "%s/%s-%u-%u/", base, base_algo, max_conns, n);
	if (mkdir(prefix, 0755) && errno != EEXIST)
		return -1;
	snprintf(conf, sizeof(conf), "%snginx.conf", prefix);
	if (lb_nginx_write_conf(conf, directive, n, max_conns))
		return -1;

	ngx_cycle_t *cycle = lb_nginx_start(strdup(prefix), strdup(conf));
	if (!cycle) {
		fprintf(stderr, "nginx: ngx_init_cycle failed\n");
		return -1;
	}

	lb_nginx *b = &lb_ngx;
	b->n = n;
	ngx_http_upstream_main_conf_t *umcf = ngx_http_cycle_get_module_main_conf(cycle, ngx_http_upstream_module);
	ngx_http_upstream_srv_conf_t **uscfp = umcf->upstreams.elts;
	for (ngx_uint_t i = 0; i < umcf->upstreams.nelts; i++)
		if (uscfp[i]->host.len == 1 && uscfp[i]->host.data[0] == 'u')
			b->us = uscfp[i];
	if (!b->us || !b->us->peer.data)
		return -1;
	ngx_http_upstream_rr_peers_t *peers = b->us->peer.data;
	if (peers->number != n || peers->next) {
		fprintf(stderr, "nginx: %lu peers, wanted %u\n", (unsigned long)peers->number, n);
		return -1;
	}
	b->peers = peers->peer;
	ngx_http_upstream_rr_peer_t *p = peers->peer;
	for (uint32_t i = 0; i < n; i++, p = p->next)
		if (p != &b->peers[i]) {
			fprintf(stderr, "nginx: peers are not contiguous at %u\n", i);
			return -1;
		}

	/* The request every pick runs on: what nginx's request code would have set that the balancers read. */
	ngx_http_conf_ctx_t *hctx = (ngx_http_conf_ctx_t *)cycle->conf_ctx[ngx_http_module.index];
	ngx_http_core_main_conf_t *cmcf = hctx->main_conf[ngx_http_core_module.ctx_index];
	b->r.main_conf = hctx->main_conf;
	b->r.srv_conf = hctx->srv_conf;
	b->r.loc_conf = hctx->loc_conf;
	b->r.variables = ngx_pcalloc(cycle->pool, (cmcf->variables.nelts + 1) * sizeof(ngx_http_variable_value_t));
	b->r.connection = &b->c;
	b->r.upstream = &b->u;
	b->c.log = cycle->log;
	b->c.sockaddr = (struct sockaddr *)&b->sin;
	b->c.socklen = sizeof(b->sin);
	b->sin.sin_family = AF_INET;
#if defined(__APPLE__)
	b->sin.sin_len = sizeof(b->sin);
#endif
	b->u.peer.log = cycle->log;
	b->u.peer.log_error = NGX_ERROR_ERR;
	b->u.state = &b->state; /* least_time reads it: no response times in this workload */

	/* Each request's pool: nginx's pool header, the balancer's state, the tried bitmap, the key. */
	size_t tried = n <= 8 * sizeof(uintptr_t) ? 0 : ((n + 8 * sizeof(uintptr_t) - 1) / (8 * sizeof(uintptr_t))) * sizeof(uintptr_t);
	size_t psize = sizeof(ngx_pool_t) + 512 + tried;
	b->slots = calloc(slots, sizeof(*b->slots));
	if (!b->slots)
		return -1;
	for (uint32_t s = 0; s < slots; s++) {
		b->slots[s].pool = ngx_create_pool(psize, cycle->log);
		if (!b->slots[s].pool)
			return -1;
	}

	out->impl = "nginx";
	out->algo = algo;
	out->commit = LB_NGINX_COMMIT;
	out->ctx = b;
	out->release = lb_nginx_release;
	out->pick = lb_nginx_pick;
	out->check = lb_nginx_check;
	return 0;
}

int main(int argc, char **argv)
{
	return lb_main(argc, argv, lb_nginx_setup);
}
