// Stack traces as .NET's tracer reports them (dd-trace-dotnet's `error.stack`: the exception's
// ToString()), for kiln's error grouping tests.
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

namespace Shop
{
    class Cart
    {
        readonly List<int> items;
        public Cart(List<int> items) { this.items = items; }
        public int Total()
        {
            foreach (var i in items) if (i < 0) throw new InvalidOperationException($"bad item {i} in cart 'c-{i * 7}'");
            return items.Sum();
        }
    }

    static class Program
    {
        static int Checkout(int order) => new Cart(new List<int> { 1, 2, -order }).Total();
        static int Parse(string s) => int.Parse(s);
        static async Task<int> Fetch(int id) { await Task.Delay(1); throw new KeyNotFoundException($"order {id} not found"); }
        static void Load(int id)
        {
            try { Parse("x" + id); } catch (FormatException e) { throw new ApplicationException($"order {id} could not load", e); }
        }
        static void Run(string name, Func<object> f)
        {
            try { f(); } catch (Exception e) { Console.Write($"---- {name}\n{e}\n"); }
        }
        static void Main()
        {
            Run("app", () => Checkout(42));
            Run("app-again", () => Checkout(7));
            Run("bcl", () => Parse("x1"));
            Run("async", () => Fetch(5).GetAwaiter().GetResult());
            Run("chained", () => { Load(99); return null; });
            Run("lambda", () => new List<int> { 1, 0 }.Select(v => 10 / v).ToList());
        }
    }
}
