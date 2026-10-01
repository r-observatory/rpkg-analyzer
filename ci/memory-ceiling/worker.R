# worker.R <pipeline dir> <out dir> <cores> <package>...
# Memory of the real worker: the Bioconductor pipeline's own clone_package and
# analyze_package for each package, forked with mclapply the way the pipeline forks
# them, while sample.sh records available memory once a second. RPKG_ANALYZER_BIN
# names the build. Writes worker.tsv, meta.txt, mem.tsv and procs.tsv to <out dir>.
# Linux only.
args <- commandArgs(trailingOnly = TRUE)
if (length(args) < 4L) stop("usage: worker.R <pipeline dir> <out dir> <cores> <package>...")
here <- dirname(sub("^--file=", "", grep("^--file=", commandArgs(FALSE), value = TRUE)[1L]))
sampler <- normalizePath(file.path(here, "sample.sh"))
pipeline <- normalizePath(args[1L])
dir.create(args[2L], recursive = TRUE, showWarnings = FALSE)
out <- normalizePath(args[2L])
cores <- as.integer(args[3L])
pkgs <- args[-(1:3)]
if (!nzchar(Sys.getenv("RPKG_ANALYZER_BIN"))) stop("RPKG_ANALYZER_BIN is not set")

# The pipeline, in its own load order.
scripts <- file.path(pipeline, "scripts")
for (f in c("config.R", "git.R", "context.R", "binary.R")) source(file.path(scripts, f))
for (f in sort(list.files(file.path(scripts, "metrics"), pattern = "[.]R$", full.names = TRUE))) source(f)
for (f in c("analyze.R", "export.R", "release_text.R", "update.R")) source(file.path(scripts, f))
setwd(out)
dir.create(WORK_DIR, showWarnings = FALSE)

status_kb <- function(key) {
  l <- grep(paste0("^", key, ":"), readLines("/proc/self/status"), value = TRUE)
  if (length(l)) as.numeric(gsub("[^0-9]", "", l[1L])) else NA_real_
}
field <- function(file, key) {
  if (!file.exists(file)) return(NA_real_)
  l <- grep(paste0("^", key, "[: ]"), readLines(file), value = TRUE)
  if (length(l)) as.numeric(gsub("[^0-9]", "", l[1L])) else NA_real_
}
kernel_log <- function() {
  l <- suppressWarnings(tryCatch(
    system2("sh", c("-c", shQuote("dmesg 2>/dev/null || sudo -n dmesg 2>/dev/null")),
            stdout = TRUE, stderr = FALSE),
    error = function(e) character(0L)))
  if (!is.null(attr(l, "status"))) character(0L) else l
}
short <- function(x) {
  x <- gsub("[[:space:]]+", " ", paste(as.character(x), collapse = " "))
  if (exists(".redact_reason")) x <- .redact_reason(x)
  substr(x, 1L, 200L)
}

# What the pipeline's worker does for one package, with the worker's own peak added.
one <- function(pkg) {
  t0 <- proc.time()[["elapsed"]]
  dest <- file.path(WORK_DIR, pkg)
  on.exit(unlink(dest, recursive = TRUE, force = TRUE), add = TRUE)
  fail <- function(stage, reason) {
    list(package = pkg, ok = FALSE, stage = stage, reason = short(reason), pid = Sys.getpid(),
         elapsed = proc.time()[["elapsed"]] - t0, hwm_kb = status_kb("VmHWM"))
  }
  ok <- tryCatch(clone_package(pkg, dest), error = function(e) FALSE)
  if (!isTRUE(ok)) return(fail("clone", "clone failed"))
  res <- tryCatch(analyze_package(dest, pkg), error = function(e) e)
  if (inherits(res, "error")) return(fail("analyze", conditionMessage(res)))
  list(package = pkg, ok = TRUE, pid = Sys.getpid(),
       elapsed = proc.time()[["elapsed"]] - t0, hwm_kb = status_kb("VmHWM"),
       summary = res$summary, churn = res$churn, api = res$api,
       functions = res$functions, edges = res$edges, datasets = res$datasets,
       text = res$text, binary_versions = res$binary_versions)
}
# The pipeline's wrapper gives each worker its cache and statistics file.
worker <- if (exists(".with_worker_telemetry")) .with_worker_telemetry(one) else one

# Each fork writes its peak as it exits, after its result has been sent back.
exit_dir <- file.path(out, "exit")
dir.create(exit_dir, showWarnings = FALSE)
hook <- bquote(try(writeLines(as.character(.(status_kb)("VmHWM")),
                              file.path(.(exit_dir), Sys.getpid())), silent = TRUE))
hooked <- tryCatch({
  suppressMessages(trace("mcexit", tracer = hook, where = asNamespace("parallel"), print = FALSE))
  TRUE
}, error = function(e) FALSE)

unlink(file.path(out, c("stop", "stopped")))
oom_before <- field("/proc/vmstat", "oom_kill")
cg_before <- field("/sys/fs/cgroup/memory.events", "oom_kill")
klog_before <- length(kernel_log())
system2("bash", c(shQuote(sampler), shQuote(out), "1"), wait = FALSE, stdout = FALSE, stderr = FALSE)
Sys.sleep(1.5)
t_start <- Sys.time()

results <- parallel::mclapply(pkgs, worker, mc.cores = cores, mc.preschedule = FALSE)

