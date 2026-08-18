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
