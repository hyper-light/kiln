/*
 * ci/bench/drivers/null.c: the null implementation alone (common.h's no-op
 * release and pick): the harness's own cost, and the calibration of B.
 */
#include "../common.h"

static int lb_null_setup(const char *algo, uint32_t n, uint32_t slots, lb_impl *out)
{
	(void)n;
	(void)slots;
	(void)out;
	fprintf(stderr, "null: the only algorithm is \"null\", not %s\n", algo);
	return -1;
}

int main(int argc, char **argv)
{
	return lb_main(argc, argv, lb_null_setup);
}
