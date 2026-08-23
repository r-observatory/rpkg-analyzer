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

# Whitespace separation is not the same as tab separation, and these are the
# places the two come apart. R reads all of them one fixed way, so the catalogue
# describes what a caller gets rather than what the file looks like it means.
wl(c("name\tcity\tn",              # a value holding a space is split by it, and
     "ann\tNew York\t1",           # the leftmost field then reads as a row name
     "bob\tLos Angeles\t2"), "ws_value_has_space.txt")
wl(c("a\tb\tc", "1\t\t3", "4\t5\t6"), "ws_ragged.tab")   # empty cell: read.table refuses
wl(c("name\tn", '"New York"\t1', '"Los Angeles"\t2'), "ws_quoted.txt")

# --- inst/extdata -------------------------------------------------------------
# The same bytes mean different things depending on which directory they sit in.
# data() applies its rules only under data/; nothing loads a file under extdata
# by name, so the extension carries no promise and a .csv here is an ordinary
# comma-separated file.
e <- file.path(dirname(d), "inst", "extdata")
dir.create(file.path(e, "nested"), recursive = TRUE, showWarnings = FALSE)
writeLines(c("height,weight,sex", "1.7,65,F", "1.8,80,M"), file.path(e, "ext_comma.csv"))
saveRDS(data.frame(a = 1:4, b = c("w","x","y","z")), file.path(e, "ext_object.rds"))
writeLines(c("a\tb", "1\tx", "2\ty"), file.path(e, "ext_tabbed.tsv"))
writeLines("not something we open", file.path(e, "ext_ignored.xlsx"))
writeLines(c("k,v", "1,10"), file.path(e, "nested", "ext_nested.csv"))

# --- the save format R used before 1.4.0 -------------------------------------
# save() cannot write this any more, so the bytes are written out directly. They
# are hand-built rather than copied from a package, and R loads them, which is
# what makes this a fixture and not a guess: a data.frame of 3 rows and 2
# columns with a factor, a numeric vector holding every special value the format
# can carry, and an integer vector with missing values, which the archive has
# more than a thousand of.
#
# Three things here are deliberate. The closure among the nodes is never
# referenced, but still has to be consumed exactly, because a file in the
# archive keeps functions beside its data and misreading one leaves every
# definition after it at the wrong offset. It is defined out of the order the
# table lists it in, which is what separates reading the table from counting
# positions. And three objects share the file, as they do in the archive.
writeLines(c(
    "1976",
    "7 25 32",
    "0 10 \"names\"",
    "1 20 \"row.names\"",
    "2 30 \"class\"",
    "3 40 \"levels\"",
    "4 50 \"df\"",
    "5 60 \"specials\"",
    "6 70 \"int_na\"",
    "7 100",
    "8 110",
    "9 120",
    "10 130",
    "11 140",
    "12 150",
    "13 160",
    "14 170",
    "15 180",
    "16 190",
    "17 200",
    "18 210",
    "19 220",
    "20 230",
    "21 240",
    "22 250",
    "23 260",
    "24 270",
    "25 280",
    "26 290",
    "27 300",
    "28 310",
    "29 320",
    "30 330",
    "31 340",
    "27 3 0 0 -1 -1 -1 -1",
    "7 2 0 0 -1 110 310 50",
    "8 19 1 0 120 2",
    "180 190",
    "9 2 0 0 -1 150 130 10",
    "10 2 0 0 -1 160 140 20",
    "11 2 0 0 -1 170 -1 30",
    "12 16 0 0 -1 2",
    "270 280",
    "13 13 0 0 -1 3",
    "1 2 3",
    "14 16 0 0 -1 1",
    "260",
    "15 14 0 0 -1 3",
    " 1.5",
    " 2.5",
    " 3.5",
    "16 13 1 0 220 3",
    "1 2 1",
    "17 16 0 0 -1 2",
    "240 250",
    "18 16 0 0 -1 1",
    "290",
    "19 2 0 0 -1 200 230 40",
    "20 2 0 0 -1 210 -1 30",
    "21 9 0 0 -1 1 \"a\"",
    "22 9 0 0 -1 1 \"b\"",
    "23 9 0 0 -1 10 \"data.frame\"",
    "24 9 0 0 -1 1 \"x\"",
    "25 9 0 0 -1 1 \"y\"",
    "26 9 0 0 -1 6 \"factor\"",
    "28 2 0 0 -1 320 330 60",
    "29 14 0 0 -1 5",
    " NA",
    " NaN",
    " Inf",
    " -Inf",
    " 1.5",
    "30 2 0 0 -1 340 -1 70",
    "31 13 0 0 -1 4",
    " NA",
    " 7",
    " NA",
    " 9",
    "100"
  ), file.path(d, "v1_ascii_frame.rda"))

