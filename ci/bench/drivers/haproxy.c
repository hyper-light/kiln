/*
 * ci/bench/drivers/haproxy.c: HAProxy's balancers under common.h's workload.
 *
 * Linked into HAProxy itself (v3.4.7, 8872a260): every object of its
 * TARGET=osx build, haproxy.c among them with its main() renamed. The
 * driver's main() writes a configuration (one backend: the N servers and the
 * algorithm's "balance" line; one frontend on a unix socket, since HAProxy
 * won't start without a listener) and starts HAProxy on it, so the servers are
 * HAProxy's, set up by its own config parser and startup (check_config_validity
 * builds each algorithm's trees). The bench runs from a per-thread init hook:
 * in thread 1 of nbthread 1, once HAProxy is up and before its poll loop.
 *
 * A pick is what assign_server() and assign_server_and_queue() do for a
 * stream with no server yet: the algorithm's get-server function, each taking
 * the backend's lbprm lock itself (fwrr_get_next_server, map_get_server_rr,
 * fwlc_get_next_server, fas_get_next_server, get_server_rnd, get_server_sh);
 * then the server's slot (served + 1, or a CAS below maxconn when the server
 * has one); then sess_change_server(s, srv): the backend's
 * served + 1 and lbprm.ops->server_take_conn. A release is sess_change_server(s,
 * NULL): served - 1 on the backend and the server, then
 * lbprm.ops->server_drop_conn. In v3.4.7 only leastconn (fwlc_srv_reposition)
 * and first (fas_srv_reposition) define those hooks; roundrobin's fwrr has
 * none. Left out, as not balancing: the stream's own list links
 * (stream_add_srv_conn), the cum_lbconn and session-rate counters, and
 * process_srv_queue() (no queue forms below maxconn).
 */
#include <haproxy/api.h>
#include <haproxy/backend.h>
#include <haproxy/init.h>
#include <haproxy/lb_fas.h>
#include <haproxy/lb_fwlc.h>
#include <haproxy/lb_fwrr.h>
#include <haproxy/lb_map.h>
#include <haproxy/proxy.h>
#include <haproxy/queue.h>
#include <haproxy/server.h>
#include <haproxy/stream-t.h>
#include <haproxy/thread.h>

#include <sys/stat.h>
#include <unistd.h>

#include "../common.h"

#ifndef LB_HAPROXY_COMMIT
#define LB_HAPROXY_COMMIT "8872a2604595140544774c2a6309a41186fc0f44"
#endif

int lb_haproxy_main(int argc, char **argv);

enum lb_hap_kind { LB_HAP_RR, LB_HAP_SRR, LB_HAP_LC, LB_HAP_FIRST, LB_HAP_RANDOM, LB_HAP_SOURCE };

/*
 * ALGO is one of these, optionally "/maxconn=K": every server then has
 * "maxconn K", HAProxy's admission (a full server is skipped, or the request
 * would queue: a refusal here). Without it, HAProxy's default: no limit, so
 * "first" sends everything to the first server, as configured that way.
 */
static const struct {
	const char *algo;
	const char *balance; /* the backend's "balance" line, and any line after it */
	enum lb_hap_kind kind;
} lb_hap_algos[] = {
	{"roundrobin", "roundrobin", LB_HAP_RR},
	{"static-rr", "static-rr", LB_HAP_SRR},
	{"leastconn", "leastconn", LB_HAP_LC},
	{"first", "first", LB_HAP_FIRST},
	{"random", "random(2)", LB_HAP_RANDOM},
	{"source", "source", LB_HAP_SOURCE},
	{"source-consistent", "source\n    hash-type consistent", LB_HAP_SOURCE},
};

typedef struct {
	struct proxy *px;
	enum lb_hap_kind kind;
	uint32_t n;
	struct stream strm;     /* get_server_rnd() reads only s->be */
	struct server **by_idx; /* index (puid - 1) to server */
	struct server **slot;   /* the server each slot's request holds */
} lb_hap;

static lb_hap lb_h;
static int lb_hap_argc;
static char **lb_hap_argv;
static int lb_hap_algo = -1;
static unsigned lb_hap_maxconn;

