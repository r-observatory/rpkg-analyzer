/* peak <stdout-file> <command> [args...]
   Runs the command with stdout to the file and stderr to <stdout-file>.err, stops it
   at its exit while its memory is still mapped, and prints one line:
     exit vmhwm_kb vmpeak_kb maxrss_kb user_s sys_s wall_s
   vmhwm_kb and vmpeak_kb come from /proc/<pid>/status at the exit stop, so they are
   exact and not sampled. They are -1 when the kernel gave no exit stop (a kill).
   exit is the exit status, or 128 plus the signal number. Linux only. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void read_peaks(pid_t pid, long *hwm, long *peak) {
  char path[64], line[256];
  snprintf(path, sizeof path, "/proc/%d/status", (int)pid);
  FILE *f = fopen(path, "r");
  if (!f) return;
  while (fgets(line, sizeof line, f)) {
    if (!strncmp(line, "VmHWM:", 6)) *hwm = atol(line + 6);
    if (!strncmp(line, "VmPeak:", 7)) *peak = atol(line + 7);
  }
  fclose(f);
}

int main(int argc, char **argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: peak <stdout-file> <command> [args...]\n");
    return 2;
  }
  char err[4096];
  snprintf(err, sizeof err, "%s.err", argv[1]);
  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);

  pid_t pid = fork();
  if (pid < 0) return 2;
  if (pid == 0) {
    int out = open(argv[1], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    int errfd = open(err, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (out < 0 || errfd < 0) _exit(126);
    dup2(out, 1);
    dup2(errfd, 2);
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    execv(argv[2], argv + 2);
    _exit(127);
  }

  int status = 0, options_set = 0;
  long hwm = -1, peak = -1;
  struct rusage ru;
  memset(&ru, 0, sizeof ru);
  for (;;) {
    if (wait4(pid, &status, 0, &ru) < 0) break;
    if (WIFEXITED(status) || WIFSIGNALED(status)) break;
    if (!WIFSTOPPED(status)) continue;
    int sig = WSTOPSIG(status);
    if (!options_set) {
      /* The stop at exec: ask for a stop at exit, and for the child to die with us. */
      ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACEEXIT | PTRACE_O_EXITKILL);
      options_set = 1;
      ptrace(PTRACE_CONT, pid, 0, 0);
    } else if (sig == SIGTRAP && (status >> 16) == PTRACE_EVENT_EXIT) {
      read_peaks(pid, &hwm, &peak);
      ptrace(PTRACE_CONT, pid, 0, 0);
    } else {
      ptrace(PTRACE_CONT, pid, 0, sig == SIGTRAP ? 0 : sig);
    }
  }
  clock_gettime(CLOCK_MONOTONIC, &t1);

  int code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
  printf("%d %ld %ld %ld %.2f %.2f %.2f\n", code, hwm, peak, ru.ru_maxrss,
         ru.ru_utime.tv_sec + ru.ru_utime.tv_usec / 1e6,
         ru.ru_stime.tv_sec + ru.ru_stime.tv_usec / 1e6,
         (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9);
  return 0;
}
