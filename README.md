# rpkg-analyzer

A static analyzer for R package source trees. Given one extracted package
directory, it emits newline-delimited JSON (NDJSON) on stdout and reads nothing
else. It is a pure function of the source bytes: no R, no build, no network, so
it runs identically on a current release or any historical version pulled from
an archive.

## Usage

```
rpkg-analyzer <package_dir> --input-kind release|git   # NDJSON to stdout
rpkg-analyzer --datasets <dir>                         # the dataset records only, nothing else
rpkg-analyzer --version                                # the build that would write them
rpkg-analyzer --explain <dir> --input-kind release|git # debug: per-file decisions
rpkg-analyzer --sexp  <file>                           # debug: print the tree-sitter parse tree
rpkg-analyzer --kinds <file>                           # debug: node-kind histogram
```

`--input-kind` is required in the analysis mode and follows the directory. `release` means the
directory is a built release (a CRAN tarball, or the github.com/cran mirror of one). `git`
means it is a git branch that R CMD build has not filtered yet (a Bioconductor release branch).
A missing flag, or any other value, exits with status 2, a usage line on stderr and no records.

A run that cannot get the memory it asks for stops there. It ends with a status that is not 0, in most cases 134 (aborted), and writes no statistics line, so the records printed before it stopped are not a whole result.

A run has 64 MiB of stack whatever stack limit it is started under, and reserves the address space for it before it reads anything. That is 64 MiB more address space than the data needs and no more resident memory, since the stack is only touched as far as an object nests. Under an address-space limit with no room for it the run is aborted before it prints anything. The data reader goes one call deeper for each level of a nested object, so the stack is what bounds the depth it reads, and how much a level takes depends on the platform and the compiler. As measured, a list nested about 49,900 deep, a saved image of as many objects and a compiled call of about 232,900 arguments are read on Linux aarch64, and about 74,800 and 322,600 on macOS arm64. A file nested deeper aborts the run with the message of a stack that ran out.

## Environment

None of these changes a record. Each is read only in the analysis mode, and an older build ignores it.

| Variable | Effect |
|---|---|
| `RPKG_ANALYZER_STATS=<file>` | After the last record, append one JSON line to the file: `build`, `ms` (the whole run), `ms_compiled`, `ms_r`, `ms_tests`, `ms_data`, `ms_other`, then `compiled`, `r`, `tests` and `data`, each `{"files": n, "hits": n}`, then `cache_errors` and `verify_mismatch`. Unset or empty writes nothing, and a file that cannot be written is ignored. |
| `RPKG_ANALYZER_CACHE_DIR=<dir>` | Keep what each compiled file under `src/` yields (counts, names, call-graph nodes) in `<dir>/src/`, keyed by the file's bytes and extension, so a later run that meets the same bytes reads them instead of parsing. Files under 2,048 bytes skip it. An entry written by another build, or damaged, is a miss; a directory that cannot be read or written only misses. Unset or empty means no cache. |
| `RPKG_ANALYZER_CACHE_VERIFY=1` | With a cache, parse on every hit as well, use the parsed result, and count each disagreement in `verify_mismatch`. |

## Output contract

One `summary` record per run, followed by intermediate records:

| `rec` | one per | key fields |
|---|---|---|
| `summary` | package version | all scalar/aggregate metrics below |
| `dependency` | declared dependency | `package` |
| `export` | exported symbol | `symbol` |
| `function` | R or compiled function | `lang` (`r`/`c`/`cpp`/`rust`/`fortran`), `name`, `file`, `line`, `loc`; R nodes also `exported`, `n_params`, `cyclocomp` |
| `call_edge` | one call-graph edge | `graph` (`r`/`native`/`c`/`rust`/`fortran`), `from`, `to` |
| `dcf` | package version | every DESCRIPTION field verbatim (the catch-all) |
| `release_notes` | package version, only when its NEWS has a section for it | `package_version`, `news_file`, `release_notes_source` (`news_md`, `news_rd`, `news_plain`), `release_notes` (at most 16,384 bytes, cut at a character boundary), `release_notes_truncated` |
| `dataset` | object shipped under `data/`, `R/sysdata.rda`, or `inst/extdata` | `name`, `file`, `origin_dir`, `format`, `compression`, `class`, `kind`, `nrow`, `ncol`, `column_detail`, `columns[]`, `schema_fp`, `shape_fp`, `content_fp`, `row_sketch`, `confidence`; see [Dataset records](#dataset-records) |

The summary record also carries `analyzer_version`, the build that wrote it. A consumer storing these results needs it to tell rows it has already collected from rows a newer build would describe differently, which is what makes a rescan decidable rather than a guess.

The `dataset` records are described in full under [Dataset records](#dataset-records) below.

The `function` records are the graph's nodes (R and compiled alike, tagged by
`lang`, each with file/line/loc) and the `call_edge` records its edges, so the
full labeled call graph can be reconstructed and stored or drawn. The summary
carries only the aggregate network stats.

From 0.5.1 the `call_edge` records of each graph come out in node order: by the position of `from` among that graph's `function` records, then of `to` (the `native` graph, whose ends sit in two graphs, in name order). So two runs over one tree print the same bytes, and the betweenness figures no longer move in their last digit between runs.

These are not five separate graphs but one cross-language graph. The `native`
edges go from an R function to the compiled function it invokes, bridging the R
call graph to the C/C++/Rust/Fortran ones. To unite them, tag each node by the
edge's `graph` (an R node on `r`; on `native`, `from` is R and `to` is compiled;
both endpoints compiled on `c`/`rust`/`fortran`) so a name shared across
languages stays distinct. On data.table that is one connected structure of about
1,700 edges (969 R, 141 R-to-C, 593 C-internal).