# A builtin saved beside real data. Nothing about the builtin is worth
# recording, but it has to be read exactly, or the frame after it is lost with
# it the way objects after a compiled function used to be.
builtin_fn <- sum
after_builtin <- data.frame(a = 1:4, b = c(2.5, 3.5, 4.5, 5.5))
save(builtin_fn, after_builtin, file = file.path(d, "mixed_builtin.rda"), version = 3)

# A string carrying an attribute. R reads one and throws it away, noting that
# older files can have them, so the bytes are written out directly: save()
# will not produce one. R loads this as c("hello", "world"); leaving the
# attribute unread takes the second string with it.
writeBin(as.raw(c(0x52,0x44,0x58,0x32,0x0a,0x58,0x0a,0x00,0x00,0x00,0x02,0x00,0x04,0x04,0x00,0x00,0x02,0x03,0x00,0x00,0x00,0x04,0x02,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x09,0x00,0x00,0x00,0x01,0x73,0x00,0x00,0x00,0x10,0x00,0x00,0x00,0x02,0x00,0x00,0x02,0x09,0x00,0x00,0x00,0x05,0x68,0x65,0x6c,0x6c,0x6f,0x00,0x00,0x04,0x02,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x09,0x00,0x00,0x00,0x05,0x62,0x6f,0x67,0x75,0x73,0x00,0x00,0x00,0x10,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x09,0x00,0x00,0x00,0x01,0x78,0x00,0x00,0x00,0xfe,0x00,0x00,0x00,0x09,0x00,0x00,0x00,0x05,0x77,0x6f,0x72,0x6c,0x64,0x00,0x00,0x00,0xfe)), file.path(d, "string_with_attribute.rda"))

# The same text stored two ways. R records a string's encoding beside it, and
# latin1 is not UTF-8: reading one as the other replaces every accented
# character with a marker, so the text is wrong and the fingerprint taken over
# it no longer matches the same text stored as UTF-8.
enc_latin <- c("caf\xe9", "na\xefve")
Encoding(enc_latin) <- "latin1"
latin_txt <- data.frame(txt = enc_latin, n = 1:2, stringsAsFactors = FALSE)
utf8_txt  <- data.frame(txt = enc2utf8(enc_latin), n = 1:2, stringsAsFactors = FALSE)
save(latin_txt, file = file.path(d, "latin_txt.rda"), version = 3)
save(utf8_txt,  file = file.path(d, "utf8_txt.rda"),  version = 3)

# One dataset name carried by two files. data() loads whichever comes first in
# its own order, which is the saved image, and never opens the csv, so only the
# image is a dataset a reader can reach.
one_per_name <- data.frame(from_rda = 1:3, second = c(4, 5, 6))
save(one_per_name, file = file.path(d, "one_per_name.rda"), version = 2)
writeLines(c("from_csv;x;y;z", "1;2;3;4"), file.path(d, "one_per_name.csv"))

