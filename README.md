# rpkg-analyzer

A static analyzer for R package source trees. Given one extracted package
directory, it emits newline-delimited JSON (NDJSON) on stdout and reads nothing
else. It is a pure function of the source bytes: no R, no build, no network, so
it runs identically on a current release or any historical version pulled from
an archive.

## Usage

```
rpkg-analyzer <package_dir>        # NDJSON to stdout
rpkg-analyzer --datasets <dir>     # the dataset records only, nothing else
rpkg-analyzer --version            # the build that would write them
rpkg-analyzer --sexp  <file>       # debug: print the tree-sitter parse tree
rpkg-analyzer --kinds <file>       # debug: node-kind histogram
```

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
| `dataset` | object shipped under `data/`, `R/sysdata.rda`, or `inst/extdata` | `name`, `file`, `origin_dir`, `format`, `compression`, `class`, `kind`, `nrow`, `ncol`, `columns[]`, `schema_fp`, `shape_fp`, `content_fp`, `row_sketch`, `confidence`; see [Dataset records](#dataset-records) |

The summary record also carries `analyzer_version`, the build that wrote it. A consumer storing these results needs it to tell rows it has already collected from rows a newer build would describe differently, which is what makes a rescan decidable rather than a guess.

The `dataset` records are described in full under [Dataset records](#dataset-records) below.

The `function` records are the graph's nodes (R and compiled alike, tagged by
`lang`, each with file/line/loc) and the `call_edge` records its edges, so the
full labeled call graph can be reconstructed and stored or drawn. The summary
carries only the aggregate network stats.

These are not five separate graphs but one cross-language graph. The `native`
edges go from an R function to the compiled function it invokes, bridging the R
call graph to the C/C++/Rust/Fortran ones. To unite them, tag each node by the
edge's `graph` (an R node on `r`; on `native`, `from` is R and `to` is compiled;
both endpoints compiled on `c`/`rust`/`fortran`) so a name shared across
languages stays distinct. On data.table that is one connected structure of about
1,700 edges (969 R, 141 R-to-C, 593 C-internal).

The `dcf` record preserves the full parsed DESCRIPTION so any field can be
promoted to a metric later without re-reading the source.

## Dataset records

One `dataset` record per object a package ships. The values are read straight out of R's serialization format (`.rda`, `.rds`, `.RData`) or out of delimited text, with no R runtime and no evaluation of package code, so a version pulled from an archive reads the same way a current release does.

### Where they come from

Three places, and `origin_dir` says which one a record came from.

`data/` is the loadable catalogue, so only the extensions `data()` itself dispatches on are opened, in `data()`'s own precedence order. A package that ships one name twice (`mtcars.rda` beside `mtcars.csv`) gets one record, for the file `data()` would actually load, because the other copy is not reachable by that name. An `.rds` here is not opened at all: `data()` cannot load one, and in an installed tree `data/Rdata.rds` is the lazy-load index rather than a dataset, which once put a fingerprinted dataset called `Rdata` in the catalogue for every package.

`R/sysdata.rda` holds objects the package uses internally. These get records with `internal` true and `origin_dir` `sysdata`: they are real data the package carries, but nothing outside the package can load them by name.

