<?php
// Stack traces as PHP's tracer reports them (dd-trace-php's `error.stack`: the throwable's
// getTraceAsString()), and the throwable as a string (what a log of it holds), for kiln's error
// grouping tests.
namespace Shop;

class Cart {
    public function __construct(private array $items) {}
    public function total(): int {
        foreach ($this->items as $i) {
            if ($i < 0) throw new \InvalidArgumentException("bad item $i in cart 'c-" . ($i * 7) . "'");
        }
        return array_sum($this->items);
    }
}

function checkout(int $order): int { return (new Cart([1, 2, -$order]))->total(); }
function parse(string $s) { return json_decode($s, flags: JSON_THROW_ON_ERROR); }

function run(string $name, callable $f): void {
    try { $f(); } catch (\Throwable $e) {
        echo "---- $name\n", $e->getTraceAsString(), "\n";
        echo "---- $name-string\n", (string) $e, "\n";
    }
}

run("app", fn() => checkout(42));
run("app-again", fn() => checkout(7));
run("builtin", fn() => parse("{not json"));
run("closure", fn() => array_map(function ($v) { return intdiv(10, $v); }, [1, 0]));
