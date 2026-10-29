// Stack traces as Java's tracers report them (dd-trace-java's `error.stack`, OpenTelemetry's
// `exception.stacktrace`: printStackTrace's), for kiln's error grouping tests.
import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.List;
import java.util.concurrent.Callable;

public class Errors {
    static class Cart {
        final List<Integer> items;
        Cart(List<Integer> items) { this.items = items; }
        int total() {
            for (int i : items) if (i < 0) throw new IllegalStateException("bad item " + i + " in cart 'c-" + (i * 7) + "'");
            return 0;
        }
    }
    static int checkout(int order) { return new Cart(List.of(1, 2, -order)).total(); }
    static int parse(String s) { return Integer.parseInt(s); }
    static void load(int id) {
        try { parse("x" + id); } catch (NumberFormatException e) { throw new RuntimeException("order " + id + " could not load", e); }
    }
    static void run(String name, Callable<?> f) {
        try { f.call(); } catch (Exception e) {
            StringWriter w = new StringWriter();
            e.printStackTrace(new PrintWriter(w));
            System.out.print("---- " + name + "\n" + w);
        }
    }
    public static void main(String[] args) {
        run("app", () -> checkout(42));
        run("app-again", () -> checkout(7));
        run("jdk", () -> parse("x1"));
        run("chained", () -> { load(99); return null; });
        run("lambda", () -> List.of(1, 0).stream().map(v -> 10 / v).toList());
    }
}