`inst/extdata` (or `extdata`, in an installed tree) is covered too, under the rules in [inst/extdata](#instextdata) below.

`file` is the path relative to the package root. `title` is the title of the Rd help page whose alias matches the dataset name, when the package documents it, so a catalogue row can say what the data is rather than only what it is called.

### How much of it was read

Every record carries `confidence`, and this is the whole vocabulary:

- `exact`: the object was read as itself. Its class, its dimensions, its columns and their values were all seen, and the fingerprints below cover the data.
- `degraded`: something is described, but not everything. A class with no reader of its own arrives as its class name plus whatever its attributes give up; a delimited text file has its column types inferred rather than declared; an object past the cell cap keeps its structure and drops the value pass; a file that would not parse carries the reader's own message. `notes` says which of these happened.
- `needs_r`: an `.R` script under `data/`. Only R can evaluate one, so nothing but the file itself is described.

A `degraded` record is not a failure to be filtered out. It is the difference between what was measured and what exists, stated on the row, and a consumer that treats it as missing data throws away most of what Bioconductor ships.

### What a record carries

Provenance and format: `name`, `file`, `origin_dir` (`data`, `sysdata`, `extdata`), `internal`, `title`, `format` (`rda`, `rds`, `csv`, `tsv`, `tab`, `psv`, `txt`, `script`), `format_version` (R's serialization version, 1, 2 or 3), `compression` (`none`, `gzip`, `bzip2`, `xz`), `compressed_bytes`.

Class and shape: `class` (the class vector as written, slash-joined), `kind` (`data.frame`, `matrix`, `array`, `vector`, `list`, `table`, `graph`, `object`, or the class name for a container with a reader of its own), `nrow`, `ncol`, `dim`, `n_dim`, `length`, `n_cells`, `has_rownames`, `has_dimnames`, `dimnames` (per margin, the labels and how many there are), `frame_class` (which flavour of data frame), `object_system` (`S4`, `R6`, `RefClass`, `S7`) and `s4_package`.

`columns[]` is one object per column: `name`, `type`, `n_missing`, `n_unique` and `col_fp` always, and then whatever the type supports. Numeric columns add `col_min`, `col_max`, `mean`, `median`, `q1`, `q3`, `sd`, `skewness`, `kurtosis`, `n_zero`, `p_zero`, `is_integer_valued`, `sort_order` (`ascending`, `descending`, `unsorted`), and `n_outliers` with `n_outliers_low` and `n_outliers_high` counted against the fences a boxplot would draw. Character columns add `min_nchar`, `max_nchar`, `n_blank`, `n_empty`, `mode_value` and `mode_share`. Factors add `levels`, `n_levels`, `is_ordered` and `level_counts`. Doubles add `n_infinite`, `n_infinite_pos`, `n_infinite_neg`, `min_infinite`, `max_infinite` and `n_nan`, because R's two spellings of missing are two different facts. Missingness has a position as well as a count: `n_missing_leading`, `n_missing_trailing` and `max_missing_run` separate a column that starts late from one that is unreliable throughout.

An object with values but no columns (a vector, a matrix, an array, a sparse matrix) gets the same summary lifted to the top level, with `summary_over` naming what it was taken over: `cells` for a dense grid, `stored values` for a sparse one, where a mean over the cells and a mean over the stored values are different numbers. `n_missing_total` is the object-wide count.

Matrices say how they are held rather than only how big they are: `matrix_shape` (`general`, `symmetric`, `triangular`, `diagonal`, `positive-definite`), `matrix_storage` (`dense`, `packed`, `column-compressed`, `row-compressed`, `triplet`, `diagonal`), `matrix_value_type` (`double`, `logical`, `integer`, `complex`, or `pattern` for a matrix that stores only where its entries are), `matrix_uplo` (which triangle a symmetric or triangular matrix keeps), `matrix_diag`, and for a sparse one `n_stored` and `density`. A grid of two dimensions or more also carries a six number summary of each margin's means, `row_mean_min` through `row_mean_max` with `row_mean_sd`, and the same for `col_mean_*`: a grid whose row means spread widely while its column means barely move is saying where its structure is, which one summary over every cell cannot say.

Containers with a reader of their own report what makes them that kind of thing. Time series and indexed series: `ts_start`, `ts_end`, `ts_frequency`, `ts_span`, and for zoo, xts, tsibble and the Rmetrics series `index_class`, `index_start`, `index_end`, `index_n`, `index_delta`, `index_regular`, `index_span`, `index_n_gaps`, `index_max_gap`, `index_tz`. Spatial objects (sf, sp, and the sfc geometry columns inside a frame): `is_spatial`, `bbox`, `crs_epsg`, `crs_input`, `crs_wkt`, `geom_type`, `n_geometries`, `geom_dimension`. Raster and terra grids: `n_layers`, `layer_names`, `layer_min`, `layer_max`, `resolution`, `nodata_value`, `in_memory`. Graphs: `n_vertices`, `n_edges`, `directed`. data.table: `dt_key` and `dt_indices`, the closest thing such a table has to a primary key. Grouped dplyr frames: `is_grouped`, `group_vars`, `n_groups`. Lists: `elements`, `element_names`, `element_class` or `element_classes`, `element_lens`, `element_len_min`, `element_len_max`, `element_len_total`, `max_depth`, `n_empty_slots` (a list of four with two of them NULL is not the list its length suggests), and for a list of frames the inner `inner_names`, `inner_ncol`, `inner_nrow_total`. Bioconductor S4 containers report their dimensions from the assay or element-metadata slot they keep them in.

Whatever no reader consumes by name is still reported rather than dropped, because dropping it would say it was not there. `label`, `comment` and `units` come through as text when they hold text, and `attrs_other` names every remaining attribute with the kind and the length of each, plus its values or its two ends when they are short enough to be worth having.

A delimited file whose header does not look like what its extension claims says so: `delimiter_looks_like` and `delimiter_would_give_ncol` report the separator that would have produced a different table, which is more use to a reader than one column named after the whole header line.

### Fingerprints

Four keys, and each answers a different question about sameness.

`content_fp` is blake3 over the per-column fingerprints in column order, where a column's own fingerprint covers its type and its values and never its name. So `content_fp` matches the same data packaged under different column names, and stops matching when a type changes or the columns are reordered. That is deliberate in both directions: renamed headers over the same numbers are the same data, while the same numbers read as character instead of numeric are not.

`schema_fp` is over `name:type` for every column in order, so it matches the same columns of the same types under the same names, whatever the rows hold.

`shape_fp` is over the column types in order with the names dropped, so it matches a table built the same way out of different data. It says nothing about how many rows there are.

`row_sketch` is a bottom-32 KMV sketch: the 32 smallest distinct row hashes, each a mix of that row's per-column cell hashes, sorted and hex encoded. Comparing two sketches estimates Jaccard similarity and containment, which is how a subset or a near-duplicate is found. Exact copies are what `content_fp` is for.

The fingerprints appear only when the whole object could be profiled. One column the reader cannot hash (a generated sequence too long to hold, whose length alone would make two different sequences one dataset) suppresses all four rather than fingerprinting a partial read, so a fingerprint that is present is a fingerprint over everything.

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

- The per-column detail list stops at 512 columns, while `ncol` still reports the true width and the fingerprints still cover every column, so what is lost is the description of the tail rather than the fact of it.
- `level_counts` stops at 50 levels and sets `level_counts_truncated`; margin labels stop at 50 and set `labels_truncated`.
- An object past the cell cap keeps its structure and skips the value pass, which is `degraded` with `notes` of `value scan skipped (size cap)`, and it carries no fingerprints.
- Help-page titles are read from at most 4000 Rd files per package, each up to 1 MiB.

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

### Object systems

`n_s3_methods`, `n_s4_classes`, `n_s4_generics`, `n_s4_methods`, `n_r6_classes`,
`n_rc_classes`, `n_s7_classes`, `uses_usemethod`.

### Deprecation

`n_deprecated_functions` (R functions calling `.Deprecated`/`.Defunct` or
`lifecycle::deprecate_*`).

### Documentation and source signals

`has_recognized_repo`, `repo_host`, `repo_url` (normalized to `host/owner/repo`),
`has_website`, `help_pages_with_examples`, `examples_coverage`, `news_up_to_date`
(latest NEWS version equals the package version).

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

`n_test_cases` and `testing_frameworks`. Cases are counted for testthat
(`test_that`/`describe`/`it`), unittest (`ok`/`ok_group`), tinytest (`expect_*`),
RUnit (`test.*` functions / `check*`), and testit (`assert`). unitizer (by its
`tests/unitizer/` directory), svUnit, quickcheck, and hedgehog are detected as
frameworks (via layout or `Suggests`) but not case-counted, since they are
expression- or property-based rather than discrete-case. Assertion libraries
(assertthat, checkmate) and mocking libraries (mockery, mockr) are intentionally
not treated as frameworks.

### NAMESPACE intelligence

`s3_methods`, `export_classes`, `export_methods`, `export_patterns`,
`import_from`, `imports_whole`, `use_dyn_lib`.

## Feedback

Found a bug, a wrong number, or a missing package? Report it at [r-observatory/feedback](https://github.com/r-observatory/feedback/issues/new/choose). All feedback about R Observatory, the site, the data, and the pipelines, is tracked in one place.