# --- help pages --------------------------------------------------------------
# A catalogue that lists names and says nothing about any of them is a poor
# catalogue, and the package has already written the sentence. The alias names
# what the page documents, which is how a title reaches the dataset it belongs
# to, and the title carries markup that has to come off.
man <- file.path(dirname(d), "man")
dir.create(man, recursive = TRUE, showWarnings = FALSE)
writeLines(c(
  "\\name{altrep_frame}",
  "\\docType{data}",
  "\\alias{altrep_frame}",
  "\\alias{plain_frame}",
  "\\title{Readings from the \\code{example} instrument}",
  "\\description{Two columns of nothing in particular.}",
  "\\keyword{datasets}"
), file.path(man, "altrep_frame.Rd"))

# A file written on a Mac before OS X separates its lines with a carriage
# return and nothing else. Most readers do not treat that as a line break, so
# the whole file arrives as one line and a table is reported as having no rows.
# It is still in the archive, in packages last touched in 2010.
cat("a;b", "1;2", "3;4", "5;6", sep = "\r", file = file.path(d, "cr_endings.csv"))

# A comma-separated file where data() reads semicolons. It really does load as
# one column named after the whole header line, so that is what is reported;
# what the file plainly is gets recorded beside it.
writeLines(c("alpha,beta,gamma", "1,2,3", "4,5,6"), file.path(d, "comma_in_csv.csv"))

# --- what a bare vector holds ------------------------------------------------
# A length on its own does not distinguish a column of numbers from a column of
# names, and raw and complex vectors were reported as objects of length zero
# because their bytes are read for size rather than kept.
sv(c(1.5, 2.5, NA, 4.5, 100.25), "vec_numeric")
sv(c("alpha", "beta", "alpha", NA), "vec_character")
sv(as.raw(c(1, 2, 255)), "vec_raw")
sv(c(1+2i, 3-4i), "vec_complex")
sv(factor(c("a", "b", "a", "c")), "vec_factor")
sv(as.raw(c(1, 2, 255)), "vec_raw_same")   # byte-identical to vec_raw
sv(as.raw(c(1, 2, 254)), "vec_raw_diff")   # one byte apart from vec_raw

# --- what summary() would say ------------------------------------------------
# A range says where the values stop, not where they sit. These are the numbers
# summary() gives for each kind of column, checked against it.
sv(data.frame(
  num = c(1.5, 2.5, NA, 4.5, 100.25),
  int = c(1L, 5L, 3L, NA, 9L),
  lgl = c(TRUE, FALSE, NA, TRUE, TRUE),
  chr = c("a", "bb", "", NA, "ccc"),
  fac = factor(c("a", "b", "a", "c", "b")),
  dat = as.Date(c("2020-01-01", "2021-06-15", "2019-03-02", NA, "2020-07-04")),
  stringsAsFactors = FALSE
), "summary_kinds")

# A geometry with a height, so the dimension is not the usual XY, and an xts
# whose index class is kept under a name of its own.
if (requireNamespace("sf", quietly = TRUE)) {
  sv(sf::st_sf(id = 1:2,
               geometry = sf::st_sfc(sf::st_point(c(1, 2, 3)),
                                     sf::st_point(c(4, 5, 6)), crs = 4326)), "sf_three_d")
}
if (requireNamespace("xts", quietly = TRUE)) {
  sv(xts::xts(matrix(c(1.5, 2.5, 3.5, 4.5), ncol = 2),
              as.POSIXct(c("2020-01-01 00:00", "2020-01-02 06:00"), tz = "UTC")), "xts_series")
}

# A logical sparse matrix counts rather than averages, and a column holding an
# infinity has a maximum JSON cannot write, which reads the same as not having
# one unless the infinities are counted.
if (requireNamespace("Matrix", quietly = TRUE)) {
  sv(as(Matrix::sparseMatrix(i = c(1, 3, 5), j = c(2, 4, 6), x = c(1, 2, 3),
                             dims = c(8, 8)) > 0, "lMatrix"), "spm_lgc")
}
sv(data.frame(v = c(as.numeric(1:99), Inf), w = as.numeric(1:100)), "with_infinity")

