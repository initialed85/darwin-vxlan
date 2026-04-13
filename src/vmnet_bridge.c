/*
 * vmnet_bridge.c — thin C wrapper around vmnet.framework.
 *
 * vmnet uses Objective-C blocks and GCD (dispatch queues) which cannot be
 * called directly from Rust. This file exposes a plain C API that Rust FFI
 * can call without any block/XPC knowledge.
 *
 * Packet notification model:
 *   - A pipe is created at startup.
 *   - The vmnet event callback writes one byte to the write end when packets
 *     are available (non-blocking write — lost bytes just mean a coalesced
 *     notification, which is fine).
 *   - The Rust side blocks on poll(2) on the read end, then drains it and
 *     calls vmnet_ctx_read_one() in a loop until it returns 0.
 */

#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <sys/uio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <stdint.h>

typedef struct {
    interface_ref    iface;
    dispatch_queue_t queue;
    int              notify_pipe[2];   /* [0]=read [1]=write, both O_NONBLOCK */
    int              shutdown_pipe[2]; /* [0]=read (poll'd by Rust) [1]=write (signalled by Rust) */
    uint8_t          mac[6];
    uint32_t         mtu;
    size_t           max_packet_size;
} vmnet_ctx_t;

/*
 * Start a vmnet interface in HOST_ONLY mode.
 * Returns a heap-allocated context on success, NULL on failure.
 * Must be called as root (or with com.apple.vm.networking entitlement).
 */
vmnet_ctx_t *vmnet_ctx_start(uint32_t requested_mtu) {
    vmnet_ctx_t *ctx = calloc(1, sizeof(vmnet_ctx_t));
    if (!ctx) return NULL;

    if (pipe(ctx->notify_pipe) < 0) { free(ctx); return NULL; }
    fcntl(ctx->notify_pipe[0], F_SETFL, O_NONBLOCK);
    fcntl(ctx->notify_pipe[1], F_SETFL, O_NONBLOCK);

    if (pipe(ctx->shutdown_pipe) < 0) {
        close(ctx->notify_pipe[0]); close(ctx->notify_pipe[1]);
        free(ctx); return NULL;
    }
    /* write end is non-blocking so Rust's signal handler never blocks */
    fcntl(ctx->shutdown_pipe[1], F_SETFL, O_NONBLOCK);

    ctx->queue = dispatch_queue_create("com.darwin-vxlan.vmnet", DISPATCH_QUEUE_SERIAL);

    xpc_object_t params = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(params, vmnet_operation_mode_key, VMNET_HOST_MODE);
    if (requested_mtu > 0)
        xpc_dictionary_set_uint64(params, vmnet_mtu_key, requested_mtu);

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block vmnet_return_t start_status = VMNET_FAILURE;

    interface_ref iface = vmnet_start_interface(params, ctx->queue,
        ^(vmnet_return_t status, xpc_object_t iface_param) {
            start_status = status;
            if (status == VMNET_SUCCESS && iface_param) {
                const char *mac_str =
                    xpc_dictionary_get_string(iface_param, vmnet_mac_address_key);
                if (mac_str) {
                    sscanf(mac_str,
                        "%02hhx:%02hhx:%02hhx:%02hhx:%02hhx:%02hhx",
                        &ctx->mac[0], &ctx->mac[1], &ctx->mac[2],
                        &ctx->mac[3], &ctx->mac[4], &ctx->mac[5]);
                }
                ctx->mtu = (uint32_t)
                    xpc_dictionary_get_uint64(iface_param, vmnet_mtu_key);
                ctx->max_packet_size = (size_t)
                    xpc_dictionary_get_uint64(iface_param, vmnet_max_packet_size_key);
            }
            dispatch_semaphore_signal(sem);
        });

    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    dispatch_release(sem);
    xpc_release(params);

    if (start_status != VMNET_SUCCESS || !iface) {
        dispatch_release(ctx->queue);
        close(ctx->notify_pipe[0]);
        close(ctx->notify_pipe[1]);
        free(ctx);
        return NULL;
    }

    ctx->iface = iface;
    if (ctx->mtu == 0)             ctx->mtu = 1500;
    if (ctx->max_packet_size == 0) ctx->max_packet_size = 1518;

    /* Write one byte to the pipe whenever packets arrive — non-blocking so
     * the GCD thread is never held up if the pipe buffer is full. */
    vmnet_interface_set_event_callback(ctx->iface,
        VMNET_INTERFACE_PACKETS_AVAILABLE, ctx->queue,
        ^(interface_event_t event     __attribute__((unused)),
          xpc_object_t      event_data __attribute__((unused))) {
            uint8_t b = 1;
            (void)write(ctx->notify_pipe[1], &b, 1);
        });

    return ctx;
}

