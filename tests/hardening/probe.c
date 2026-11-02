/* What a same-user attacker tries against a hardened process (ADR-0020). See audit.sh. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef __APPLE__
#include <mach/mach.h>
#include <mach/mach_vm.h>
#define ATTACH PT_ATTACH
#define DETACH PT_DETACH
#else
#include <elf.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#define ATTACH PTRACE_ATTACH
#define DETACH PTRACE_DETACH
#endif

/* Prints no memory: only how the attempt ended. */
static int attach(pid_t pid) {
  errno = 0;
  if (ptrace(ATTACH, pid, 0, 0) != 0) {
    printf("attach refused: %s\n", strerror(errno));
    return 1;
  }
  int status = 0;
  waitpid(pid, &status, WUNTRACED);
  ptrace(DETACH, pid, (void *)(long)(DETACH == 0 ? 1 : 0), 0);
  printf("attached and detached\n");
  return 0;
}

/* Starts a program and attaches to it, as a debugger in a pane does. */
static int launch(char **program) {
  pid_t child = fork();
  if (child == 0) {
    kill(getpid(), SIGSTOP);
    execvp(program[0], program);
    _exit(127);
  }
  int status = 0;
  waitpid(child, &status, WUNTRACED);
  int result = attach(child);
  kill(child, SIGKILL);
  waitpid(child, &status, 0);
  return result;
}

/* Starts a program with this process as its tracer from before exec, as a debugger does, and
 * says how the program fares. */
static int trace(char **program) {
  pid_t child = fork();
  if (child == 0) {
#ifdef __APPLE__
    ptrace(PT_TRACE_ME, 0, 0, 0);
#else
    ptrace(PTRACE_TRACEME, 0, 0, 0);
#endif
    execvp(program[0], program);
    _exit(127);
  }
  int status = 0, signal_number = 0;
  for (int tick = 0; tick < 300; tick++) {
    if (waitpid(child, &status, WNOHANG | WUNTRACED) == child) {
      if (WIFEXITED(status)) {
        printf("exited with status %d\n", WEXITSTATUS(status));
        return 0;
      }
      if (WIFSIGNALED(status)) {
        printf("killed by signal %d\n", WTERMSIG(status));
        return 0;
      }
      signal_number = WSTOPSIG(status) == SIGTRAP ? 0 : WSTOPSIG(status);
#ifdef __APPLE__
      ptrace(PT_CONTINUE, child, (caddr_t)1, signal_number);
#else
      ptrace(PTRACE_CONT, child, 0, signal_number);
#endif
    }
    usleep(10000);
  }
  printf("running after 3 seconds, still traced\n");
#ifndef __APPLE__
  kill(child, SIGSTOP);
  waitpid(child, &status, 0);
  unsigned long registers[64];
  struct iovec vector = {registers, sizeof registers};
  long got = ptrace(PTRACE_GETREGSET, child, NT_PRSTATUS, &vector);
  printf("registers: %s\n", got == 0 ? "read" : strerror(errno));
#if defined(__aarch64__)
  unsigned long stack = registers[31];
#else
  unsigned long stack = registers[19];
#endif
  errno = 0;
  ptrace(PTRACE_PEEKDATA, child, stack, 0);
  printf("memory: %s\n", errno == 0 ? "read" : strerror(errno));
#endif
  kill(child, SIGKILL);
  return 0;
}

#ifdef __APPLE__
/* Counts `marker` in every readable page of the target. */
static int scan(pid_t pid, const char *marker) {
  mach_port_t task = 0;
  kern_return_t result = task_for_pid(mach_task_self(), pid, &task);
  if (result != KERN_SUCCESS) {
    printf("task_for_pid refused: %s\n", mach_error_string(result));
    return 1;
  }
  size_t length = strlen(marker), found = 0;
  mach_vm_address_t address = 0;
  for (;;) {
    mach_vm_size_t size = 0, got = 0;
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object;
    if (mach_vm_region(task, &address, &size, VM_REGION_BASIC_INFO_64, (vm_region_info_t)&info,
                       &count, &object) != KERN_SUCCESS) {
      break;
    }
    char *buffer = (info.protection & VM_PROT_READ) ? malloc(size) : NULL;
    if (buffer && mach_vm_read_overwrite(task, address, size, (mach_vm_address_t)buffer, &got) ==
                      KERN_SUCCESS) {
      for (char *at = buffer; (at = memmem(at, got - (at - buffer), marker, length));
           at += length) {
        found++;
      }
    }
    free(buffer);
    address += size;
  }
  printf("task port granted: marker found %zu times\n", found);
  return found == 0;
}
#else
/* Copies every descriptor it can out of the target, the attack ADR-0020 point 1 stops. */
static int copy_fds(pid_t pid) {
  int target = (int)syscall(SYS_pidfd_open, pid, 0);
  if (target < 0) {
    printf("pidfd_open refused: %s\n", strerror(errno));
    return 1;
  }
  int copied = 0, error = 0;
  for (int fd = 0; fd < 64; fd++) {
    int copy = (int)syscall(SYS_pidfd_getfd, target, fd, 0);
    if (copy >= 0) {
      copied++;
      close(copy);
    } else if (errno != EBADF) {
      error = errno;
    }
  }
  printf("copied %d descriptors%s%s\n", copied, error ? ", refused: " : "",
         error ? strerror(error) : "");
  return copied == 0;
}

#endif

int main(int count, char **arguments) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *mode = count > 2 ? arguments[1] : "";
  if (strcmp(mode, "attach") == 0) {
    return attach((pid_t)atoi(arguments[2]));
  }
  if (strcmp(mode, "launch") == 0) {
    return launch(arguments + 2);
  }
#ifdef __APPLE__
  if (strcmp(mode, "scan") == 0 && count > 3) {
    return scan((pid_t)atoi(arguments[2]), arguments[3]);
  }
#else
  if (strcmp(mode, "copy") == 0) {
    return copy_fds((pid_t)atoi(arguments[2]));
  }
#endif
  if (strcmp(mode, "trace") == 0) {
    return trace(arguments + 2);
  }
  if (strcmp(mode, "hold") == 0) {
    pause();
  }
  fprintf(stderr, "usage: probe attach <pid> | launch <program...> | trace <program...> | "
                  "hold - | scan <pid> <marker> | copy <pid>\n");
  return 2;
}