# A raster with everything a reader needs to judge the figures above: the cell
# size, whether the cells are here or behind a file handle, the sentinel that
# stands in for nothing, and the range of each named layer.
if (requireNamespace("raster", quietly = TRUE)) {
  rr <- raster::raster(nrows = 10, ncols = 20, xmn = 0, xmx = 10, ymn = 0, ymx = 5,
                       crs = "EPSG:4326")
  rr[] <- seq_len(200)
  bb <- raster::brick(rr, rr * 2)
  names(bb) <- c("elev", "depth")
  raster::NAvalue(bb) <- -9999
  sv(bb, "raster_named_brick")
}
# A dense grid that is almost all zeros. It has the shape of sparse data
# without the class that announces it.
sv(local({ m <- matrix(0, nrow = 10, ncol = 10); m[1:5] <- 1:5; m }), "mostly_zero")

# A tibble, a data.table and a plain frame all inherit from data.frame, and the
# difference lives in the inheritance chain where anything matching on the class
# gets it wrong: an exact match counts no tibbles, a substring match counts
# everything. That mistake has already undercounted the archive once.
flav_plain <- data.frame(a = 1:3, b = c("x", "y", "z"), stringsAsFactors = FALSE)
sv(flav_plain, "flav_plain")
if (requireNamespace("tibble", quietly = TRUE)) {
  sv(tibble::as_tibble(flav_plain), "flav_tibble")
  sv(structure(flav_plain, class = c("grouped_df", "tbl_df", "tbl", "data.frame")),
     "flav_grouped")
}
if (requireNamespace("data.table", quietly = TRUE)) {
  sv(data.table::as.data.table(flav_plain), "flav_data_table")
}

# --- what a dataset says about itself beyond its shape -----------------------
# A list was a black box, and inside a frame it was worse: a nested tibble
# reports its group count as its row count, so a table of two thousand
# observations was catalogued as twenty rows.
sv(list(train = data.frame(x = 1:10, y = 1:10), test = data.frame(x = 1:5, y = 1:5)),
   "list_of_frames")
sv(list(a = 1:5, b = letters[1:5], c = as.numeric(1:5)), "list_parallel")
sv(list(l1 = list(l2 = list(l3 = list(l4 = 1:3)))), "list_deep")
if (requireNamespace("tidyr", quietly = TRUE) && requireNamespace("tibble", quietly = TRUE)) {
  nested <- tidyr::nest(tibble::tibble(g = rep(c("a", "b", "c"), times = c(1, 4, 10)),
                                       v = 1:15, w = as.numeric(1:15)), data = c(v, w))
  sv(nested, "nested_tibble")
}
# Things written down beside the values: the order a factor declares, the zone
# a moment is in, and what a number is a number of.
sv(factor(c("lo", "hi", "mid"), levels = c("lo", "mid", "hi"), ordered = TRUE), "ord_factor")
sv(as.POSIXct(c("2020-01-01", "2020-06-01"), tz = "America/Chicago"), "tz_stamps")
if (requireNamespace("units", quietly = TRUE)) {
  sv(units::set_units(c(1.5, 2.5, 3.5), "m/s"), "unit_speeds")
}
# A table of counts is unreadable without its margin labels.
sv(table(treat = c("A", "A", "B"), outcome = c("hit", "miss", "hit")), "labelled_table")
if (requireNamespace("data.table", quietly = TRUE)) {
  kdt <- data.table::data.table(a = 1:6, b = 6:1, v = as.numeric(1:6))
  data.table::setkey(kdt, a); data.table::setindex(kdt, b)
  sv(kdt, "keyed_dt")
}
if (requireNamespace("dplyr", quietly = TRUE)) {
  sv(dplyr::group_by(tibble::tibble(g = c("x", "x", "y"), v = 1:3), g), "grouped_tbl")
}

