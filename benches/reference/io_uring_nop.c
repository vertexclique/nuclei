/*
 * Reference for the Rust ring layer's NOP round-trip measurement (bench_nop_round_trip).
 *
 * Raw syscalls only: no liburing. The ring is set up with the same flags as the Rust ring
 * (SINGLE_ISSUER | DEFER_TASKRUN | SUBMIT_ALL | CLAMP | CQSIZE, plus NO_SQARRAY with a retry
 * without it on EINVAL), 256 submission entries and a completion queue of 512. For each batch
 * size (1 and 32) it runs 7 times: submit `batch` NOPs with one io_uring_enter that also waits
 * for `batch` completions, reap them, repeat until 200000 completions, and report the time per
 * op. Output is one line per batch size, the same format as the Rust test:
 *
 *   nop batch=1 ops=200000 runs=7 median_ns_per_op=1234.5
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/io_uring.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define ENTRIES 256u
#define OPS 200000ull
#define RUNS 7

struct ring {
	int fd;
	unsigned *khead, *ktail;
	unsigned sq_mask, cq_mask;
	unsigned *chead, *ctail;
	struct io_uring_sqe *sqes;
	struct io_uring_cqe *cqes;
	unsigned tail; /* local submission tail */
};

static int die(const char *what)
{
	fprintf(stderr, "%s: %s\n", what, strerror(errno));
	exit(1);
}

static int sys_setup(unsigned entries, struct io_uring_params *p)
{
	return (int)syscall(__NR_io_uring_setup, entries, p);
}

static int sys_enter(int fd, unsigned to_submit, unsigned min_complete, unsigned flags)
{
	return (int)syscall(__NR_io_uring_enter, fd, to_submit, min_complete, flags, NULL, 0);
}

static void ring_init(struct ring *r)
{
	struct io_uring_params p;
	unsigned base = IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN |
			IORING_SETUP_SUBMIT_ALL | IORING_SETUP_CLAMP | IORING_SETUP_CQSIZE;
	int has_array = 0;

	for (int attempt = 0; attempt < 2; attempt++) {
		int no_sqarray = attempt == 0;
		memset(&p, 0, sizeof p);
		p.flags = base | (no_sqarray ? IORING_SETUP_NO_SQARRAY : 0);
		p.cq_entries = ENTRIES * 2;
		r->fd = sys_setup(ENTRIES, &p);
		if (r->fd >= 0) {
			has_array = !no_sqarray;
			break;
		}
		if (errno != EINVAL || attempt == 1)
			die("io_uring_setup");
	}

	size_t sq_len = has_array ? p.sq_off.array + p.sq_entries * sizeof(unsigned) : 0;
	size_t cq_len = p.cq_off.cqes + p.cq_entries * sizeof(struct io_uring_cqe);
	size_t ring_len = sq_len > cq_len ? sq_len : cq_len;
	char *ring = mmap(NULL, ring_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE,
			  r->fd, IORING_OFF_SQ_RING);
	if (ring == MAP_FAILED)
		die("mmap rings");
	r->sqes = mmap(NULL, p.sq_entries * sizeof(struct io_uring_sqe), PROT_READ | PROT_WRITE,
		       MAP_SHARED | MAP_POPULATE, r->fd, IORING_OFF_SQES);
	if (r->sqes == MAP_FAILED)
		die("mmap sqes");

	r->khead = (unsigned *)(ring + p.sq_off.head);
	r->ktail = (unsigned *)(ring + p.sq_off.tail);
	r->sq_mask = p.sq_entries - 1;
	r->chead = (unsigned *)(ring + p.cq_off.head);
	r->ctail = (unsigned *)(ring + p.cq_off.tail);
	r->cq_mask = p.cq_entries - 1;
	r->cqes = (struct io_uring_cqe *)(ring + p.cq_off.cqes);
	if (has_array) {
		unsigned *array = (unsigned *)(ring + p.sq_off.array);
		for (unsigned i = 0; i < p.sq_entries; i++)
			array[i] = i;
	}
	r->tail = __atomic_load_n(r->ktail, __ATOMIC_RELAXED);
}

static void push_nop(struct ring *r, uint64_t user_data)
{
	struct io_uring_sqe *sqe = &r->sqes[r->tail & r->sq_mask];
	memset(sqe, 0, sizeof *sqe);
	sqe->opcode = IORING_OP_NOP;
	sqe->fd = -1;
	sqe->user_data = user_data;
	r->tail++;
}

/* Publish the tail, enter the kernel and wait for `wait` completions. */
static void submit_and_wait(struct ring *r, unsigned wait)
{
	__atomic_store_n(r->ktail, r->tail, __ATOMIC_RELEASE);
	unsigned to_submit = r->tail - __atomic_load_n(r->khead, __ATOMIC_ACQUIRE);
	while (sys_enter(r->fd, to_submit, wait, IORING_ENTER_GETEVENTS) < 0) {
		if (errno != EINTR)
			die("io_uring_enter");
	}
}

/* Drain every available completion; returns how many. */
static unsigned reap(struct ring *r)
{
	unsigned start = __atomic_load_n(r->chead, __ATOMIC_RELAXED);
	unsigned tail = __atomic_load_n(r->ctail, __ATOMIC_ACQUIRE);
	unsigned head = start;
	while (head != tail) {
		struct io_uring_cqe cqe = r->cqes[head & r->cq_mask];
		if (cqe.res != 0) {
			fprintf(stderr, "unexpected completion result %d\n", cqe.res);
			exit(1);
		}
		head++;
	}
	if (head != start)
		__atomic_store_n(r->chead, head, __ATOMIC_RELEASE);
	return head - start;
}

static int cmp_double(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return (x > y) - (x < y);
}

static double now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1e9 + ts.tv_nsec;
}

int main(void)
{
	struct ring r;
	ring_init(&r);

	unsigned batches[2] = { 1, 32 };
	for (int b = 0; b < 2; b++) {
		unsigned batch = batches[b];
		double per_op[RUNS];
		for (int run = 0; run < RUNS; run++) {
			double start = now_ns();
			uint64_t done = 0, id = 0;
			while (done < OPS) {
				for (unsigned i = 0; i < batch; i++)
					push_nop(&r, id++);
				submit_and_wait(&r, batch);
				done += reap(&r);
			}
			per_op[run] = (now_ns() - start) / (double)OPS;
		}
		qsort(per_op, RUNS, sizeof(double), cmp_double);
		printf("nop batch=%u ops=%llu runs=%d median_ns_per_op=%.1f\n", batch,
		       (unsigned long long)OPS, RUNS, per_op[RUNS / 2]);
	}
	return 0;
}