The `dcf` record preserves the full parsed DESCRIPTION so any field can be
promoted to a metric later without re-reading the source.

## Input kind and what NULL means

Every summary carries `input_kind`, the contract it was computed under. A row with no
`input_kind` was written by 0.4.0 or earlier, or by a pipeline's R fallback, and keeps the
older meaning.

Release-content columns (every column except the seven below) describe what the release
contains. On `release` input they read the tree as given. On `git` input they read the files
R CMD build would keep: the tree after the 18 default exclude patterns of R 4.6.1's
`tools:::get_exclude_patterns()`, every non-empty line of `.Rbuildignore` (split the way
`readLines` splits, not trimmed, `#` lines used as patterns, compiled case-insensitively), and
the structural exclusions of `tools:::.build_packages`. An excluded directory takes its
contents. For these columns 0 means "not in this release", and NULL means the analyzer could
not tell: the file could not be parsed, or on `git` input the `.Rbuildignore` exists and could
not be read. In that last case every release-content column is NULL and only the `dcf` record
follows the summary. A few detail columns are NULL when their parent says there is nothing to
describe: `news_file` and `release_notes_source` (parent `news_present`), the `citation_*`
columns (parent `has_citation`), the `rd_example_pages_*` breakdowns (parent
`rd_example_pages`), `examples_coverage_fn` and its basis (an empty denominator), `n_test_units`,
`test_unit`, `n_test_blocks`, `n_test_blocks_cran_skipped` and `tests_gated_not_cran` (parent
`test_framework_primary`), and `vignette_eval_gated` (parent `has_vignettes`). `changelog_file`
has no parent: NULL means the release has no ChangeLog, CHANGELOG or CHANGES file.

Repository-only columns are `ci_present`, `ci_type`, `ci_matrix_breadth`, `ci_pr_gated`,
`has_pkgdown`, `has_code_of_conduct` and `has_contributing_guide`. On `git` input they read the
whole branch and are true or false. On `release` input a presence column is true when the
release itself carries a matching file and NULL otherwise, never false, because a release
cannot show what its repository keeps; `ci_type`, `ci_matrix_breadth` and `ci_pr_gated` are NULL
unless `ci_present` is true.

