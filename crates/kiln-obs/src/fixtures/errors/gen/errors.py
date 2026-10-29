# Stack traces as Python's tracers report them (ddtrace's and OpenTelemetry's `error.stack` /
# `exception.stacktrace`: traceback.format_exc()), for kiln's error grouping tests
# (crates/kiln-obs/src/errors.rs). Each case printed between `---- name` lines.
import json, traceback


class Cart:
    def __init__(self, items):
        self.items = items

    def total(self):
        for item in self.items:
            if item < 0:
                raise ValueError(f"bad item {item} in cart 'c-{item * 7}'")
        return sum(self.items)


def checkout(order):
    return Cart([1, 2, -order]).total()


def parse(body):
    return json.loads(body)


def load(order_id):
    try:
        return {"id": order_id, "rows": [1 // 0]}
    except ZeroDivisionError as e:
        raise RuntimeError(f"order {order_id} could not load") from e


def case(name, f):
    try:
        f()
    except Exception:
        print(f"---- {name}")
        print(traceback.format_exc(), end="")


case("app", lambda: checkout(42))
case("app-again", lambda: checkout(7))
case("stdlib", lambda: parse("{not json"))
case("chained", lambda: load(99))
case("lambda", lambda: [x for x in map(lambda v: 10 // v, [1, 0])])
