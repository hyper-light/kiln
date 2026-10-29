# Stack traces as Ruby's tracer reports them (dd-trace-rb's `error.stack`, its
# Datadog::Core::Error.full_backtrace: the first line `file:line:in 'method': message (Class)`, the
# rest each `\tfrom file:line:in 'method'`, then each cause's alike), for kiln's error grouping tests.
class Cart
  def initialize(items) = @items = items

  def total
    @items.each { |i| raise ArgumentError, "bad item #{i} in cart 'c-#{i * 7}'" if i < 0 }
    @items.sum
  end
end

def checkout(order) = Cart.new([1, 2, -order]).total
def parse(s) = Integer(s)

def load(id)
  parse("x#{id}")
rescue ArgumentError
  raise RuntimeError, "order #{id} could not load"
end

def backtrace_for(ex, out)
  trace = ex.backtrace or return
  out << "\n" unless out.empty?
  out << "#{trace[0]}: #{ex.message} (#{ex.class})"
  trace[1..].each { |line| out << "\n\tfrom #{line}" }
end

def full_backtrace(ex)
  out = +""
  seen = {}
  while ex && !seen.key?(ex)
    backtrace_for(ex, out)
    seen[ex] = true
    ex = ex.cause
  end
  out
end

def run(name)
  yield
rescue => e
  puts "---- #{name}"
  puts full_backtrace(e)
end

run("app") { checkout(42) }
run("app-again") { checkout(7) }
run("core") { parse("x1") }
run("chained") { load(99) }
run("block") { [1, 0].map { |v| 10 / v } }