static int32_t lb_hap_pick(void *ctx, uint32_t slot, uint64_t key)
{
	lb_hap *h = ctx;
	struct proxy *px = h->px;
	struct server *srv = NULL;

	switch (h->kind) {
	case LB_HAP_RR:
		srv = fwrr_get_next_server(px, NULL);
		break;
	case LB_HAP_SRR:
		srv = map_get_server_rr(px, NULL);
		break;
	case LB_HAP_LC:
		srv = fwlc_get_next_server(px, NULL);
		break;
	case LB_HAP_FIRST:
		srv = fas_get_next_server(px, NULL);
		break;
	case LB_HAP_RANDOM:
		srv = get_server_rnd(&h->strm, NULL);
		break;
	case LB_HAP_SOURCE: {
		/* the client's IPv4 address, in network order as in its sockaddr_in */
		uint32_t addr = (uint32_t)(key >> 32);
		srv = get_server_sh(px, (const char *)&addr, 4, NULL);
		break;
	}
	}
	if (!srv)
		return -1;

	/* assign_server_and_queue(): the server's slot */
	if (srv->maxconn) {
		int served = srv->served, got_it = 0;
		if (!srv->queueslength && served < (int)srv_dynamic_maxconn(srv)) {
			do {
				got_it = _HA_ATOMIC_CAS(&srv->served, &served, served + 1);
			} while (!got_it && served < (int)srv_dynamic_maxconn(srv) && __ha_cpu_relax());
		}
		if (!got_it)
			return -1; /* HAProxy would queue it */
	} else
		_HA_ATOMIC_INC(&srv->served);

	/* sess_change_server(s, srv) */
	_HA_ATOMIC_INC(&px->served);
	__ha_barrier_atomic_store();
	if (px->lbprm.ops && px->lbprm.ops->server_take_conn)
		px->lbprm.ops->server_take_conn(srv);

	h->slot[slot] = srv;
	return srv->puid - 1;
}

static void lb_hap_release(void *ctx, uint32_t slot)
{
	lb_hap *h = ctx;
	struct server *srv = h->slot[slot];

	/* sess_change_server(s, NULL) */
	_HA_ATOMIC_DEC(&srv->proxy->served);
	_HA_ATOMIC_DEC(&srv->served);
	__ha_barrier_atomic_store();
	if (srv->proxy->lbprm.ops && srv->proxy->lbprm.ops->server_drop_conn)
		srv->proxy->lbprm.ops->server_drop_conn(srv);
}

static int lb_hap_check(void *ctx, char *why, size_t len)
{
	lb_hap *h = ctx;
	if (h->px->served != 0) {
		snprintf(why, len, "the backend serves %d after every release", h->px->served);
		return 1;
	}
	for (uint32_t i = 0; i < h->n; i++)
		if (h->by_idx[i]->served != 0) {
			snprintf(why, len, "server %u serves %d after every release", i, h->by_idx[i]->served);
			return 1;
		}
	return 0;
}

static int lb_hap_setup(const char *algo, uint32_t n, uint32_t slots, lb_impl *out)
{
	lb_hap *h = &lb_h;
	if (lb_hap_algo < 0 || strncmp(algo, lb_hap_algos[lb_hap_algo].algo, strlen(lb_hap_algos[lb_hap_algo].algo)))
		return -1;
	h->px = proxy_be_by_name("b");
	if (!h->px) {
		fprintf(stderr, "haproxy: no backend \"b\"\n");
		return -1;
	}
	h->kind = lb_hap_algos[lb_hap_algo].kind;
	h->n = n;
	h->strm.be = h->px;
	h->by_idx = calloc(n, sizeof(*h->by_idx));
	h->slot = calloc(slots, sizeof(*h->slot));
	if (!h->by_idx || !h->slot)
		return -1;
	uint32_t count = 0;
	for (struct server *s = h->px->srv; s; s = s->next, count++) {
		if (s->puid < 1 || (uint32_t)s->puid > n || h->by_idx[s->puid - 1]) {
			fprintf(stderr, "haproxy: server ids are not 1..%u\n", n);
			return -1;
		}
		h->by_idx[s->puid - 1] = s;
	}
	if (count != n) {
		fprintf(stderr, "haproxy: %u servers, wanted %u\n", count, n);
		return -1;
	}
	if (!h->px->lbprm.tot_weight) {
		fprintf(stderr, "haproxy: no usable server\n");
		return -1;
	}
	for (uint32_t i = 0; i < n; i++)
		if (h->by_idx[i]->maxconn != lb_hap_maxconn || !srv_currently_usable(h->by_idx[i])) {
			fprintf(stderr, "haproxy: server %u has maxconn %u (wanted %u) or isn't up\n", i + 1, h->by_idx[i]->maxconn, lb_hap_maxconn);
			return -1;
		}
	if (h->kind == LB_HAP_RANDOM && h->px->lbprm.arg_opt1 != 2) {
		fprintf(stderr, "haproxy: random draws %d, not 2\n", h->px->lbprm.arg_opt1);
		return -1;
	}
	out->impl = "haproxy";
	out->algo = algo;
	out->commit = LB_HAPROXY_COMMIT;
	out->ctx = h;
	out->release = lb_hap_release;
	out->pick = lb_hap_pick;
	out->check = lb_hap_check;
	return 0;
}

