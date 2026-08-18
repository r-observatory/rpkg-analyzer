# Regenerates the dataset fixtures. Committed output means the suite runs
# without R; this script exists so the inputs are reproducible and reviewable.
# Run from the repo root: Rscript tests/fixtures/make.R
d <- "tests/fixtures/pkg/data"
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

# Real objects from real packages, committed so the suite needs neither sf, sp,
# data.table nor zoo installed. Each caught something a hand-built imitation did
# not: sf's own nc example names its CRS rather than declaring an EPSG code, and
# every data.table carries an external pointer.
if (requireNamespace("sf", quietly = TRUE)) {
  nc <- sf::st_read(system.file("shape/nc.shp", package = "sf"), quiet = TRUE)
  sv(nc, "real_sf_nc", 3)
  sv(sf::st_as_sf(data.frame(id = 1:5, x = 1:5, y = 5:1), coords = c("x", "y"), crs = 3857),
     "real_sf_points", 3)
}
if (requireNamespace("sp", quietly = TRUE)) {
  sv(sp::SpatialPointsDataFrame(cbind(c(1, 2, 3), c(4, 5, 6)), data.frame(v = c(10, 20, 30)),
                                proj4string = sp::CRS("+proj=longlat +datum=WGS84")),
     "real_sp_points", 3)
}
if (requireNamespace("data.table", quietly = TRUE)) {
  sv(data.table::data.table(id = 1:100, grp = rep(letters[1:4], 25), val = seq_len(100) / 7),
     "real_datatable", 3)
  sv(data.table::data.table(a = 1:3, b = letters[1:3]), "a_datatable", 3)
}
sv(AirPassengers, "real_ts_air", 3)
if (requireNamespace("zoo", quietly = TRUE)) {
  sv(zoo::zoo(seq_len(50) / 3, as.Date("2020-01-01") + 0:49), "real_zoo_date", 3)
}
sv(data.frame(a = integer(0), b = character(0)), "frame_zero_rows", 3)

# Metadata patterns authors actually use, none of which was being read.
lb <- c(1, 2, 1, 3)
attr(lb, "label")  <- "Respondent age band"
attr(lb, "labels") <- c(Young = 1, Middle = 2, Old = 3)
attr(lb, "format.stata") <- "%9.0g"
class(lb) <- c("haven_labelled", "vctrs_vctr", "double")
lf <- data.frame(x = 1:4); lf$age <- lb; attr(lf, "label") <- "Survey wave 1"
sv(lf, "labelled_frame", 3)

hm <- data.frame(wt = c(1.5, 2.5))
attr(hm$wt, "label") <- "Body weight"; attr(hm$wt, "units") <- "kg"
sv(hm, "hmisc_labels", 3)

cm <- data.frame(a = 1:3); comment(cm) <- "Collected 2019, see vignette"
sv(cm, "commented", 3)

am <- data.frame(z = 1:3)
attr(am, "source_url") <- "https://example.org/data.csv"
attr(am, "license")    <- "CC-BY-4.0"
attr(am, "collected")  <- as.Date("2021-05-04")
sv(am, "author_metadata", 3)

if (requireNamespace("Matrix", quietly = TRUE)) {
  sv(Matrix::Matrix(c(1, 0, 0, 2), 2, 2, sparse = TRUE), "sparse_matrix", 3)
}

# Sparse matrices, packed rasters, graphs and higher-rank arrays.
if (requireNamespace("Matrix", quietly = TRUE)) {
  sv(Matrix::sparseMatrix(i = c(1, 3, 5), j = c(2, 4, 6), x = c(1.5, 2.5, 3.5),
                          dims = c(100, 100)), "sparse_big", 3)
  sv(methods::as(Matrix::Matrix(c(1, 0, 0, 0, 2, 0, 0, 0, 3), 3, 3, sparse = TRUE),
                 "TsparseMatrix"), "sparse_dgt", 3)
}
if (requireNamespace("terra", quietly = TRUE)) {
  tr <- terra::rast(nrows = 8, ncols = 12)
  terra::values(tr) <- 1:96
  sv(terra::wrap(tr), "terra_packed", 3)
}
if (requireNamespace("igraph", quietly = TRUE)) {
  sv(igraph::make_ring(10), "igraph_ring", 3)
  g <- igraph::sample_gnp(20, 0.2)
  igraph::V(g)$name <- paste0("n", 1:20)
  sv(g, "igraph_weighted", 3)
}
sv(array(seq_len(120) / 7, dim = c(2, 3, 4, 5)), "arr_4d_dbl", 3)

