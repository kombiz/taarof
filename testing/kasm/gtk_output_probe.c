/* Disposable-only VTE delivery probe. No terminal bytes are persisted: counters,
 * SHA-256, GTK ticks and a final-marker boolean only. The Python fixture pauses
 * at a quiescent READY boundary before releasing its inert producer. */
#define _GNU_SOURCE
#include <vte/vte.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/stat.h>
#include <sys/ioctl.h>

typedef struct {
    GWeakRef terminal;
    int fd;
    struct stat identity;
    guint64 bytes;
    GChecksum *digest;
    gboolean paused;
    gboolean fail_next_read;
    guint injected_errors;
    guint eof;
    gboolean sentinel;
} Slot;
static Slot slots[32];
static guint count;
static guint last_action_index;
static GMutex lock;
static guint64 ticks;
static const char *directory;
static void (*real_set_pty)(VteTerminal *, VtePty *);
static ssize_t (*real_read)(int, void *, size_t);
static void eof_cb(VteTerminal *terminal, gpointer data) {
    guint index = GPOINTER_TO_UINT(data);
    char *text = vte_terminal_get_text(terminal, NULL, NULL, NULL);
    g_mutex_lock(&lock);
    slots[index].eof++;
    slots[index].fd = -1;
    slots[index].sentinel = text && strstr(text, "FINAL_SENTINEL");
    g_mutex_unlock(&lock);
    g_free(text);
}
void vte_terminal_set_pty(VteTerminal *terminal, VtePty *pty) {
    if (!real_set_pty) real_set_pty = dlsym(RTLD_NEXT, "vte_terminal_set_pty");
    if (directory && pty) {
        g_mutex_lock(&lock);
        guint i;
        for (i = 0; i < count; i++) {
            VteTerminal *known = g_weak_ref_get(&slots[i].terminal);
            gboolean same = known == terminal;
            if (known) g_object_unref(known);
            if (same) break;
        }
        if (i == count && count < G_N_ELEMENTS(slots)) {
            count++;
            g_weak_ref_init(&slots[i].terminal, terminal);
            slots[i].digest = g_checksum_new(G_CHECKSUM_SHA256);
            g_signal_connect(terminal, "eof", G_CALLBACK(eof_cb), GUINT_TO_POINTER(i));
        }
        if (i < count) { slots[i].fd = vte_pty_get_fd(pty); fstat(slots[i].fd, &slots[i].identity); }
        g_mutex_unlock(&lock);
    }
    real_set_pty(terminal, pty);
}
ssize_t read(int fd, void *buffer, size_t length) {
    if (!real_read) real_read = dlsym(RTLD_NEXT, "read");
    struct stat identity;
    gboolean identified = fstat(fd, &identity) == 0;
    if (directory) {
        gboolean paused = FALSE, failed = FALSE;
        g_mutex_lock(&lock);
        for (guint i=0; i<count; i++) {
            if (slots[i].fd != fd || !identified || slots[i].identity.st_dev != identity.st_dev || slots[i].identity.st_ino != identity.st_ino || slots[i].identity.st_rdev != identity.st_rdev) continue;
            paused = slots[i].paused;
            if (slots[i].fail_next_read) {
                slots[i].fail_next_read = FALSE;
                slots[i].injected_errors++;
                failed = TRUE;
            }
        }
        g_mutex_unlock(&lock);
        if (failed) { errno = EIO; return -1; }
        if (paused) { errno = EAGAIN; return -1; }
    }
    ssize_t result = real_read(fd, buffer, length);
    if (directory && result > 0) {
        g_mutex_lock(&lock);
        for (guint i = 0; i < count; i++) {
            if (slots[i].fd != fd || !identified || slots[i].identity.st_dev != identity.st_dev || slots[i].identity.st_ino != identity.st_ino || slots[i].identity.st_rdev != identity.st_rdev) continue;
            int packet = 0;
            ioctl(fd, TIOCGPKT, &packet);
            size_t offset = packet ? 1 : 0;
            if (packet && ((unsigned char *)buffer)[0] != TIOCPKT_DATA) continue;
            slots[i].bytes += result - offset;
            g_checksum_update(slots[i].digest, (unsigned char *)buffer + offset, result - offset);
        }
        g_mutex_unlock(&lock);
    }
    return result;
}
static gboolean tick(gpointer unused) {
    (void)unused;
    ticks++;
    char filename[4096];
    snprintf(filename, sizeof filename, "%s/command", directory);
    char *command = NULL;
    if (g_file_get_contents(filename, &command, NULL, NULL)) {
        unlink(filename);
        char action[16]; unsigned index;
        int parsed = sscanf(command, "%15s %u", action, &index);
        if (parsed == 2 && index < count) {
            gboolean fail_ready = !strcmp(action, "fail-ready");
            if (!strcmp(action, "pause-ready") || fail_ready) {
                for (index = 0; index < count; index++) {
                    VteTerminal *candidate = g_weak_ref_get(&slots[index].terminal);
                    if (!candidate) continue;
                    char *text = vte_terminal_get_text(candidate, NULL, NULL, NULL);
                    gboolean ready = text && strstr(text, "OUTPUT_READY") && vte_terminal_get_pty(candidate) && gtk_widget_get_mapped(GTK_WIDGET(candidate));
                    g_free(text);
                    g_object_unref(candidate);
                    if (ready) break;
                }
                strcpy(action, fail_ready ? "fail" : "pause");
            }
            if (index >= count) { g_free(command); return G_SOURCE_CONTINUE; }
            last_action_index = index;
            Slot *slot = &slots[index];
            VteTerminal *terminal = g_weak_ref_get(&slot->terminal);
            if (terminal) {
                if (!strcmp(action, "pause") && !slot->paused) {
                    VtePty *pty = vte_terminal_get_pty(terminal);
                    if (pty) {
                        g_mutex_lock(&lock);
                        slot->paused = TRUE;
                        g_mutex_unlock(&lock);
                    }
                } else if (!strcmp(action, "resume") && slot->paused) {
                    g_mutex_lock(&lock);
                    slot->paused = FALSE;
                    g_mutex_unlock(&lock);
                } else if (!strcmp(action, "fail")) {
                    g_mutex_lock(&lock);
                    slot->fail_next_read = TRUE;
                    g_mutex_unlock(&lock);
                } else if (!strcmp(action, "reset")) {
                    g_mutex_lock(&lock);
                    slot->bytes = 0;
                    g_checksum_reset(slot->digest);
                    slot->sentinel = FALSE;
                    slot->eof = 0;
                    g_mutex_unlock(&lock);
                }
                g_object_unref(terminal);
            }
        }
        g_free(command);
    }
    GString *json = g_string_new(NULL);
    g_string_append_printf(json, "{\"ticks\":%lu,\"last_action_index\":%u,\"terminals\":[", (unsigned long)ticks, last_action_index);
    g_mutex_lock(&lock);
    for (guint i = 0; i < count; i++) {
        GChecksum *copy = g_checksum_copy(slots[i].digest);
        VteTerminal *terminal = g_weak_ref_get(&slots[i].terminal);
        gboolean has_pty = terminal && vte_terminal_get_pty(terminal);
        gboolean mapped = terminal && gtk_widget_get_mapped(GTK_WIDGET(terminal));
        char *contents = terminal ? vte_terminal_get_text(terminal, NULL, NULL, NULL) : NULL;
        gboolean ready = contents && strstr(contents, "OUTPUT_READY");
        g_free(contents);
        if (terminal) g_object_unref(terminal);
        g_string_append_printf(json,
            "%s{\"index\":%u,\"bytes\":%lu,\"sha256\":\"%s\",\"paused\":%s,\"eof\":%u,\"sentinel_at_eof\":%s,\"has_pty\":%s,\"ready\":%s,\"injected_errors\":%u,\"mapped\":%s}",
            i ? "," : "", i, (unsigned long)slots[i].bytes,
            g_checksum_get_string(copy), slots[i].paused ? "true" : "false",
            slots[i].eof, slots[i].sentinel ? "true" : "false", has_pty ? "true" : "false", ready ? "true" : "false", slots[i].injected_errors, mapped ? "true" : "false");
        g_checksum_free(copy);
    }
    g_mutex_unlock(&lock);
    g_string_append(json, "]}\n");
    snprintf(filename, sizeof filename, "%s/state.json", directory);
    g_file_set_contents(filename, json->str, json->len, NULL);
    g_string_free(json, TRUE);
    return G_SOURCE_CONTINUE;
}
__attribute__((constructor)) static void install_probe(void) {
    char executable[4096];
    ssize_t length = readlink("/proc/self/exe", executable, sizeof executable - 1);
    if (length < 0) return;
    executable[length] = 0;
    const char *base = strrchr(executable, '/');
    if (!base || strcmp(base + 1, "taarof-app")) return;
    directory = getenv("TAAROF_TEST_OUTPUT_DIR");
    if (directory) g_timeout_add(10, tick, NULL);
}
