// ci/bench/drivers/envoy/envoy.cc: Envoy v1.40.2's load balancers under common.h's workload.
//
// Built in Envoy's tree with bazel -c opt (run.py copies this package to
// test/extensions/load_balancing_policies/kiln_bench). The hosts and the load balancers are set up
// as Envoy's own benches set them up (BaseTester: N hosts of weight 1 in one locality, one
// priority; each LB constructed as its *_lb_benchmark.cc constructs it; ring_hash and maglev
// initialize()d, then factory()->create(), the worker's LB). What changes from the stock benches is
// the timed loop, to bench-spec.md's step: no string-keyed hit map in it, every N, and real
// rq_active from the workload.
//
// A pick is what the worker does for a request: ClusterEntry::chooseHost's override check
// (crossPriorityHostMap() and HostUtility::selectOverrideHost, cluster_manager_impl.cc:2249-2263),
// then chooseHost(&context) on the LB, with a LoadBalancerContextBase as the router passes one (for
// ring_hash and maglev its computeHashKey() returns the request's key: the route's hash policy; the
// others get none, as a route without one), then the connection pool's take of a stream on the host:
// rq_active + 1 (conn_pool_base.cc:342). The router holds the HostConstSharedPtr for the request's
// life. A release is the stream's end: rq_active - 1 (conn_pool_base.cc:358), and the host let go.
// Left out: the cluster-wide counters beside rq_active (upstream_rq_active, the resource manager:
// in BaseTester they sit behind gmock's MockClusterInfo, whose call cost isn't Envoy's).
//
// Algorithms: round_robin, least_request (P2C: choice_count 2, Envoy's default),
// least_request_full_scan (selection_method FULL_SCAN), random, ring_hash, maglev (default tables).

#include <cstdio>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

#include "source/common/upstream/host_utility.h"
#include "source/extensions/load_balancing_policies/least_request/least_request_lb.h"
#include "source/extensions/load_balancing_policies/maglev/maglev_lb.h"
#include "source/extensions/load_balancing_policies/random/random_lb.h"
#include "source/extensions/load_balancing_policies/ring_hash/ring_hash_lb.h"
#include "source/extensions/load_balancing_policies/round_robin/round_robin_lb.h"

#include "test/extensions/load_balancing_policies/common/benchmark_base_tester.h"

#include "absl/container/flat_hash_map.h"

#include "test/extensions/load_balancing_policies/kiln_bench/common.h"

