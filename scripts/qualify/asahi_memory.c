/* Qualification-only Asahi GEM allocation high-water mark, including staging.
 * Build on Asahi: gcc -shared -fPIC -O2 -o asahi-memory.so asahi_memory.c -ldl -pthread
 * Use via measure_h2o.py --memory-trace /absolute/path/asahi-memory.so.
 * Driver allocations are outside process RSS; add the two peaks conservatively.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <drm/asahi_drm.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct { int fd; uint32_t handle; uint64_t bytes; } buffers[16384];
static uint64_t current, peak;
static int overflow;
static int (*real_ioctl)(int, unsigned long, ...);
static pthread_once_t init_once = PTHREAD_ONCE_INIT;

static void init(void) { real_ioctl = dlsym(RTLD_NEXT, "ioctl"); }

int ioctl(int fd, unsigned long request, ...) {
    va_list ap;
    va_start(ap, request);
    void *arg = va_arg(ap, void *);
    va_end(ap);
    pthread_once(&init_once, init);
    int result = real_ioctl(fd, request, arg);
    if (result != 0) return result;
    if (request != DRM_IOCTL_ASAHI_GEM_CREATE && request != DRM_IOCTL_GEM_CLOSE) return result;
    pthread_mutex_lock(&lock);
    if (request == DRM_IOCTL_ASAHI_GEM_CREATE) {
        struct drm_asahi_gem_create *gem = arg;
        unsigned i;
        for (i = 0; i < sizeof(buffers) / sizeof(buffers[0]); ++i) {
            if (!buffers[i].bytes) {
                buffers[i].fd = fd;
                buffers[i].handle = gem->handle;
                buffers[i].bytes = gem->size;
                current += gem->size;
                if (current > peak) peak = current;
                break;
            }
        }
        if (i == sizeof(buffers) / sizeof(buffers[0])) overflow = 1;
    } else {
        struct drm_gem_close *gem = arg;
        for (unsigned i = 0; i < sizeof(buffers) / sizeof(buffers[0]); ++i) {
            if (buffers[i].bytes && buffers[i].fd == fd && buffers[i].handle == gem->handle) {
                current -= buffers[i].bytes;
                buffers[i].bytes = 0;
                break;
            }
        }
    }
    pthread_mutex_unlock(&lock);
    return result;
}

__attribute__((destructor)) static void report(void) {
    fprintf(stderr, "asahi_gem_peak_bytes=%llu asahi_gem_tracking_overflow=%d\n",
            (unsigned long long)peak, overflow);
}
