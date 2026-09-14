/* Test-only GLib heartbeat loaded into the disposable app, never its panes.
 * cc -shared -fPIC $(pkg-config --cflags glib-2.0) ... $(pkg-config --libs glib-2.0)
 */
#include <glib.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static int tick_fd = -1;
static uint64_t ticks;
static gboolean tick(gpointer unused) {
    (void)unused;
    ++ticks;
    if (pwrite(tick_fd, &ticks, sizeof ticks, 0) != sizeof ticks)
        return G_SOURCE_REMOVE;
    return G_SOURCE_CONTINUE;
}
__attribute__((constructor)) static void install_probe(void) {
    char executable[4096];
    ssize_t length = readlink("/proc/self/exe", executable, sizeof executable - 1);
    if (length < 0) return;
    executable[length] = 0;
    const char *base = strrchr(executable, '/');
    if (!base || strcmp(base + 1, "taarof-app")) return;
    const char *filename = getenv("TAAROF_TEST_TICK_FILE");
    if (!filename) return;
    tick_fd = open(filename, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (tick_fd >= 0) g_timeout_add(10, tick, NULL);
}
