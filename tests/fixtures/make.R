# Regenerates the dataset fixtures. Committed output means the suite runs
# without R; this script exists so the inputs are reproducible and reviewable.
# Run from the repo root: Rscript tests/fixtures/make.R
dir.create(d, recursive = TRUE, showWarnings = FALSE)
sv <- function(obj, name, version = 3) {
  assign(name, obj); save(list = name, file = file.path(d, paste0(name, ".rda")), version = version)
}
# ALTREP: 1:n is a compact_intseq under version 3
sv(data.frame(id = 1:5, v = c(1.5,2.5,3.5,4.5,5.5)), "altrep_frame", 3)
sv(data.frame(id = 1:5, v = c(1.5,2.5,3.5,4.5,5.5)), "plain_frame", 2)
sv(seq_len(10), "altrep_seq", 3)
sv(sort(c(3L,1L,2L)), "altrep_wrap", 3)
# time series
sv(ts(1:48, start = c(1949,1), frequency = 12), "ts_month", 2)
sv(ts(matrix(1:40, ncol = 2), start = c(2000,1), frequency = 4), "ts_multi", 2)
# matrix + higher-dim array
sv(matrix(1:12, nrow = 3, dimnames = list(c("a","b","c"), c("w","x","y","z"))), "mat_named", 2)
sv(array(1:24, dim = c(2,3,4)), "arr_3d", 2)
# list column frame (the sf shape, without needing sf installed)
lf <- data.frame(a = 1:3)
lf$geom <- list(c(1,2), c(3,4), c(5,6))
attr(lf$geom, "class") <- c("sfc_POINT", "sfc")
attr(lf$geom, "crs") <- structure(list(input = "EPSG:4326", wkt = "GEOGCRS[\"WGS 84\"]"), class = "crs")
attr(lf$geom, "bbox") <- structure(c(xmin=1,ymin=2,xmax=5,ymax=6), class = "bbox")
sv(lf, "sf_like", 2)
# plain list, factor, dates
sv(list(a = 1, b = "two", c = TRUE), "plain_list", 2)
sv(factor(c("a","b","a","c")), "fac", 2)
sv(as.Date("2020-01-01") + 0:9, "dates", 2)
sv(as.POSIXct("2020-01-01 10:00", tz = "UTC") + 0:4, "times", 2)
cat("fixtures:", length(list.files(d)), "\n")
