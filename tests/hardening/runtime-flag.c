// Prints a process's code-signing flags as the kernel holds them, read with
// csops(CS_OPS_STATUS), and whether the hardened runtime is among them (ADR-0044).
// With no argument it reads its own. Run it through runtime-flag.sh.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// xnu bsd/sys/codesign.h: libSystem exports csops, and the public SDK has no header for it.
int csops(pid_t pid, unsigned int ops, void *useraddr, size_t usersize);
#define CS_OPS_STATUS 0
#define CS_GET_TASK_ALLOW 0x00000004
#define CS_RUNTIME 0x00010000
#define CS_LINKER_SIGNED 0x00020000

int main(int argc, char **argv) {
  pid_t pid = argc > 1 ? (pid_t)atoi(argv[1]) : getpid();
  uint32_t flags = 0;
  if (csops(pid, CS_OPS_STATUS, &flags, sizeof flags) != 0) {
    perror("csops");
    return 1;
  }
  printf("pid %d flags=0x%08x runtime=%s get-task-allow=%s linker-signed=%s\n", pid, flags,
         flags & CS_RUNTIME ? "yes" : "no", flags & CS_GET_TASK_ALLOW ? "yes" : "no",
         flags & CS_LINKER_SIGNED ? "yes" : "no");
  return 0;
}