namespace Envoy {
namespace Upstream {
namespace {

struct Bench {
  explicit Bench(uint32_t n) : tester(n) {}
  BaseTester tester;
  std::unique_ptr<ThreadAwareLoadBalancer> thread_aware;
  LoadBalancerPtr lb;
  TestLoadBalancerContext context;
  bool hashing{false};
  HostUtility::HostStatusSet override_statuses;
  std::vector<HostConstSharedPtr> slots;
  absl::flat_hash_map<const Host*, int32_t> index;
  std::string algo;
};

int32_t pick(void* ctx, uint32_t slot, uint64_t key) {
  Bench* b = static_cast<Bench*>(ctx);
  if (b->hashing) {
    b->context.hash_key_ = key;
  }
  auto cross = b->tester.priority_set_.crossPriorityHostMap();
  auto over = HostUtility::selectOverrideHost(cross.get(), b->override_statuses, &b->context);
  HostConstSharedPtr h = std::move(over.host);
  if (h == nullptr) {
    if (over.strict) {
      return -1;
    }
    h = b->lb->chooseHost(&b->context).host;
    if (h == nullptr) {
      return -1;
    }
  }
  h->stats().rq_active_.inc();
  b->slots[slot] = std::move(h);
  return 0;
}

void release(void* ctx, uint32_t slot) {
  Bench* b = static_cast<Bench*>(ctx);
  b->slots[slot]->stats().rq_active_.dec();
  b->slots[slot].reset();
}

int32_t index(void* ctx, uint32_t slot) {
  Bench* b = static_cast<Bench*>(ctx);
  auto it = b->index.find(b->slots[slot].get());
  return it == b->index.end() ? -1 : it->second;
}

int check(void* ctx, char* why, size_t len) {
  Bench* b = static_cast<Bench*>(ctx);
  const auto& hosts = b->tester.priority_set_.hostSetsPerPriority()[0]->hosts();
  for (size_t i = 0; i < hosts.size(); i++) {
    if (hosts[i]->stats().rq_active_.value() != 0) {
      std::snprintf(why, len, "host %zu has rq_active %llu after every release", i,
                    static_cast<unsigned long long>(hosts[i]->stats().rq_active_.value()));
      return 1;
    }
  }
  return 0;
}

int setup(const char* algo, uint32_t n, uint32_t slots, lb_impl* out) {
  Bench* b = new Bench(n);
  BaseTester& t = b->tester;
  b->algo = algo;
  const std::string a = algo;
  if (a == "least_request" || a == "least_request_full_scan") {
    envoy::extensions::load_balancing_policies::least_request::v3::LeastRequest config;
    config.mutable_choice_count()->set_value(2);
    if (a == "least_request_full_scan") {
      config.set_selection_method(
          envoy::extensions::load_balancing_policies::least_request::v3::LeastRequest::FULL_SCAN);
    }
    b->lb = std::make_unique<LeastRequestLoadBalancer>(t.priority_set_, &t.local_priority_set_, t.stats_,
                                                       t.runtime_, t.random_, 50, config, t.simTime());
  } else if (a == "round_robin") {
    envoy::extensions::load_balancing_policies::round_robin::v3::RoundRobin config;
    b->lb = std::make_unique<RoundRobinLoadBalancer>(t.priority_set_, &t.local_priority_set_, t.stats_,
                                                     t.runtime_, t.random_, 50, config, t.simTime());
  } else if (a == "random") {
    envoy::extensions::load_balancing_policies::random::v3::Random config;
    b->lb = std::make_unique<RandomLoadBalancer>(t.priority_set_, &t.local_priority_set_, t.stats_,
                                                 t.runtime_, t.random_, 50, config);
  } else if (a == "ring_hash" || a == "maglev") {
    if (a == "ring_hash") {
      envoy::extensions::load_balancing_policies::ring_hash::v3::RingHash config;
      b->thread_aware = std::make_unique<RingHashLoadBalancer>(t.priority_set_, t.stats_, t.stats_scope_,
                                                               t.runtime_, t.random_, 50, config, nullptr);
    } else {
      envoy::extensions::load_balancing_policies::maglev::v3::Maglev config;
      b->thread_aware = std::make_unique<MaglevLoadBalancer>(t.priority_set_, t.stats_, t.stats_scope_,
                                                             t.runtime_, t.random_, 50, config, nullptr);
    }
    if (!b->thread_aware->initialize().ok()) {
      std::fprintf(stderr, "envoy: %s failed to initialize\n", algo);
      return -1;
    }
    b->lb = b->thread_aware->factory()->create(t.lb_params_);
    b->hashing = true;
  } else {
    std::fprintf(stderr, "envoy: unknown algorithm %s\n", algo);
    return -1;
  }
  b->override_statuses =
      HostUtility::createOverrideHostStatus(envoy::config::cluster::v3::Cluster::CommonLbConfig());
  const auto& hosts = t.priority_set_.hostSetsPerPriority()[0]->hosts();
  if (hosts.size() != n) {
    std::fprintf(stderr, "envoy: %zu hosts, wanted %u\n", hosts.size(), n);
    return -1;
  }
  for (size_t i = 0; i < hosts.size(); i++) {
    b->index[hosts[i].get()] = static_cast<int32_t>(i);
  }
  b->slots.resize(slots);
  out->impl = "envoy";
  out->algo = b->algo.c_str();
  out->commit = "b2d89e0ae53916f28770771fbdd49c5c99c5c5a2";
  out->ctx = b;
  out->release = release;
  out->pick = pick;
  out->check = check;
  out->index = index;
  return 0;
}

} // namespace
} // namespace Upstream
} // namespace Envoy

int main(int argc, char** argv) { return lb_main(argc, argv, Envoy::Upstream::setup); }
