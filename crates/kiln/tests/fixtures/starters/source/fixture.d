module fixture;

int add(int a, int b) { return a + b; }

unittest
{
    assert(add(1, 2) == 3);
}