# Spread, order and where the gaps fall. A series that starts late is a
# different thing from one that is patchy throughout, and both had the same
# description: a count of missing values.
sv(data.frame(
  spread  = c(2, 4, 4, 4, 5, 5, 7, 9),
  rising  = 1:8,
  falling = 8:1,
  wholes  = as.numeric(1:8),
  late    = c(NA, NA, NA, 1, 2, 3, 4, 5),
  patchy  = c(1, NA, 3, NA, 5, NA, 7, NA)
), "stats_kinds")

# How far apart the observations are, and whether they are evenly so. A daily
# series with a fortnight missing and one observed daily throughout have the
# same start, end and count.
if (requireNamespace("zoo", quietly = TRUE)) {
  sv(zoo::zoo(1:10, as.Date("2020-01-01") + 0:9), "z_regular")
  sv(zoo::zoo(1:6, as.Date("2020-01-01") + c(0, 1, 2, 16, 17, 18)), "z_gappy")
}
# A symmetric matrix keeps one triangle, so its stored count is roughly half
# its non-zeros: every off-diagonal entry stands for two.
if (requireNamespace("Matrix", quietly = TRUE)) {
  sv(Matrix::sparseMatrix(i = c(1, 2, 3, 3), j = c(1, 1, 1, 3), x = c(5, 1, 2, 7),
                          dims = c(4, 4), symmetric = TRUE), "sym_sparse")
}

# The shape of a distribution, which a range and a middle do not give: a long
# right tail with one value far out, and a column of codes where the commonest
# value is most of the column.
sv(data.frame(
  # 8 sits outside the usual fences but inside a wider one, so the width of
  # the fence is testable rather than merely the presence of an extreme.
  skewed = as.numeric(c(rep(1, 40), rep(2, 30), rep(3, 28), 8, 50)),
  codes  = as.numeric(c(rep(1, 60), rep(2, 30), rep(3, 10)))
), "shape_kinds")

# Tables do not always sit directly in the slots. Cross-validation folds keep
# theirs a level down, and counting only direct slots reported a dataset of
# forty-three rows as holding none.
sv(list(fold1 = list(train = data.frame(x = 1:10), test = data.frame(x = 1:5)),
        fold2 = list(train = data.frame(x = 1:20), test = data.frame(x = 1:8))),
   "nested_folds")
# Slots holding nothing: a NULL and an empty vector count towards the length
# and are not there.
sv(list(a = NULL, b = NA, c = 1:3, d = character(0)), "holey_list")
# A list with no names at all, and one whose slots are S4 objects that state
# their size in a slot rather than by their length.
sv(list(1:3, letters[1:2], as.numeric(1:4)), "unnamed_list")
if (requireNamespace("Matrix", quietly = TRUE)) {
  sv(list(m1 = Matrix::Diagonal(3), m2 = Matrix::Diagonal(4)), "list_of_s4")
}

# Where the variation lies. rows_vary has a level per row and near-identical
# columns; cols_vary is its transpose, so the two summaries have to swap. A
# summary over every cell as one vector is identical for both, which is the
# point: it cannot tell them apart.
rv <- matrix(rep(c(1, 50, 100), each = 4), nrow = 3, byrow = TRUE) +
      matrix(rep(c(0.1, -0.1, 0.2, -0.2), 3), nrow = 3, byrow = TRUE)
sv(rv, "rows_vary")
sv(t(rv), "cols_vary")
# A margin mean has to skip the missing values rather than be poisoned by one.
sv(matrix(c(1, NA, 3, 4, 5, 6, NA, 8, 9, 10, 11, 12), nrow = 3), "margin_na")
# One row is not a margin worth summarising.
sv(matrix(1:4, nrow = 1), "margin_thin")
# Folds of deliberately different sizes, so a per-element count cannot be
# recovered from the total and the element count.
sv(list(fold1 = list(data.frame(x = 1:50)),
        fold2 = list(data.frame(x = 1:150)),
        fold3 = list(data.frame(x = 1:7))), "uneven_folds")

