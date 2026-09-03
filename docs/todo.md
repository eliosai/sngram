# Open work

Only unfinished work belongs here. Git history carries completed plans and measurements.

## Before 1.0

- gate the one-line comment rule: `just comment-scan` counts 521 comments over one line, 194 in
  `crates/lib` and 327 in `crates/eg`, and each needs a rewrite that keeps its facts
- bring the 22 files in `crates/eg` over 400 lines under the limit, most of them ripgrep's flag
  definitions and the postings and manifest code
- publish the Python package from the release workflow with PyPI trusted publishing
- give elgrep's daemon tests a wait that does not lose the 30 ms vouch race under load, then drop
  the nextest retries
- decide whether python's `WeightTable.matrix()` keeps a consumer, and drop `matrix()` from both
  surfaces if not