`build_ignored` names the release items a `git` branch leaves out of its build, as a JSON array
in this order: `README.md`, `README.Rmd`, `README.qmd`, `NEWS.md`, `NEWS`, `tests`, `vignettes`,
`vignettes/articles`, `_pkgdown.yml`, `pkgdown`, `docs`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`,
`data-raw`, `.github`, `inst/NEWS.Rd`, `inst/CITATION`, `inst/REFERENCES.bib`, `man`. A file item
is listed when it is in the tree and excluded; a directory item when it is excluded or every
file under it is; `_pkgdown.yml` stands for any pkgdown config path, `CODE_OF_CONDUCT.md` and
`CONTRIBUTING.md` for any root spelling below; `vignettes` is also listed when every vignette
source in it is excluded, whatever else survives. It is NULL on `release` input and `[]` when
nothing is left out. `build_ignore_bad_lines` counts `.Rbuildignore` lines that do not compile
and are skipped; NULL on `release` input or without a `.Rbuildignore`.

## Dataset records

One `dataset` record per object a package ships. The values are read straight out of R's serialization format (`.rda`, `.rds`, `.RData`) or out of delimited text, with no R runtime and no evaluation of package code, so a version pulled from an archive reads the same way a current release does.

### Where they come from

Three places, and `origin_dir` says which one a record came from.

`data/` is the loadable catalogue, so only the extensions `data()` itself dispatches on are opened, in `data()`'s own precedence order. A package that ships one name twice (`mtcars.rda` beside `mtcars.csv`) gets one record, for the file `data()` would actually load, because the other copy is not reachable by that name. An `.rds` here is not opened at all: `data()` cannot load one, and in an installed tree `data/Rdata.rds` is the lazy-load index rather than a dataset, which once put a fingerprinted dataset called `Rdata` in the catalogue for every package.

A `.tsv` or a `.dat` under `data/` gets no record at all, for the same reason: `data()` does not dispatch on those extensions, so a row for one would put a dataset in the catalogue that nobody can reach. Of the text formats it does dispatch on, `.csv` is read with a semicolon and `.tab` and `.txt` with whitespace, which is what `data()` itself does and not what the extension usually means elsewhere.

`R/sysdata.rda` holds objects the package uses internally. These get records with `internal` true and `origin_dir` `sysdata`: they are real data the package carries, but nothing outside the package can load them by name.

`inst/extdata` (or `extdata`, in an installed tree) is covered too, under the rules in [inst/extdata](#instextdata) below.

`file` is the path relative to the package root. `title` is the title of the Rd help page whose alias matches the dataset name, when the package documents it, so a catalogue row can say what the data is rather than only what it is called. For a `data/` dataset with such a page, `dataset_doc_format` is 1 when the page has a `\format` block and 0 when not, and `dataset_doc_source` is its `\source` as text (at most 4,096 bytes) or null when the page states none; with no page both are null, and so are `sysdata` and `extdata` records. On `git` input files the build leaves out are not read.

### How much of it was read

Every record carries `confidence`, and this is the whole vocabulary:

- `exact`: the object was read as itself. Its class, its dimensions, its columns and their values were all seen, and the fingerprints below cover the data.
- `degraded`: something is described, but not everything. A class with no reader of its own arrives as its class name plus whatever its attributes give up; a delimited text file has its column types inferred rather than declared; an object holding a vector past the cell cap keeps its structure and drops the value pass; a file that would not parse carries the reader's own message. `notes` says which of these happened.
- `needs_r`: an `.R` script under `data/`. Only R can evaluate one, so nothing but the file itself is described.

A `degraded` record is not a failure to be filtered out. It is the difference between what was measured and what exists, stated on the row, and a consumer that treats it as missing data throws away most of what Bioconductor ships.

### What a record carries

Provenance and format: `name`, `file`, `origin_dir` (`data`, `sysdata`, `extdata`), `internal`, `title`, `format` (`rda`, `rds`, `csv`, `tsv`, `tab`, `psv`, `txt`, `script`), `format_version` (R's serialization version, 1, 2 or 3), `compression` (`none`, `gzip`, `bzip2`, `xz`), `compressed_bytes`.

Class and shape: `class` (the class vector as written, slash-joined), `kind` (`data.frame`, `matrix`, `array`, `vector`, `list`, `table`, `graph`, `object`, or the class name for a container with a reader of its own), `nrow`, `ncol`, `dim`, `n_dim`, `length`, `n_cells`, `has_rownames`, `has_dimnames`, `dimnames` (per margin, the labels and how many there are), `frame_class` (which flavour of data frame), `object_system` (`S4`, `R6`, `RefClass`, `S7`) and `s4_package`.

`columns[]` is one object per column: `name`, `type` and `col_fp` on every column, `n_missing` on every column but `complex`, `n_unique` on every column but `complex` and `raw`, and then whatever the type supports. Those two are the ones whose elements this reader does not keep. The whole vector is hashed on the way past instead, so every cell of one carries the same digest and a distinct count taken off those digests would say 1 however many different values are in there. It never looks at a value of either, which is also why it never finds one missing: on a `complex` column that is a count it cannot take, and three NAs in ten came back as none, so the field is left off. A `raw` vector has no missing value to find, R has none of that kind, so its 0 is a fact about the type and stays. The same exceptions apply at `reduced` depth below. `type` is drawn from a closed list: `logical`, `integer`, `numeric`, `character`, `factor`, `Date`, `POSIXct`, `list`, `raw`, `complex`, and `unknown` for a value this reader holds no representation of. The first three of those are decided by what the column is made of, `factor`, `Date` and `POSIXct` by its class, so a factor reports `factor` and never `integer`.

Past those, every field is conditional, on the type of the column and on what its values turn out to be, and a consumer building a column specification off this file should treat each of them as one that can be absent. The conditions, with how often each held over 2,151 profiled columns of real packages and fixtures:

- Numeric columns (`integer`, `numeric`, and `Date` and `POSIXct`, which are numbers underneath) add `col_min`, `col_max`, `mean`, `n_zero` and `p_zero` where at least one value is finite; `median`, `q1` and `q3` beside them where the column is also no longer than 5000000 values, past which holding a second sorted copy of it to take quantiles from stops being worth the memory; `sd` and `sort_order` (`ascending`, `descending`, `constant`, `unsorted`) where at least two values are finite; `skewness` and `kurtosis` where at least three are and the spread is not zero, which is 1,749 of 1,752 numeric columns; `n_outliers` with `n_outliers_low` and `n_outliers_high`, counted against the fences a boxplot would draw, where the quartiles are apart and something falls outside them, which is 820 of 1,752; `mode_value` with `mode_share` where the column has between 2 and 20 distinct values and the commonest of them is at least a fifth of the column; and `is_integer_valued`, only ever `true`, on a double column whose values are all whole, which is 37 of 1,752. On `integer` columns `sort_order` held 245 times in 248.
- Logical columns add `n_true` and `n_false`, and carry no `col_min` or `col_max`: a range of 0 to 1 says nothing that the counts do not.
- Character columns add `min_nchar` and `max_nchar` where at least one value is present, and `n_blank` only where some value is the empty string, which was 2 of 33 character columns.
- Doubles add `n_infinite` only where the column holds an infinity, and then `n_infinite_pos`, `n_infinite_neg`, `max_infinite` and `min_infinite` for whichever end runs off; and `n_nan` only where some missing value is a NaN arithmetic produced rather than an NA nobody recorded. `n_missing` counts R's NA and a NaN alike, because `is.na()` does, and `n_nan` is the part of it that is the second kind.
- Missingness has a position as well as a count, and only where there is any: `max_missing_run` where the column has a missing value at all, with `n_missing_leading` and `n_missing_trailing` where a run reaches the respective end. A column with nothing missing carries none of the three, which separates a column that starts late from one that is unreliable throughout without writing three zeroes against every complete column in the corpus.
- Factors add `is_factor`, which at this depth is written only where it is true, and `n_levels` and `levels`, `level_counts` where any level is used, `is_ordered` only where the factor is ordered, and `levels_truncated` or `level_counts_truncated` where there are more than 50 to list. `is_factor` and `n_levels` are the two that survive to `structural` depth, since neither needs a value read, and `structural` is the one depth that writes `is_factor` when it is false. Neither survives to `reduced`.
- A column holding a matrix of its own adds `cell_nrow` and `cell_ncol`, which is why it holds more values than the frame has rows. A geometry column adds the spatial fields listed further down.
- Whatever was written beside the values comes through as well where it is there: `label`, `comment`, `units` and `attrs_other`.

So at `full` depth `name`, `type` and `col_fp` are on every column without exception, `n_missing` on every one but `complex`, and `n_unique` on every one but `complex` and `raw`. Nothing else is.

How much of that a record carries depends on how wide the object is, on what its columns are, and on whether the values were read at all, and `column_detail` is `full`, `reduced`, `none` or `structural` to say which. Width alone is not the test, because a frame is only repeating itself when its columns are also alike.

- `full`: every column and every statistic above. Anything up to 512 columns, whatever its columns hold. Ten numeric columns need their own means and ranges however alike their types are, so uniformity counts for nothing here.
- `reduced`: every column, carrying `name`, `type`, `n_missing` and `n_unique`, except that a `raw` column carries no `n_unique` and a `complex` column carries no `n_unique` and no `n_missing`, for the reasons given above, so those two carry three fields and two. A factor there carries neither `is_factor` nor `n_levels`, which `full` and `structural` both give it, and no column there carries `col_fp`. A frame past 512 columns whose columns are not all one type, and also one whose columns are all one type but which has too many cells to summarise whole. Each column there is a different variable, so the list keeps all of them and the statistics are what goes. One column can carry more: a geometry column keeps `is_geometry`, `geom_type`, `geom_dimension`, `n_geometries`, `n_empty`, `bbox`, `crs_input`, `crs_epsg` and `crs_wkt` as well, because the record's own extent and projection are read off that column and a frame has at most one of it. Those are the object's geography rather than a statistic about a column, and a frame that lost them would report as an ordinary table.
- `none`: no `columns[]` at all, and in its place the whole-object treatment a matrix gets, described below: `n_cells`, `n_missing_total`, the summary over every cell with `summary_over` of `cells`, and the `row_mean_*` and `col_mean_*` margin summaries. A frame past 512 columns whose columns are all one of `numeric`, `integer`, `logical` or `character` is a matrix wearing a data.frame coat, and a per-column profile of one is the word `numeric` nineteen thousand times over. A factor is not counted as one type for this: its levels are per-column vocabulary that no whole-object summary can carry. The list is given up only in exchange for that summary, so this depth is used only where the summary can be taken: a frame of one type whose cells run past the cell cap of 8000000, or whose columns are not all the same length, is `reduced` instead and keeps every column. The 8000000 here is the object's own cell count, `nrow` times `ncol`, because taking the summary means laying every column end to end into one vector. The same number bounds the length of a single column in the `structural` rule below, which is a different test on a different quantity. The one `none` record without a summary is a frame with no rows, where `nrow` and `n_cells` are both 0 and there are no values to describe; every other one carries `n_cells` and `summary_over` of `cells`.
- `structural`: every column, carrying `name`, `type` and `is_factor`, the last of those written whether it is true or false, which no other depth does, plus `n_levels` where the column declares levels. No count of anything, because no value was read. Such a record is `degraded` with `notes` of `value scan skipped (size cap)` and carries no `n_missing_total`. It does carry `content_fp`, and on a frame or a grid `schema_fp` and `shape_fp` as well, because the reader hashed the bytes of every column as it went past them, and it never carries a `row_sketch`, because no row was assembled to hash. A column the reader could not hash at all takes the other three with it, under [Fingerprints](#fingerprints) below. What puts a frame here is a column whose values were never materialised, which happens two ways. The first is length, and it is on three types only, `logical`, `integer` and `numeric`: one of those longer than 8000000 values has its length read and its values skipped over. That is the length of one column and not the cell count of the object, so a frame reaches this depth when `nrow` passes 8000000 at any width, and never because `nrow` times `ncol` does. A 600 by 14000 frame is 8400000 cells and every column in it is 600 long, so nothing is skipped and it is `reduced`. No other type is bounded by length. A `character` or a `list` column of that size runs into the item budget under [Known limits](#known-limits) first and takes the whole file with it, so a frame never reaches this depth on one of those; a `raw` or a `complex` column is read for its size at any length whatever, and a nine million element raw vector comes back `exact`. The second is type: a column the reader cannot materialise at all, which is a compact sequence (`1:n`, `seq_len(n)`) declaring more than 8000000 elements, or a value it holds no representation for, an S4 object for instance. `complex` and `raw` columns are not that case even though their elements are not kept either, because the whole vector is hashed on the way past and so they can be profiled. One such column is enough: a frame is at this depth or it is not, and the other columns come with it. Width has nothing to do with any of it, so a three column frame lands here if it is long enough.

`ncol` is the true width at all four depths. `content_fp`, `schema_fp`, `shape_fp` and `row_sketch` are taken over every column at `full`, `reduced` and `none` alike, so nothing about which of those was used changes which objects are the same data. A `structural` record has no `row_sketch` and carries the other three off the digests of its columns rather than off their values. Records with no column structure at all, a matrix or a vector or a list, carry no `column_detail`: there is no list for it to describe.

An object with values but no columns (a vector, a matrix, an array, a sparse matrix) gets the same summary lifted to the top level, with `summary_over` naming what it was taken over: `cells` for a dense grid, `stored values` for a sparse one, where a mean over the cells and a mean over the stored values are different numbers. Such a record also carries `type` for what its cells are, from the same vocabulary a column's `type` comes from, with `is_factor` and `n_levels` where they are a factor. Those are read off the object rather than off the summary, so a grid or a vector says what it holds whether or not a cell of it was read. `n_missing_total` is the object-wide count where there was one to take, summed over the values that were counted: a complex value has no cell the reader looked at, so an object made of them carries no total at all and a frame with a complex column among others carries the total over the rest.

Matrices say how they are held rather than only how big they are: `matrix_shape` (`general`, `symmetric`, `triangular`, `diagonal`, `positive-definite`), `matrix_storage` (`dense`, `packed`, `column-compressed`, `row-compressed`, `triplet`, `diagonal`), `matrix_value_type` (`double`, `logical`, `integer`, `complex`, or `pattern` for a matrix that stores only where its entries are), `matrix_uplo` (which triangle a symmetric or triangular matrix keeps), `matrix_diag`, and for a sparse one `n_stored` and `density`. A grid of two dimensions or more also carries a six number summary of each margin's means, `row_mean_min` through `row_mean_max` with `row_mean_sd`, and the same for `col_mean_*`: a grid whose row means spread widely while its column means barely move is saying where its structure is, which one summary over every cell cannot say.

Containers with a reader of their own report what makes them that kind of thing. Time series and indexed series: `ts_start`, `ts_end`, `ts_frequency`, `ts_span`, and for zoo, xts, tsibble and the Rmetrics series `index_class`, `index_start`, `index_end`, `index_n`, `index_delta`, `index_regular`, `index_span`, `index_n_gaps`, `index_max_gap`, `index_tz`. Spatial objects (sf, sp, and the sfc geometry columns inside a frame, where `is_geometry` marks the column): `is_spatial`, `bbox`, `crs_epsg`, `crs_input`, `crs_wkt`, `geom_type`, `n_geometries`, `geom_dimension`, and `n_empty` for geometries that are present and hold nothing, since they count as rows and draw as nothing. Raster and terra grids: `n_layers`, `layer_names`, `layer_min`, `layer_max`, `resolution`, `nodata_value`, `in_memory`. Graphs: `n_vertices`, `n_edges`, `directed`. data.table: `dt_key` and `dt_indices`, the closest thing such a table has to a primary key. Grouped dplyr frames: `is_grouped`, `group_vars`, `n_groups`. Lists: `elements`, `element_names`, `element_class` or `element_classes`, `element_lens`, `element_len_min`, `element_len_max`, `element_len_total`, `max_depth`, `n_empty_slots` (a list of four with two of them NULL is not the list its length suggests), and for a list of frames the inner `inner_names`, `inner_ncol`, `inner_nrow_total`. Bioconductor S4 containers report their dimensions from the assay or element-metadata slot they keep them in.

Whatever no reader consumes by name is still reported rather than dropped, because dropping it would say it was not there. `label`, `comment` and `units` come through as text when they hold text, and `attrs_other` names every remaining attribute with the kind and the length of each, plus its values or its two ends when they are short enough to be worth having.

A delimited file under `data/`, where the extension does promise a separator, says when its header does not look like what the extension claims: `delimiter_looks_like` and `delimiter_would_give_ncol` report the separator that would have produced a different table, which is more use to a reader than one column named after the whole header line.

### Fingerprints

Four keys, and each answers a different question about sameness.

`content_fp` is blake3 over the per-column fingerprints in column order, where a column's own fingerprint covers its type and its values and never its name. So `content_fp` matches the same data packaged under different column names, and stops matching when a type changes or the columns are reordered. That is deliberate in both directions: renamed headers over the same numbers are the same data, while the same numbers read as character instead of numeric are not.

`schema_fp` is over `name:type` for every column in order, so it matches the same columns of the same types under the same names, whatever the rows hold.

`shape_fp` is over the column types in order with the names dropped, so it matches a table built the same way out of different data. It says nothing about how many rows there are.

`row_sketch` is a bottom-32 KMV sketch: the 32 smallest distinct row hashes, each a mix of that row's per-column cell hashes, sorted and hex encoded. Comparing two sketches estimates Jaccard similarity and containment, which is how a subset or a near-duplicate is found. Exact copies are what `content_fp` is for.

The fingerprints appear only when everything they cover was read or hashed. One column the reader can do neither with, a generated sequence too long to hold whose length alone would make two different sequences one dataset, suppresses all four rather than fingerprinting a partial read, so a fingerprint that is present is a fingerprint over everything.

A value past the cell cap is not that case. Its cells were skipped, so nothing in it can be counted, but its bytes were hashed on the way past, and that hash stands in for the values in `content_fp`. `schema_fp` and `shape_fp` come with it, since names and types need no values at all. There is no `row_sketch`, because a sketch is built out of rows and no row was assembled. Such a `content_fp` is taken over digests rather than over cells, so it is comparable with another object the cap skipped and not with one the reader read through: an object is on one side of the cap or the other, and two copies of the same object are on the same side. Without it 34 objects in the published corpus had no identity at all, one of them a 149 million cell matrix, and a consumer that keys on `content_fp` drops a record that has none.

### inst/extdata

R points authors here for data files that are not datasets, and for many packages it is the only real data they carry. Nothing under it is loadable by name, so a record from here is not a catalogue entry: `origin_dir` is `extdata` and presenting one as something `data()` can reach is wrong.

`data()`'s conventions do not apply here either, so the extension promises nothing. A `.csv` under extdata is an ordinary comma separated file rather than the semicolon one `data()` expects, and the separator used is whichever of comma, tab, semicolon or pipe the header actually holds the most of.

What is opened: `.rds`, `.rda` and `.RData` as serialized objects, and `.csv`, `.tsv`, `.tab`, `.txt`, `.psv` and `.dat` as delimited text. Files above 8 MiB are skipped whatever their extension, because fingerprinting an 11 MB sequence file describes nothing a reader would ask about. Everything else, a spreadsheet or an image or a compressed archive, is left shut.

What is left shut is still counted. The summary record's `extdata` object reports `files`, `bytes` and `largest_bytes` over the whole directory, per-extension counts and byte totals split into `read` and `unread`, and up to 100 names of the unread ones, because `gz 1/11.0M` says a great deal less than `dm3_upstream2000.fa.gz`.

### Known limits

Two limits are silent: nothing on a record or in the inventory says the reader stopped, so any census built on them is a floor rather than a total.

- The extdata walk stops after 200 files, and it does not follow directories past depth 3. `inst/extdata/a/b` is read, `inst/extdata/a/b/c` is not.
- `attrs_other` sorts the leftover attributes alphabetically and keeps the first 24 entries. An object with more of them reports the alphabetical head, and says nothing about the tail.

The remaining bounds announce themselves, one way or another:

- Per-column detail stops being written column by column past 512 columns, and `column_detail` on the record says at which of the four depths it was written. No column is dropped: at `none` the whole list is replaced by a summary over every cell, and at the other three every column is listed. `ncol` reports the true width throughout.
- `level_counts` stops at 50 levels and sets `level_counts_truncated`; margin labels stop at 50 and set `labels_truncated`.
- A vector longer than 8000000 values has its length read and its values skipped, so an object built out of one keeps its structure and gets no value pass. On a frame that is `column_detail` of `structural`; on a grid or a bare vector, which have no column list to say it on, there is no summary and no `n_unique`. Either way the record is `degraded` with `notes` of `value scan skipped (size cap)`, counts nothing, and is identified by the hash the reader took of its bytes rather than by its values. The test is the length of one vector, not the cell count of the object: a 600 by 14000 frame is 8400000 cells and is fully measured, while a 3 by 9000000 frame is not measured at all. A column the reader cannot materialise for a reason other than length puts a frame at `structural` too, and the depth bullets above say which.
- A file is read on a budget of 5000000 items, where an item is one object on the wire: a vector, an attribute, and every element of a character vector or a list one apiece. Past it the read stops and the whole file is given up rather than the object that overran it, because the read is one pass through one byte stream. The record is `degraded` with `notes` of `item budget exceeded`, carrying the file's name, its path and its size on disk and nothing else: no `format`, no `class`, no dimensions, no columns and no fingerprints. An `.rda` holding several objects loses all of them under the one name. For a `character` or a `list` column this is the tighter of the two bounds by nearly a factor of two, so it is what actually fires: a frame of 8000001 rows of character comes back as that one line, while the same frame of doubles comes back at `structural` depth with every column named and typed. The same frame at 4000000 rows of character reads exactly.
- Help-page titles are read from at most 4000 Rd files per package, each up to 1 MiB.
- The extdata inventory splits files by whether their extension is one this reader opens, so a `.csv` above the 8 MiB parse limit is counted under `read` although no record was written for it. `read` is the count of files whose extension we open, not of files we opened.

## Static fields

All of the following are additive. They are static (a pure function of the
source), so they are collected once per version and never need recomputation.
Sizes on disk are the extracted-source footprint; download size, binary size,
installed size, and reverse dependencies live outside the analyzer.

### DESCRIPTION metadata

`config` (map of all `Config/*` fields), `date_publication`, `encoding`,
`priority`, `url`, `bug_reports`, `vignette_builder`, `os_type`, `type`,
`language`, `copyright`, `biocviews`, `needs_compilation`,
`has_additional_repositories`, `additional_repositories`.

### Author roles

`desc_n_aut`, `desc_n_cre`, `desc_n_ctb`, `desc_n_cph`, `desc_n_fnd`,
`desc_n_rev`, `desc_n_trl` (counts per role in `Authors@R`).

### Data sets and size

`num_data_files`, `data_size_total`, `data_size_median`, `data_files` (file
names), `datasets` (object names from `data/datalist` or file stems),
`total_source_size` (extracted bytes on disk).

`extdata` is an inventory of `inst/extdata` rather than a count: `files`, `bytes`, `largest_bytes`, per-extension counts and bytes split into `read` and `unread`, and the names of the files left shut. It is null when the package has no such directory. See [inst/extdata](#instextdata) for what read and unread mean.

### File and subdirectory counts

`num_vignettes`, `num_demos`, `files_r`, `files_src`, `files_tests`,
`files_inst` (`inst/include` only), `files_vignettes`.

### R functions

`n_fns_r`, `n_fns_r_exported`, `n_fns_r_not_exported`; `loc_per_fn_mean/median`
and its `_exported_` and `_internal_` splits; `npars_exported_mean/median`;
`cyclocomp_mean/median/max` (cyclomatic complexity); `doclines_per_fn_mean/median`
(Rd help-page length).

### Compiled functions

`n_fns_src` (total) with `n_fns_c`, `n_fns_cpp`, `n_fns_fortran`, `n_fns_rust`,
and `n_fns_per_file_src`. Counted with tree-sitter grammars for each language.

From 0.5.0 Rust sources (`.rs` under `src/`) count as compiled code. Besides `n_fns_rust`, `rnet_*` and the Rust `function` and `call_edge` records, which already read them, they now count toward `has_src`, `loc_src`, `loc_total`, `compiled_share` and `lang_breakdown` (which gains `rs`), the source scan behind `uses_openmp` (`#pragma omp` or `_OPENMP`), and the registration-table scan behind `n_native_edges`, `n_native_targets` and `native_resolution_rate`.

Vendored crates (`src/rust/vendor*/`) and cargo output (`target/` under `src/`) are not the package's own code. Files under them, in any language, are left out of `has_src`, `loc_src`, `loc_total`, `compiled_share`, `lang_breakdown`, `uses_openmp`, the registration-table scan, `n_fns_src`, `n_fns_per_file_src` and every per-language count (`n_fns_c`, `n_fns_cpp`, `n_fns_fortran`, `n_fns_rust`). Vendored Rust is also left out of `rnet_*` and the Rust `function` and `call_edge` records, while `cnet_*`, `fnet_*` and their records still read vendored C, C++ and Fortran. `files_src`, `blank_lines_src`, `comment_lines_src`, `rel_space_src`, `all_languages` and `language_categories` count every file as before, vendored ones included.

### Object systems

`n_s3_methods`, `n_s4_classes`, `n_s4_generics`, `n_s4_methods`, `n_r6_classes`,
`n_rc_classes`, `n_s7_classes`, `uses_usemethod`.

### Deprecation

`n_deprecated_functions` (R functions calling `.Deprecated`/`.Defunct` or
`lifecycle::deprecate_*`).

### Documentation and source signals

`has_recognized_repo`, `repo_host`, `repo_url` (normalized to `host/owner/repo`),
`help_pages_with_examples`, `examples_coverage`, `news_up_to_date`
(latest NEWS version equals the package version). The raw `URL` field is emitted
as `url`, so a consumer that wants a website flag derives it from that with its
own rule rather than inheriting one baked in here.

### Languages

`all_languages` (LOC by extension over the whole package, including `inst/`),
`language_categories` (rollup into code / documentation / data / config / web /
other), `has_web_assets`, `minified_asset_files`, `nexpr` (median expression
nodes per code line).

### Whitespace and style, per subdirectory

`blank_lines_r/src/tests`, `comment_lines_r/src/tests`, `rel_space_r/src/tests`
(blank / total), `indentation` (dominant space width, `-1` for tabs).

### Call network

Syntactic R call graph (an edge is one function calling another package
function). `net_n_nodes`, `net_n_edges`, `net_n_clusters`, `net_n_isolated`,
`net_node_degree_mean/median/max`, `net_n_terminal_nodes`,
`net_betweenness_mean/median/max` (Brandes betweenness). Definitions are our own.

The graph is also cross-language at the R-to-native boundary. Every `.Call`/`.C`/
`.Fortran`/`.External` site is resolved to the compiled function it invokes,
using the string routine name, a `C_`-stripped symbol, and the C registration
table (`R_CallMethodDef` and friends). `n_native_call_sites` (total),
`n_native_edges` (resolved), `n_native_targets` (distinct compiled functions
reached), `native_resolution_rate`. On data.table all 178 sites resolve to 108 C
functions.

The compiled side has its own internal call graphs, built the same way from
tree-sitter (no ctags/gtags). `cnet_*` (combined C/C++), `rnet_*` (Rust), and
`fnet_*` (Fortran, resolving both subroutine calls and function references),
each with `n_nodes`, `n_edges`, `n_clusters`, `node_degree_max`,
`betweenness_max`. On data.table `cnet` is 389 nodes / 593 edges; on quantreg
`fnet` is 58 nodes / 38 edges. So the full call graph (R-to-R, R-to-native, and
each compiled language internally) is covered without external tooling.

### Static-check-style signals

`n_native_calls` (`.Call`/`.C`/`.Fortran`/`.External`), `n_library_calls_in_r`
(`library`/`require` in package code), `n_internal_calls` (`.Internal`),
`n_global_assign` (`<<-`).

### SystemRequirements

`uses_openmp`, `sysreq_has_java`, `sysreq_has_gnu_make`, `sysreq_components`.

### Tests

`test_framework_primary` is the framework with the most test files among those the layout
names: testthat (`tests/testthat/` holds a file), tinytest (`inst/tinytest/`,
`tests/tinytest.R`, or `tinytest::` in a test file), RUnit (`inst/unitTests/`, or `RUnit::`,
`library(RUnit)`, `runTestSuite` or `BiocGenerics:::testPackage` in a test file), testit,
unitizer (`tests/unitizer/`), and scripts (`tests/*.R` with none of these). Ties go to that
order. `helper*` and `setup*` files under `tests/testthat/` never name a framework. It is
`none` only when `tests/`, `inst/tinytest/` and `inst/unitTests/` hold no file, and NULL when
they hold files but none names a framework. `test_frameworks_used` lists every framework with a
hit, `test_frameworks_declared` the ones named in Suggests. `n_test_units` counts the primary
framework's unit, named in `test_unit`: `test_block` (testthat `test_that()` and `it()`),
`expectation` (tinytest `expect_*()`, testit `assert()`), `test_function` (RUnit `test*`
functions), `script_file` (`tests/*.R`). `n_rout_save` counts `tests/*.Rout.save`.

For testthat, `n_test_blocks` counts blocks and `n_test_blocks_cran_skipped` those CRAN skips:
a block calling `skip_on_cran()`, `skip_if_offline()` or a top-level function in a helper,
setup file or `R/` that calls one of them; a block mentioning `NOT_CRAN`; a block inside an
`if` whose condition mentions `NOT_CRAN`; every block after a top-level skip call. When every
`test_check()` in `tests/*.R` sits inside a `NOT_CRAN` condition, every block is skipped and
`tests_gated_not_cran` is true. Deeper wrappers are missed, so the count is a lower bound.

`n_test_cases` and `testing_frameworks` keep their earlier rules: cases are counted for
testthat (`test_that`/`describe`/`it`), unittest (`ok`/`ok_group`), tinytest (`expect_*`),
RUnit (`test.*` functions / `check*`), and testit (`assert`), and frameworks are also taken
from `Suggests`. Both are kept for rows written before 0.5.0 and will be removed in a later
release.

### Help pages and examples

Help pages are `man/*.Rd` in any case, plus `man/unix/` and `man/windows/`; `man/macros` holds
macro definitions and is not counted. `n_help_topics`, `n_help_topics_internal`
(`\keyword{internal}`), `n_help_topics_data` (`\docType{data}`) and `n_help_topics_package`
(`\docType{package}` or an alias `<Package>-package`). `rd_example_pages` counts pages with an
`\examples` block, split into `rd_example_pages_run` (code outside `\dontrun`, `\donttest`,
`\dontshow` and `\testonly`; `\dontdiff` code runs), `rd_example_pages_donttest_only`,
`rd_example_pages_never_run` (code only in `\dontrun`, `\dontshow` or `\testonly`) and
`rd_example_pages_empty` (comments only). `rd_example_pages_conditional` counts run pages behind
roxygen's `@examplesIf` or an opening `if (` on `interactive()`, `requireNamespace()` or
`Sys.getenv()`. `examples_coverage_fn` is the share of pages aliasing an export that have
examples; with no plain export it uses pages that are not internal and do not document data,
the package, a class or methods, and `examples_coverage_fn_basis` says which (`exports`,
`not_internal`).

The older help-page columns read the same page set from 0.5.0, so each now counts lowercase `.rd` pages and leaves out `man/macros`: `examples_coverage`, `help_pages_with_examples`, `dontrun_example_ratio`, `references_coverage`, `value_doc_rate`, `undocumented_params_rate`, `roxygen_doc_coverage`, `doclines_per_fn_mean` and `doclines_per_fn_median`. Apart from the page set, `examples_coverage` and `dontrun_example_ratio` keep their earlier rules. `examples_coverage_fn` and the `rd_example_pages_*` counts supersede them; both are kept for rows written before 0.5.0 and will be removed in a later release.

### Citation file and references

`has_citation` is `inst/CITATION` in the release. The file is decoded as UTF-8, or Latin-1 when
DESCRIPTION declares it, parsed and never evaluated. `citation_read` is `literal`, `meta` (it
reads DESCRIPTION fields through `meta$`), `needs_eval` (any other call, assignment or free
symbol) or `parse_error`. `citation_n_entries`, `citation_bibtype` (JSON array, lowercase),
`citation_kind` (`publication` or `software_only`), `citation_dois` (JSON array, normalised)
and `citation_venue` (JSON array of `jss`, `rjournal`, `joss`, `other`; CRAN package and Zenodo
DOIs are no venue). An empty DOI list claims "no DOI" only on `literal` and `meta` reads; a
`needs_eval` read with none leaves both NULL. `has_rd_bibliography` is `inst/REFERENCES.bib` or
`inst/REFERENCES.R` in the release.

### README, NEWS and vignettes

`has_readme` looks for a root README.md, README.markdown, README.Rmd, README.qmd, README or
README.txt, in any case, and `readme_prose_length` reads the first one found. `news_file` is
the first of inst/NEWS.Rd, NEWS.md, inst/NEWS.md, NEWS and inst/NEWS in the release, the order
R's readers use; `news_present`, `news_up_to_date` and `news_structure_quality` read it (the
last is NULL for NEWS.Rd). `changelog_file` is the first of ChangeLog, CHANGELOG and CHANGES
at the root of the release; it has no parent column, and NULL means the release has none of
them. `release_notes_source` says which reader found a section for the analysed version, whose
text is the `release_notes` record.

A vignette source sits directly under `vignettes/`: `.Rnw` and `.Snw` count on their own, other
engines only with `\VignetteEngine{` in the text. `has_vignettes` and `num_vignettes` count them.
A vignette is static when building it runs no R code (a `.Rmd.orig` beside it, no R chunk,
every chunk `eval=FALSE`, a global `opts_chunk$set(eval = FALSE)` no chunk overrides, Quarto
`execute: eval: false`, `\SweaveOpts{eval=FALSE}`), and gated when its `eval=` names
`NOT_CRAN`, `Sys.getenv`, `identical`, `nzchar`, `requireNamespace` or `interactive`.
`vignette_dynamic` is true when any vignette is not static and `vignette_eval_gated` counts the
gated ones; both are NULL without vignettes.

### Repository practices, license and authors

`has_pkgdown` checks pkgdown 2.2.0's six config paths (`_pkgdown.yml`, `_pkgdown.yaml`, the same
two under `pkgdown/` and under `inst/`). `has_code_of_conduct` and `has_contributing_guide` check
the root and `.github/` for CODE_OF_CONDUCT.md, CODE_OF_CONDUCT, CODE_OF_CONDUCT.Rmd,
CODE_OF_CONDUCT.rst, code_of_conduct.md, Code_of_conduct.md, CODE-OF-CONDUCT.md, CONDUCT.md and
CONTRIBUTING.md, CONTRIBUTING, CONTRIBUTING.Rmd, CONTRIBUTING.rst, contributing.md,
Contributing.md, CONTRIBUTING.MD, the same lists vcs-signals uses.

`license_file_completeness`, for an MIT or BSD template license, is true when `YEAR:` and
`COPYRIGHT HOLDER:` both carry real values, or when the file is a full license text with
neither line.

Each entry of `authors` is `{"given":...,"family":...,"roles":[...]}` followed, only when the
version declares them, by `comment` (at most 120 characters, emails removed), `orcid` (bare,
check digit verified) and `ror`. A free-text Author field is split on commas, semicolons,
"and", "&" and "with contributions from", with bracketed roles and parenthesised notes kept
whole, emails removed, and notes and a leading "... by" moved into `comment`.

### Retired columns

`has_website` and `copyright_holder_declared` are no longer emitted from 0.5.0. The first read
arXiv, DOI and CRAN links as websites; the second was true for every legacy Author field.

### NAMESPACE intelligence

`s3_methods`, `export_classes`, `export_methods`, `export_patterns`,
`import_from`, `imports_whole`, `use_dyn_lib`.

## Feedback

Found a bug, a wrong number, or a missing package? Report it at [r-observatory/feedback](https://github.com/r-observatory/feedback/issues/new/choose). All feedback about R Observatory, the site, the data, and the pipelines, is tracked in one place.
