/* Spike C — host libpipewire-module-filter-chain inside our own process with
 * pw_context_load_module(), the same call the Rust daemon would make through
 * pipewire-sys FFI. Loads the module, then reloads it (destroy + load) a few
 * times, printing CLOCK_REALTIME timestamps so the shell can line them up with
 * recordings.
 *
 * usage: inproc <filter-chain-args.spa-json> <reloads> <interval-s>
 * build: cc -O2 -o inproc inproc.c $(pkg-config --cflags --libs libpipewire-0.3)
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <pipewire/pipewire.h>
#include <pipewire/impl-module.h>

static struct pw_main_loop *ml;
static struct pw_context *ctx;
static struct pw_impl_module *mod;
static char *args;
static int reloads_left;

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_REALTIME, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void load(void)
{
	double t0 = now();
	mod = pw_context_load_module(ctx, "libpipewire-module-filter-chain", args, NULL);
	printf("load %.6f took %.3f ms %s\n", t0, (now() - t0) * 1e3, mod ? "ok" : "FAILED");
	fflush(stdout);
}

static void on_timer(void *data, uint64_t expirations)
{
	if (reloads_left-- <= 0) {
		pw_main_loop_quit(ml);
		return;
	}
	double t0 = now();
	pw_impl_module_destroy(mod);
	printf("destroy %.6f took %.3f ms\n", t0, (now() - t0) * 1e3);
	load();
}

int main(int argc, char *argv[])
{
	if (argc < 4) {
		fprintf(stderr, "usage: %s <args.spa-json> <reloads> <interval-s>\n", argv[0]);
		return 1;
	}
	FILE *f = fopen(argv[1], "r");
	if (!f) { perror(argv[1]); return 1; }
	fseek(f, 0, SEEK_END);
	long n = ftell(f);
	rewind(f);
	args = calloc(1, n + 1);
	if (fread(args, 1, n, f) != (size_t)n) { perror("read"); return 1; }
	fclose(f);
	reloads_left = atoi(argv[2]);
	double interval = atof(argv[3]);

	pw_init(&argc, &argv);
	ml = pw_main_loop_new(NULL);
	/* default config = client.conf, which loads libpipewire-module-rt */
	ctx = pw_context_new(pw_main_loop_get_loop(ml), NULL, 0);
	if (pw_context_connect(ctx, NULL, 0) == NULL) { fprintf(stderr, "connect failed\n"); return 1; }

	load();

	struct spa_source *t = pw_loop_add_timer(pw_main_loop_get_loop(ml), on_timer, NULL);
	struct timespec v = { (time_t)interval, (long)((interval - (time_t)interval) * 1e9) };
	pw_loop_update_timer(pw_main_loop_get_loop(ml), t, &v, &v, false);

	pw_main_loop_run(ml);
	if (mod)
		pw_impl_module_destroy(mod);
	pw_context_destroy(ctx);
	pw_main_loop_destroy(ml);
	pw_deinit();
	return 0;
}
