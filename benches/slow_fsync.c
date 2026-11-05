// Slow storage for `benches/echo.rs`, loaded into the core with DYLD_INSERT_LIBRARIES.
//
// Each fsync, and each fcntl(F_FULLFSYNC), which is how Rust's `sync_all` syncs on macOS,
// appends a byte to PINFOLD_SLOW_FSYNC_LOG and then waits PINFOLD_SLOW_FSYNC_MS before it
// syncs. The wait is a fixed sleep, so a run repeats on any Mac.

#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static void slow(void) {
    const char *log = getenv("PINFOLD_SLOW_FSYNC_LOG");
    const char *ms = getenv("PINFOLD_SLOW_FSYNC_MS");
    if (log) {
        int fd = open(log, O_WRONLY | O_APPEND | O_CREAT, 0600);
        if (fd >= 0) {
            write(fd, ".", 1);
            close(fd);
        }
    }
    if (ms) {
        long wait = atol(ms);
        struct timespec delay = {wait / 1000, (wait % 1000) * 1000000};
        nanosleep(&delay, NULL);
    }
}

static int slow_fsync(int fd) {
    slow();
    return fsync(fd);
}

// Every fcntl argument fits a pointer-sized slot, so it passes through unchanged.
static int slow_fcntl(int fd, int cmd, ...) {
    va_list args;
    va_start(args, cmd);
    void *arg = va_arg(args, void *);
    va_end(args);
    if (cmd == F_FULLFSYNC) {
        slow();
    }
    return fcntl(fd, cmd, arg);
}

__attribute__((used, section("__DATA,__interpose"))) static const struct {
    const void *replacement;
    const void *original;
} interposers[] = {
    {(const void *)slow_fsync, (const void *)fsync},
    {(const void *)slow_fcntl, (const void *)fcntl},
};