/* File descriptor to poll(2) on — becomes readable when packets are pending. */
int vmnet_ctx_notify_fd(vmnet_ctx_t *ctx) { return ctx->notify_pipe[0]; }

/* Write end of the shutdown pipe. Write any byte to unblock the poll loop. */
int vmnet_ctx_shutdown_write_fd(vmnet_ctx_t *ctx) { return ctx->shutdown_pipe[1]; }

/* Read end of the shutdown pipe — polled alongside notify_fd. */
int vmnet_ctx_shutdown_read_fd(vmnet_ctx_t *ctx) { return ctx->shutdown_pipe[0]; }

uint32_t vmnet_ctx_mtu(vmnet_ctx_t *ctx)             { return ctx->mtu; }
size_t   vmnet_ctx_max_packet_size(vmnet_ctx_t *ctx) { return ctx->max_packet_size; }

/*
 * Read one Ethernet frame into buf[0..buf_len].
 * Returns: >0 = frame length, 0 = no frame available, -1 = error.
 *
 * Call in a loop after poll(notify_fd) returns POLLIN; drain the pipe first
 * (read all pending bytes) so the next poll() waits for fresh events.
 */
int vmnet_ctx_read_one(vmnet_ctx_t *ctx, uint8_t *buf, size_t buf_len) {
    struct iovec iov = { .iov_base = buf, .iov_len = buf_len };
    struct vmpktdesc pkt = {
        .vm_pkt_size   = buf_len,
        .vm_pkt_iov    = &iov,
        .vm_pkt_iovcnt = 1,
        .vm_flags      = 0,
    };
    int count = 1;
    vmnet_return_t ret = vmnet_read(ctx->iface, &pkt, &count);
    if (ret != VMNET_SUCCESS || count == 0) return 0;
    return (int)pkt.vm_pkt_size;
}

/*
 * Write one Ethernet frame.
 * Returns 0 on success, -1 on error.
 */
int vmnet_ctx_write(vmnet_ctx_t *ctx, const uint8_t *buf, size_t len) {
    struct iovec iov = { .iov_base = (void *)buf, .iov_len = len };
    struct vmpktdesc pkt = {
        .vm_pkt_size   = len,
        .vm_pkt_iov    = &iov,
        .vm_pkt_iovcnt = 1,
        .vm_flags      = 0,
    };
    int count = 1;
    vmnet_return_t ret = vmnet_write(ctx->iface, &pkt, &count);
    return (ret == VMNET_SUCCESS) ? 0 : -1;
}

/* Stop the interface and free all resources. */
void vmnet_ctx_stop(vmnet_ctx_t *ctx) {
    if (!ctx) return;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    vmnet_stop_interface(ctx->iface, ctx->queue,
        ^(vmnet_return_t status __attribute__((unused))) {
            dispatch_semaphore_signal(sem);
        });
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    dispatch_release(sem);
    dispatch_release(ctx->queue);
    close(ctx->notify_pipe[0]);
    close(ctx->notify_pipe[1]);
    close(ctx->shutdown_pipe[0]);
    close(ctx->shutdown_pipe[1]);
    free(ctx);
}
