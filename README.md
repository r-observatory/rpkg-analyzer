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
| `function` | top-level R function | `name`, `exported`, `file`, `line`, `loc`, `n_params`, `cyclocomp` |
| `dcf` | package version | every DESCRIPTION field verbatim (the catch-all) |

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
`net_betweenness_mean/median/max` (Brandes betweenness). Definitions are our own. `n_native_calls` is the R-to-compiled edge signal.

### Static-check-style signals

`n_native_calls` (`.Call`/`.C`/`.Fortran`/`.External`), `n_library_calls_in_r`
(`library`/`require` in package code), `n_internal_calls` (`.Internal`),
`n_global_assign` (`<<-`).

### SystemRequirements

`uses_openmp`, `sysreq_has_java`, `sysreq_has_gnu_make`, `sysreq_components`.

### Tests

`n_test_cases` and `testing_frameworks`, recognizing testthat (`test_that`/`it`),
tinytest (`expect_*`), RUnit (`test.*` functions / `check*`), and testit
(`assert`).

### NAMESPACE intelligence

`s3_methods`, `export_classes`, `export_methods`, `export_patterns`,
`import_from`, `imports_whole`, `use_dyn_lib`.