# A summary must not contradict its own bounds. Rounding to six decimal places
# turned every statistic here into zero while the minimum and maximum, which
# are not rounded, stayed where they were.
sv(data.frame(pico = c(1e-12, 2e-12, 3e-12), nano = c(4e-9, 5e-9, 6e-9)), "tiny_values")

# NA and NaN are both is.na() and are not the same finding: one is a value
# nobody recorded, the other is one a calculation could not produce. R tells
# them apart by the payload it puts in NA_real_, and so must anything reading
# the bytes. Infinities counted by sign rather than flagged at each end.
sv(data.frame(
  mixed    = c(NA, NA, NaN, 0/0, 0/0, 1, 2, 3, 4, Inf, Inf, -Inf),
  just_na  = c(1, NA, 3, NA, 5, NA, 7, 8, 9, 10, 11, 12),
  just_nan = c(1, NaN, 3, NaN, 5, 6, 7, 8, 9, 10, 11, 12),
  runs     = c(NA, NA, NA, 4, 5, 6, 7, 8, 9, NA, NA, NA)), "na_and_nan")

# Width plus one type is a matrix wearing a data.frame coat. Six hundred
# columns of numbers described one at a time is the same sentence six hundred
# times, and what a reader wants from an object like this is where the
# variation lies rather than six hundred near-identical means.
set.seed(7)
wide_homogeneous <- as.data.frame(matrix(round(rnorm(600 * 6, 10, 3), 3), nrow = 6, ncol = 600))
wide_homogeneous[[3]][2] <- NA
sv(wide_homogeneous, "wide_homogeneous")
# The same numbers stored as the matrix they are. The whole-object treatment a
# uniform frame gets has to be the treatment a matrix gets and not a second one
# that drifts away from it, which is what comparing these two says.
sv(as.matrix(wide_homogeneous), "wide_as_matrix")
# The same width with the types mixed, where every column is its own variable
# and dropping any of them loses one nobody can recover.
wide_heterogeneous <- wide_homogeneous
for (j in seq(1, 600, by = 3)) wide_heterogeneous[[j]] <- paste0("s", seq_len(6) + j)
for (j in seq(2, 600, by = 3)) wide_heterogeneous[[j]] <- rep(c(TRUE, FALSE), 3)
sv(wide_heterogeneous, "wide_heterogeneous")
# One type and few columns. The per-column means are the description here, so
# uniformity on its own must cost nothing.
sv(as.data.frame(matrix((1:40) + 0.5, nrow = 5, ncol = 8)), "narrow_homogeneous")
# An sf frame past the cap. The geometry column is the one column the record's
# own extent and projection are lifted off, so a depth that dropped it would
# take the object's identity with it rather than a statistic.
wide_sf <- wide_homogeneous
wide_sf$geom <- list(c(1, 2), c(3, 4), c(5, 6), c(7, 8), c(9, 10), c(11, 12))
attr(wide_sf$geom, "class") <- c("sfc_POINT", "sfc")
attr(wide_sf$geom, "crs") <- structure(list(input = "EPSG:4326", wkt = "GEOGCRS[\"WGS 84\"]"), class = "crs")
attr(wide_sf$geom, "bbox") <- structure(c(xmin = 1, ymin = 2, xmax = 11, ymax = 12), class = "bbox")
sv(wide_sf, "wide_sf")
# Longer than the reader will hold. Every column is past the cell cap, so no
# value is read at all and the record can list the columns without describing
# any of them, which is a different shape of list from the one a wide frame
# gets. Nine million rows cost a few hundred bytes here because R stores 1:n as
# a compact sequence and writes the state rather than the numbers.
sv(data.frame(a = 1:9000000L, b = 1:9000000L, c = 1:9000000L), "unread_columns")
