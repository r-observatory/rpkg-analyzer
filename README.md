# rpkg-analyzer

A static analyzer for R package source trees. Given one extracted package
directory, it emits newline-delimited JSON (NDJSON) on stdout and reads nothing
else. It is a pure function of the source bytes: no R, no build, no network, so
it runs identically on a current release or any historical version pulled from
an archive.

## Usage

```
rpkg-analyzer <package_dir>        # NDJSON to stdout
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
| `dataset` | dataset under `data/` or `R/sysdata.rda` | `name`, `file`, `format`, `compression`, `class`, `nrow`, `ncol`, `columns[]` (`name`, `type`, `n_missing`, `n_unique`, `col_min`/`col_max`, `col_fp`), `schema_fp`, `shape_fp`, `content_fp`, `row_sketch`, `confidence` |

The `dataset` records come from reading R's serialization format (`.rda`/`.rds`/`.RData`) and delimited text (`.csv`/`.tab`) directly, with no R: the reader walks each object for its class, dimensions, and per-column names/types/factor levels, then makes one bounded value pass for the missing/unique/range profile and the fingerprints. The fingerprints identify the same or similar datasets across packages: `content_fp` matches identical data regardless of packaging, `schema_fp` matches the same columns and types, and `row_sketch` (a bottom-k row hash) estimates row overlap for subsets and near-duplicates. S4 objects report their class (and dimensions for common Bioconductor containers); `.R` data scripts are flagged as needing R.

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
