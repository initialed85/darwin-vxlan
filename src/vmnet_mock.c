/*
 * vmnet_mock.c — test stub that satisfies all vmnet_ctx_* symbols without
 * vmnet.framework.  Uses real pipes so the Rust poll/read/write logic runs
 * unchanged.
 *
 * Behaviour:
 *   - vmnet_ctx_start pre-fills the notify pipe so the blocking thread
 *     wakes immediately on the first poll, driving the frame-read path.
 *   - vmnet_ctx_read_one returns exactly one synthetic 14-byte Ethernet
 *     frame on the first call, then returns 0 (no more frames).
 *   - vmnet_ctx_write returns 0 (success) by default, or -1 when
 *     vmnet_mock_set_write_fail(1) has been called.
 *   - vmnet_ctx_start returns NULL when vmnet_mock_set_start_fail(1) has
 *     been called.
 *
 * Enabled via the `vmnet-mock` Cargo feature (build.rs selects this file
 * instead of vmnet_bridge.c and skips -framework vmnet).
 */

#include <unistd.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

typedef struct {
    int      notify_pipe[2];    /* [0] = read (polled by Rust), [1] = write */
    int      shutdown_pipe[2];  /* [0] = read (polled), [1] = write (signalled) */
    uint32_t mtu;
    size_t   max_packet_size;
    int      frames_sent;       /* how many fake frames have been returned */
} mock_ctx_t;

/*
 * Fault-injection flags.
 *
 * mock_start_fail is _Thread_local so that one test can make
 * vmnet_ctx_start() return NULL without affecting concurrent tests running in
 * other OS threads (which is the normal cargo-test parallel mode).
 *
 * mock_write_fail is also _Thread_local for the same reason.
 */
static _Thread_local int mock_start_fail = 0;
static _Thread_local int mock_write_fail = 0;

void vmnet_mock_set_start_fail(int v) { mock_start_fail = v; }
void vmnet_mock_set_write_fail(int v) { mock_write_fail = v; }

mock_ctx_t *vmnet_ctx_start(uint32_t requested_mtu) {
    if (mock_start_fail) return NULL;

    mock_ctx_t *ctx = calloc(1, sizeof(mock_ctx_t));
    if (!ctx) return NULL;

    if (pipe(ctx->notify_pipe) < 0) { free(ctx); return NULL; }
    if (pipe(ctx->shutdown_pipe) < 0) {
        close(ctx->notify_pipe[0]); close(ctx->notify_pipe[1]);
        free(ctx); return NULL;
    }

    fcntl(ctx->notify_pipe[0],   F_SETFL, O_NONBLOCK);
    fcntl(ctx->notify_pipe[1],   F_SETFL, O_NONBLOCK);
    fcntl(ctx->shutdown_pipe[1], F_SETFL, O_NONBLOCK);

    ctx->mtu             = requested_mtu > 0 ? requested_mtu : 1500;
    ctx->max_packet_size = ctx->mtu + 18; /* Ethernet overhead */
    ctx->frames_sent     = 0;

    /*
     * Pre-fill the notify pipe with one byte so that the Rust blocking
     * thread wakes on the very first poll() call.  This lets tests
     * exercise the drain-and-read path without needing real vmnet traffic.
     */
    uint8_t one = 1;
    write(ctx->notify_pipe[1], &one, 1);

    return (void *)ctx;
}

int      vmnet_ctx_notify_fd(mock_ctx_t *ctx)         { return ctx->notify_pipe[0]; }
int      vmnet_ctx_shutdown_write_fd(mock_ctx_t *ctx) { return ctx->shutdown_pipe[1]; }
int      vmnet_ctx_shutdown_read_fd(mock_ctx_t *ctx)  { return ctx->shutdown_pipe[0]; }
uint32_t vmnet_ctx_mtu(mock_ctx_t *ctx)               { return ctx->mtu; }
size_t   vmnet_ctx_max_packet_size(mock_ctx_t *ctx)   { return ctx->max_packet_size; }

int vmnet_ctx_read_one(mock_ctx_t *ctx, uint8_t *buf, size_t buf_len) {
    if (ctx->frames_sent == 0 && buf_len >= 14) {
        ctx->frames_sent = 1;
        /* Minimal valid Ethernet frame: 6-byte dst + 6-byte src + 2-byte ethertype */
        memset(buf, 0xAA, 14);
        return 14;
    }
    return 0; /* no more frames available */
}

int vmnet_ctx_write(mock_ctx_t *ctx, const uint8_t *buf, size_t len) {
    (void)ctx; (void)buf; (void)len;
    return mock_write_fail ? -1 : 0;
}

void vmnet_ctx_stop(mock_ctx_t *ctx) {
    if (!ctx) return;
    close(ctx->notify_pipe[0]);
    close(ctx->notify_pipe[1]);
    close(ctx->shutdown_pipe[0]);
    close(ctx->shutdown_pipe[1]);
    free(ctx);
}