elapsed <- as.numeric(difftime(Sys.time(), t_start, units = "secs"))
parent_hwm_kb <- status_kb("VmHWM")
Sys.sleep(1.5)
invisible(file.create(file.path(out, "stop")))
for (i in 1:50) if (file.exists(file.path(out, "stopped"))) break else Sys.sleep(0.1)
oom_after <- field("/proc/vmstat", "oom_kill")
cg_after <- field("/sys/fs/cgroup/memory.events", "oom_kill")
klog <- kernel_log()
klog_new <- if (length(klog) > klog_before) klog[(klog_before + 1L):length(klog)] else character(0L)
klog_oom <- grep("out of memory|oom-kill|oom_reaper|killed process", klog_new,
                 ignore.case = TRUE, value = TRUE)
writeLines(klog_oom, file.path(out, "kernel-oom.txt"))

# The size of each result as the fork sent it: serialized, counted without being held.
serialized_bytes <- function(x) {
  f <- tempfile()
  on.exit(unlink(f), add = TRUE)
  con <- pipe(paste("wc -c >", shQuote(f)), "wb")
  serialize(x, con, xdr = FALSE)
  close(con)
  as.numeric(trimws(readLines(f)))
}
rows <- lapply(seq_along(pkgs), function(i) {
  r <- results[[i]]
  if (!is.list(r)) {
    return(data.frame(package = pkgs[i], ok = FALSE, stage = "crash",
                      reason = if (is.null(r)) "the fork returned nothing" else short(r),
                      pid = NA, versions = NA, analyzer_versions = NA, elapsed_s = NA,
                      hwm_return_kb = NA, hwm_exit_kb = NA, object_bytes = NA,
                      serialized_bytes = NA, stringsAsFactors = FALSE))
  }
  exit_file <- file.path(exit_dir, r$pid)
  data.frame(
    package = pkgs[i], ok = isTRUE(r$ok),
    stage = if (isTRUE(r$ok)) "" else r$stage, reason = if (isTRUE(r$ok)) "" else r$reason,
    pid = r$pid,
    versions = if (isTRUE(r$ok)) nrow(r$summary) else NA,
    analyzer_versions = if (isTRUE(r$ok)) length(r$binary_versions) else NA,
    elapsed_s = round(r$elapsed, 1), hwm_return_kb = r$hwm_kb,
    hwm_exit_kb = if (file.exists(exit_file)) as.numeric(readLines(exit_file)[1L]) else NA,
    object_bytes = as.numeric(utils::object.size(r)),
    serialized_bytes = serialized_bytes(r), stringsAsFactors = FALSE)
})
table <- do.call(rbind, rows)
write.table(table, file.path(out, "worker.tsv"), sep = "\t", quote = FALSE, row.names = FALSE, na = "NA")

mem <- tryCatch(utils::read.table(file.path(out, "mem.tsv"), sep = "\t"), error = function(e) NULL)
meta <- c(
  sprintf("date %s", format(Sys.time(), "%Y-%m-%dT%H:%M:%SZ", tz = "UTC")),
  sprintf("r %s", paste(R.version$major, R.version$minor, sep = ".")),
  sprintf("arch %s", R.version$arch),
  sprintf("analyzer %s", rpkg_analyzer_version()),
  sprintf("pipeline %s", Sys.getenv("PIPELINE_COMMIT", "unknown")),
  sprintf("cores %d", cores),
  sprintf("packages %s", paste(pkgs, collapse = " ")),
  sprintf("elapsed_s %.0f", elapsed),
  sprintf("memtotal_kb %.0f", field("/proc/meminfo", "MemTotal")),
  sprintf("swaptotal_kb %.0f", field("/proc/meminfo", "SwapTotal")),
  sprintf("cgroup_memory_max %s",
          if (file.exists("/sys/fs/cgroup/memory.max")) readLines("/sys/fs/cgroup/memory.max")[1L] else "none"),
  sprintf("cgroup_peak_kb %.0f",
          if (file.exists("/sys/fs/cgroup/memory.peak")) as.numeric(readLines("/sys/fs/cgroup/memory.peak")[1L]) / 1024 else NA),
  sprintf("samples %d", if (is.null(mem)) 0L else nrow(mem)),
  sprintf("min_available_kb %.0f", if (is.null(mem)) NA else min(mem[[2L]])),
  sprintf("parent_hwm_kb %.0f", parent_hwm_kb),
  sprintf("oom_kills %.0f", oom_after - oom_before),
  sprintf("cgroup_oom_kills %.0f", cg_after - cg_before),
  sprintf("kernel_log %s", if (length(klog)) "readable" else "unreadable"),
  sprintf("kernel_log_oom_lines %d", length(klog_oom)),
  sprintf("exit_hook %s", if (hooked) "on" else "off"))
writeLines(meta, file.path(out, "meta.txt"))

# One line per package for the log: names and memory figures only.
for (i in seq_len(nrow(table))) {
  with(table[i, ], cat(sprintf(
    "%s %s versions=%s worker VmHWM at return %.0f MiB, at exit %.0f MiB, result %.1f MiB serialized\n",
    package, if (ok) "ok" else paste("failed:", stage), versions,
    hwm_return_kb / 1024, hwm_exit_kb / 1024, serialized_bytes / 1024^2)))
}
cat(sprintf("minimum available memory %.0f MiB of %.0f MiB, parent VmHWM %.0f MiB, OOM kills %.0f\n",
            min(mem[[2L]]) / 1024, field("/proc/meminfo", "MemTotal") / 1024, parent_hwm_kb / 1024,
            oom_after - oom_before))