# --- The Matrix class zoo, the object systems and raster ----------------------
# Each of these used to cost the whole file it sat in rather than just itself:
# a compiled function body is reachable from a reference class, an S7 object and
# a raster's layer data alike, and reading one as a plain vector left the stream
# out of step for everything that followed.
if (requireNamespace("Matrix", quietly = TRUE)) {
  M <- asNamespace("Matrix")
  sp <- M$sparseMatrix
  i <- c(1, 3, 5); j <- c(2, 4, 6)
  sv(sp(i = i, j = j, x = c(1, 2, 3), dims = c(8, 8)), "spm_dgc")
  sv(as(sp(i = i, j = j, x = c(1, 2, 3), dims = c(8, 8)), "RsparseMatrix"), "spm_dgr")
  sv(sp(i = c(1, 2, 3), j = c(1, 2, 3), x = c(1, 2, 3), dims = c(5, 5), symmetric = TRUE), "spm_dsc")
  sv(sp(i = c(2, 3), j = c(1, 2), x = c(1, 2), dims = c(4, 4), triangular = TRUE), "spm_dtc")
  sv(sp(i = i, j = j, dims = c(8, 8)), "spm_ngc")          # pattern: no values slot
  sv(M$Diagonal(4), "spm_unit_diag")                        # unit diagonal: nothing stored
}

Acc <- setRefClass("Acc", fields = list(balance = "numeric", owner = "character"))
sv(Acc$new(balance = 100, owner = "a"), "oo_refclass")
if (requireNamespace("R6", quietly = TRUE)) {
  Cnt <- R6::R6Class("Cnt", public = list(n = 0, add = function() self$n <- self$n + 1))
  sv(Cnt$new(), "oo_r6")
}
if (requireNamespace("S7", quietly = TRUE)) {
  Pt <- S7::new_class("Pt", properties = list(x = S7::class_numeric, y = S7::class_numeric))
  sv(Pt(x = 1, y = 2), "oo_s7")
}

if (requireNamespace("raster", quietly = TRUE)) {
  rr <- raster::raster(nrows = 10, ncols = 20, xmn = 0, xmx = 10, ymn = 0, ymx = 5,
                       crs = "EPSG:4326")
  rr[] <- seq_len(200)
  sv(rr, "raster_layer")
  sv(raster::brick(rr, rr), "raster_brick")
}

# A compiled function saved beside real data in one file. Everything after the
# function depends on the reader coming out of it in step.
cmp_fun <- compiler::cmpfun(function(x, y = 2) { z <- x + y; z * 2 })
after_compiled <- data.frame(a = 1:4, b = letters[1:4], stringsAsFactors = FALSE)
save(cmp_fun, after_compiled, file = file.path(d, "mixed_compiled.rda"), version = 3)

# --- The text formats data() accepts -----------------------------------------
# Every one of these is read a fixed way that does not follow from its name:
# .txt and .tab split on runs of whitespace rather than on tabs, .csv splits on
# a semicolon, and a header one field short means the first column names the
# rows. The .dat and .tsv files are here to stay unreadable: data() has no entry
# for them, so nothing in data/ with those names can be loaded.
wl <- function(lines, name) writeLines(lines, file.path(d, name))
wl(c("grade   sex   score",
     "    6   M        43",
     "    7   F        88",
     "    8   M        61"), "txt_plain.txt")
wl(c("  Expt  Run  Speed",         # header one short: first field names the row
     "001    1    1    850",
     "002    1    2    740",
     "003    2    1    900"), "tab_rownames.tab")
wl(c("height;weight;sex", "1.7;65;F", "1.8;80;M"), "csv_semicolon.csv")
wl(c("height,weight,sex", "1.7,65,F", "1.8,80,M"), "csv_comma.csv")
wl(c("city;pop", '"Paris, France";2140000', '"Lyon, France";515000'), "csv_quoted.csv")
wl(c("# a note about the file", "x y", "1 2", "3 4  # trailing note"), "txt_comments.txt")
wl(c("a b", "1 2", "3 4"), "notdata.dat")
wl(c("a\tb", "1\t2"), "notdata.tsv")
for (cf in list(list("gz", gzfile), list("bz2", bzfile), list("xz", xzfile))) {
  con <- cf[[2]](file.path(d, paste0("txt_", cf[[1]], ".txt.", cf[[1]])), "wt")
  writeLines(c("k v", "1 10", "2 20", "3 30"), con); close(con)
}

# The same table written both ways. Reading the text file the way data() does
# is what lets the two carry one fingerprint and dedup against each other.
same_as_text <- data.frame(g = c("a","b","a"), n = c(1L,2L,3L), v = c(1.5,2.5,3.5),
                           stringsAsFactors = FALSE)
sv(same_as_text, "same_as_rda")
wl(c("g n v", "a 1 1.5", "b 2 2.5", "a 3 3.5"), "same_as_text.txt")
# data() has no entry for .rds, and in an installed package data/Rdata.rds is
# the lazy-load index rather than a dataset.
saveRDS(data.frame(a = 1:3), file.path(d, "notdata.rds"))
