// Envoy's own selection kernels under the common trace (ci/bench/drivers/common.h), built in
// Envoy's tree at the pinned release with bazel -c opt (ci/bench/build-envoy.sh copies this
// package to test/extensions/load_balancing_policies/kiln_bench).
//
// Envoy's stock benches (least_request_lb_benchmark.cc and its siblings) time chooseHost with a
// node_hash_map insert of the host's address string in the loop and every host at rq_active 0;
// this one keeps Envoy's own setup (BaseTester: N hosts of weight 1, one locality, the load
// balancer constructed as the stock bench constructs it) and changes only what research-bench.md
// asks: no map in the timed loop, an N sweep, and real rq_active from the common trace. What is
// timed is chooseHost and the rq_active increment the connection pool makes on assignment
// (conn_pool_base.cc: host_->stats().rq_active_.inc()); a request's end is the decrement.
//
// Algorithms: least_request (P2C, choice_count 2, Envoy's default), least_request_full_scan
// (selection_method FULL_SCAN), round_robin (EDF when weighted, a plain index here) and random.

#include <cstdio>
#include <memory>
#include <string>

#include "source/extensions/load_balancing_policies/least_request/least_request_lb.h"
#include "source/extensions/load_balancing_policies/random/random_lb.h"
#include "source/extensions/load_balancing_policies/round_robin/round_robin_lb.h"

#include "test/extensions/load_balancing_policies/common/benchmark_base_tester.h"

#include "absl/container/flat_hash_map.h"

extern "C" {
#include "test/extensions/load_balancing_policies/kiln_bench/common.h"
}

namespace Envoy {
namespace Upstream {
namespace {

class Tester : public BaseTester {
public:
  explicit Tester(uint64_t n) : BaseTester(n) {}
};

int run(const kb_args& a) {
  Tester t(a.n);
  std::unique_ptr<LoadBalancer> lb;
  const std::string algo = a.algo;
  if (algo == "least_request" || algo == "least_request_full_scan") {
    envoy::extensions::load_balancing_policies::least_request::v3::LeastRequest config;
    config.mutable_choice_count()->set_value(2);
    if (algo == "least_request_full_scan") {
      config.set_selection_method(
          envoy::extensions::load_balancing_policies::least_request::v3::LeastRequest::FULL_SCAN);
    }
    lb = std::make_unique<LeastRequestLoadBalancer>(t.priority_set_, &t.local_priority_set_,
                                                    t.stats_, t.runtime_, t.random_, 50, config,
                                                    t.simTime());
  } else if (algo == "round_robin") {
    envoy::extensions::load_balancing_policies::round_robin::v3::RoundRobin config;
    lb = std::make_unique<RoundRobinLoadBalancer>(t.priority_set_, &t.local_priority_set_,
                                                  t.stats_, t.runtime_, t.random_, 50, config,
                                                  t.simTime());
  } else if (algo == "random") {
    envoy::extensions::load_balancing_policies::random::v3::Random config;
    lb = std::make_unique<RandomLoadBalancer>(t.priority_set_, &t.local_priority_set_, t.stats_,
                                              t.runtime_, t.random_, 50, config);
  } else {
    std::fprintf(stderr, "envoy_pick: unknown algo %s\n", a.algo);
    return 2;
  }

  // Each host's index, for the quality bookkeeping (untimed: POST).
  absl::flat_hash_map<const HostDescription*, int> index;
  const auto& hosts = t.priority_set_.hostSetsPerPriority()[0]->hosts();
  for (size_t i = 0; i < hosts.size(); i++) {
    index[hosts[i].get()] = static_cast<int>(i);
  }
  TestLoadBalancerContext context;
  LoadBalancer* l = lb.get();

#define PREP (void)0
#define PICK                                                                                     \
  do {                                                                                           \
    HostConstSharedPtr h = l->chooseHost(&context).host;                                         \
    if (h != nullptr) {                                                                          \
      h->stats().rq_active_.inc();                                                               \
      kb_h = const_cast<Host*>(h.get());                                                         \
    }                                                                                            \
  } while (0)
#define POST                                                                                     \
  do {                                                                                           \
    if (kb_h != nullptr) {                                                                       \
      auto it = index.find(static_cast<const HostDescription*>(static_cast<Host*>(kb_h)));       \
      kb_b = it == index.end() ? -1 : it->second;                                                \
    }                                                                                            \
  } while (0)
#define FREE static_cast<Host*>(kb_r->handle)->stats().rq_active_.dec()

  KB_RUN2("envoy-1.40.2", &a, "", PREP, PICK, POST, FREE);
  return 0;
}

} // namespace
} // namespace Upstream
} // namespace Envoy

int main(int argc, char** argv) {
  kb_args a;
  if (kb_parse(argc, argv, &a) != 0) {
    return 2;
  }
  return Envoy::Upstream::run(a);
}
