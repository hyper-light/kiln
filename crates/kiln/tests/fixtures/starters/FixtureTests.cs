using Xunit;

public class FixtureTests
{
    static int Add(int a, int b) => a + b;

    [Fact]
    public void Adds() => Assert.Equal(3, Add(1, 2));
}