/* Thread 1's per-thread init: HAProxy is up; run the bench in its thread, then leave. */
static int lb_hap_bench(void)
{
	if (tid != 0)
		return 1;
	int rc = lb_main(lb_hap_argc, lb_hap_argv, lb_hap_setup);
	fflush(stdout);
	fflush(stderr);
	_exit(rc);
	return 0;
}
REGISTER_PER_THREAD_INIT(lb_hap_bench);

int main(int argc, char **argv)
{
	if (argc < 2) {
		fprintf(stderr, "usage: %s ALGO --n N --rho R ... (see common.h)\n", argv[0]);
		return 2;
	}
	lb_hap_argc = argc;
	lb_hap_argv = argv;
	char base_algo[64];
	const char *slash = strchr(argv[1], '/');
	snprintf(base_algo, sizeof(base_algo), "%.*s", slash ? (int)(slash - argv[1]) : (int)strlen(argv[1]), argv[1]);
	if (slash && (sscanf(slash, "/maxconn=%u", &lb_hap_maxconn) != 1 || !lb_hap_maxconn)) {
		fprintf(stderr, "haproxy: %s: the only variant is /maxconn=K\n", argv[1]);
		return 2;
	}
	for (size_t i = 0; i < sizeof(lb_hap_algos) / sizeof(lb_hap_algos[0]); i++)
		if (!strcmp(base_algo, lb_hap_algos[i].algo))
			lb_hap_algo = (int)i;
	if (lb_hap_algo < 0 && strcmp(argv[1], "null")) {
		fprintf(stderr, "haproxy: unknown algorithm %s (have:", argv[1]);
		for (size_t i = 0; i < sizeof(lb_hap_algos) / sizeof(lb_hap_algos[0]); i++)
			fprintf(stderr, " %s", lb_hap_algos[i].algo);
		fprintf(stderr, ")\n");
		return 2;
	}
	uint32_t n = 0;
	for (int i = 2; i + 1 < argc; i++)
		if (!strcmp(argv[i], "--n"))
			n = (uint32_t)strtoul(argv[i + 1], NULL, 10);
	if (!n) {
		fprintf(stderr, "--n is required\n");
		return 2;
	}
	if (lb_hap_algo < 0)
		lb_hap_algo = 0; /* the null: any configuration starts HAProxy */

	const char *home = getenv("HOME");
	char dir[1024], cfg[1100], sock[1100];
	if (!home)
		return 1;
	snprintf(dir, sizeof(dir), "%s/.cache/kiln-lb/bench/build/haproxy-run", home);
	mkdir(dir, 0755);
	snprintf(dir, sizeof(dir), "%s/.cache/kiln-lb/bench/build/haproxy-run/%s-%u-%u", home, lb_hap_algos[lb_hap_algo].algo, lb_hap_maxconn, n);
	if (mkdir(dir, 0755) && errno != EEXIST)
		return 1;
	snprintf(cfg, sizeof(cfg), "%s/haproxy.cfg", dir);
	snprintf(sock, sizeof(sock), "%s/f.sock", dir);
	FILE *f = fopen(cfg, "w");
	if (!f)
		return 1;
	fprintf(f,
	        "global\n    nbthread 1\n\n"
	        "defaults\n    mode tcp\n    timeout connect 5s\n    timeout client 50s\n    timeout server 50s\n\n"
	        "frontend f\n    bind unix@%s\n    default_backend b\n\n"
	        "backend b\n    balance %s\n",
	        sock, lb_hap_algos[lb_hap_algo].balance);
	char server_kw[32] = "";
	if (lb_hap_maxconn)
		snprintf(server_kw, sizeof(server_kw), " maxconn %u", lb_hap_maxconn);
	for (uint32_t i = 0; i < n; i++)
		fprintf(f, "    server s%u 10.%u.%u.%u:80%s\n", i + 1, (i >> 16) & 255u, (i >> 8) & 255u, i & 255u, server_kw);
	if (fclose(f))
		return 1;

	char *hargv[] = {"haproxy", "-f", cfg, NULL};
	return lb_haproxy_main(3, hargv);
}
