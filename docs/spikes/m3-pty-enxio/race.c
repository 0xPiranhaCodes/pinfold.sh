// Churns PTY masters from several threads; counts posix_openpt failures by errno.
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <unistd.h>
static atomic_int enxio, other, ok;
static int retry;
static void *churn(void *arg) {
  int held[24]; int n = 0;
  for (int i = 0; i < 20000; i++) {
    int fd = posix_openpt(O_RDWR | O_NOCTTY);
    if (fd < 0 && errno == ENXIO && retry) fd = posix_openpt(O_RDWR | O_NOCTTY);
    if (fd < 0) { if (errno == ENXIO) enxio++; else { other++; fprintf(stderr, "errno %d\n", errno); } }
    else { ok++; held[n++] = fd; }
    if (n == 24 || (rand() & 1 && n)) { close(held[--n]); }
  }
  while (n) close(held[--n]);
  return arg;
}
int main(int argc, char **argv) {
  retry = argc > 1;
  pthread_t t[8];
  for (int i = 0; i < 8; i++) pthread_create(&t[i], 0, churn, 0);
  for (int i = 0; i < 8; i++) pthread_join(t[i], 0);
  printf("retry=%d ok=%d enxio=%d other=%d\n", retry, ok, enxio, other);
}
