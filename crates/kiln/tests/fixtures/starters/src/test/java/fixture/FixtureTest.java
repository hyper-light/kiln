package fixture;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class FixtureTest {
    @Test
    void adds() {
        assertEquals(3, Fixture.add(1, 2));
    }
}
