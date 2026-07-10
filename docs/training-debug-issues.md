# Training Debug Issues

This records the issues found while debugging the current `train/bins`
artifacts. It is intentionally limited to observed problems and evidence;
it does not describe fixes.

Inspected artifacts:

- `train/bins/train-events.jsonl`
- `train/bins/.checkpoint/state.json`
- `train/bins/100gb_weights.bin`
- `train/bins/500gb_weights.bin`
- `train/bins/1tb_weights.bin`
- `train/bins/2tb_weights.bin`

The run reached `2.047743729426 TB` durable counted bytes over about
`13h 38m`, with minted tables at `100gb`, `500gb`, `1tb`, and `2tb`.

## Issue: release mints include in-flight preview data

Minted tables are not based only on durable checkpointed counters. The mint
counter includes in-flight worker preview counters from active remote streams.

Observed mint composition:

| Mint | Mint bytes | Durable bytes | In-flight preview bytes |
| --- | ---: | ---: | ---: |
| `100gb` | `100.017 GB` | `23.690 GB` | `76.328 GB` |
| `500gb` | `500.002 GB` | `395.709 GB` | `104.293 GB` |
| `1tb` | `1000.017 GB` | `597.214 GB` | `402.802 GB` |
| `2tb` | `2000.014 GB` | `1934.310 GB` | `65.704 GB` |

The `1tb` table is especially affected: about `40.3%` of the table came from
preview bytes that were not durable at mint time. The `100gb` table is also
mostly preview data.

This makes release mints depend on data that may later be rejected by final
source caps.

## Issue: source caps are checked too late for parallel streams

The run accepted more bytes from some sources than the durable state could keep.
Those excess bytes were later logged as `cap_skip` events.

Total accounting:

- S3 accepted bytes: `2.367776899037 TB`
- Durable bytes: `2.047743729426 TB`
- `cap_skip` bytes: `320.033169611 GB`
- Accepted minus durable: `320.033169611 GB`

The accepted/durable gap exactly matches the skipped cap excess.

Largest accepted excess by source:

| Source | Accepted | Durable | Excess |
| --- | ---: | ---: | ---: |
| `stack-config-build-infra/XML` | `246.097 GB` | `90.000 GB` | `156.097 GB` |
| `stack-config-build-infra/JSON` | `183.067 GB` | `90.000 GB` | `93.067 GB` |
| `stack-config-build-infra/YAML` | `160.869 GB` | `90.000 GB` | `70.869 GB` |

The skipped bytes were not harmless for mints: some of them had already
contributed to preview counters before the final cap decision.

## Issue: early minted tables are distribution-skewed

The distribution contract is area based, but early mints did not represent the
final balanced corpus. They reflected whichever workers had large in-flight
remote streams at mint time.

Examples:

- `100gb` minted with only `23.690 GB` durable bytes.
- `1tb` minted with only `597.214 GB` durable bytes.
- `1tb` included `402.802 GB` of preview data, then the run later skipped
  `320.033 GB` of over-cap source data.

The consequence is that early tables are not just smaller samples; they are
samples with unstable source membership.

## Issue: final area totals are closer than source-level totals

At `2.047743729426 TB` durable bytes, the six area totals were not catastrophic,
but they hid large source-level skew.

| Area | Actual share | Target share | Delta |
| --- | ---: | ---: | ---: |
| Core programming | `39.26%` | `43.33%` | `-4.08 pp` |
| Docs / prose / markup | `20.68%` | `19.17%` | `+1.51 pp` |
| Config / build / infra | `14.22%` | `12.50%` | `+1.72 pp` |
| Web / UI / templates | `7.78%` | `10.00%` | `-2.22 pp` |
| Data / query / schema | `10.17%` | `8.33%` | `+1.84 pp` |
| Long-tail floor | `7.90%` | `6.67%` | `+1.23 pp` |

The area numbers can look acceptable while individual file types dominate their
areas.

## Issue: individual source types dominate their areas

The run did not keep file types evenly represented inside each area.

Largest within-area concentrations:

| Area | Dominant sources | Share of area |
| --- | --- | ---: |
| Docs / prose / markup | `Jupyter_Notebook`, `Text` | `65.2%` combined |
| Config / build / infra | `YAML`, `JSON`, `XML` | `92.7%` combined |
| Data / query / schema | `CSV`, `TSV`, `SQL` | `86.4%` combined |
| Web / UI / templates | `HTML`, `CSS` | `73.1%` combined |

Large durable source totals:

| Source | Durable bytes |
| --- | ---: |
| `stack-long-tail/default` | `161.735 GB` |
| `stack-core-programming/C++` | `143.967 GB` |
| `stack-docs-prose-markup/Jupyter_Notebook` | `138.000 GB` |
| `stack-docs-prose-markup/Text` | `138.000 GB` |
| `stack-core-programming/JavaScript` | `128.988 GB` |
| `stack-core-programming/Java` | `111.249 GB` |
| `stack-config-build-infra/YAML` | `90.000 GB` |
| `stack-config-build-infra/JSON` | `90.000 GB` |
| `stack-config-build-infra/XML` | `90.000 GB` |

The `Jupyter_Notebook` and `Text` source totals explain why the early docs
portion looked dominated by notebook-like JSON and prose rather than a broader
docs mix.

## Issue: the minted tables show numeric and structured-data bias

The minted weight tables contain unusually strong numeric and structured-data
pairs compared with the production table.

Representative rank comparison:

| Pair | Production rank | `100gb` | `500gb` | `1tb` | `2tb` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `"  "` | `16` | `12` | `16` | `12` | `11` |
| `"00"` | `338` | `242` | `67` | `95` | `96` |
| `"0."` | `1248` | `440` | `167` | `223` | `179` |
| `".0"` | `2078` | `608` | `191` | `264` | `248` |
| `"AA"` | `2648` | `362` | `552` | `598` | `924` |
| `"/*"` | `1124` | `7112` | `5868` | `9929` | `5385` |
| `"*/"` | `1124` | `7272` | `5974` | `10145` | `5491` |
| `"\t\t"` | `122` | `518` | `287` | `319` | `228` |

The current checkpoint's largest exact pair counts are also dominated by common
layout and numeric pairs:

| Pair | Count | Share |
| --- | ---: | ---: |
| `"  "` | `167.5B` | `8.18%` |
| `"00"` | `21.9B` | `1.07%` |
| `"\n "` | `20.5B` | `1.00%` |
| `", "` | `13.3B` | `0.65%` |
| `"0."` | `11.4B` | `0.56%` |
| `"\t\t"` | `8.7B` | `0.43%` |

The visible "weird character" concern is present, but it is not the dominant
signal. Whitespace/control pairs were about `6.30%` of exact pair counts, and
high-byte pairs were about `3.03%`. The larger issue visible in the tables is
over-representation of numeric, config, data, and notebook-shaped text.

## Issue: throughput slowed because of source mix, not clear rate limiting

The run reached high aggregate read rates earlier, then slowed materially near
the end.

Observed throughput:

- Overall durable average: about `41.7 MB/s`.
- Best 30 minute window: about `96.9 MB/s`.
- Hour 1: `300.2 GB`, about `83.4 MB/s`.
- Hour 4: `372.2 GB`, about `103.4 MB/s`.
- Hour 10: `77.9 GB`, about `21.6 MB/s`.
- Hour 12: `74.4 GB`, about `20.7 MB/s`.
- Last five minutes: `0 MB/s` durable and about `18.8 MB/s` total in-flight.

There is no strong evidence in these logs that public S3 rate limiting was the
primary bottleneck. The slowdown aligns with source mix, object shape, metadata
scanning, low-yield filters, and long-running slow sources.

## Issue: slow sources spend huge time scanning low-yield metadata

Several sources scanned many rows and objects but produced low accepted byte
rates.

Examples from aggregate `s3_batch` telemetry:

| Source | Accepted | Batch seconds | Accepted rate | Rows | Objects |
| --- | ---: | ---: | ---: | ---: | ---: |
| `stack-long-tail/default` | `161.735 GB` | `48245s` | `3.35 MB/s` | `202.4M` | `1.27M` |
| `stack-core-programming/Java` | `111.249 GB` | `45289s` | `2.46 MB/s` | `99.4M` | `2.69M` |
| `stack-core-programming/JavaScript` | `128.988 GB` | `39019s` | `3.31 MB/s` | `89.3M` | `1.84M` |
| `stack-core-programming/Python` | `70.458 GB` | `24949s` | `2.82 MB/s` | `47.3M` | `1.58M` |
| `stack-config-build-infra/Text` | `0.151 GB` | `23016s` | `0.01 MB/s` | `86.4M` | `1219` |
| `stack-data-query-schema/Text` | `0.239 GB` | `17284s` | `0.01 MB/s` | `51.9M` | `919` |

The `Text` sources are especially pathological: they scan tens of millions of
rows for almost no accepted bytes.

## Issue: long-tail default scans too broadly

`stack-long-tail/default` had the largest durable total, but its telemetry shows
large amounts of skipped metadata work:

- Accepted bytes: `161.735 GB`
- Scanned rows: `202.4M`
- Objects: `1.27M`
- Zero-byte batches: `54`
- Small-row skips: `177.39M`
- Bucket mismatch skips: `19.53M`

This indicates that the long-tail source is spending substantial time walking
rows that are later rejected by size or bucket classification.

## Issue: small-row filtering dominates several expensive scans

The largest slow sources spend most of their scan effort rejecting small rows.

Examples:

| Source | Scanned rows | Small-row skips |
| --- | ---: | ---: |
| `stack-long-tail/default` | `202.4M` | `177.39M` |
| `stack-core-programming/Java` | `99.4M` | `96.62M` |
| `stack-core-programming/JavaScript` | `89.3M` | `79.07M` |
| `stack-core-programming/Python` | `47.3M` | `45.64M` |
| `stack-config-build-infra/Text` | `86.4M` | `74.01M` |

This creates a disconnect between apparent concurrency and useful counting
throughput: many workers can be busy while accepted bytes barely move.

## Issue: RSS exceeds the intended soft limit

The training process has a `5 GB` soft memory limit, but observed RSS exceeded
that limit by a large margin.

Memory event evidence:

- `memory_trim` events: `1098`
- Peak pre-trim RSS: `8.46 GB`
- Peak post-trim RSS: `7.05 GB`
- Late trim example: `7.48 GB` to `5.73 GB`

Arrow memory was not the main contributor:

- Typical logged Arrow bytes at high RSS: about `8-14 MB`
- Maximum logged Arrow bytes: about `60.2 MB`

The soft cap currently records and trims after memory is already high. The logs
show that it does not keep process RSS at or below `5 GB`.

## Issue: telemetry does not fully explain mint contents

The event log is strong enough to show durable bytes, skipped cap bytes, source
progress, and mint totals. It is weaker for explaining exactly which in-flight
source bytes entered a specific mint.

Observed gaps:

- Mint events include aggregate in-flight preview bytes, but not a full
  per-source preview breakdown.
- Later `cap_skip` events identify rejected source bytes, but not whether each
  rejected byte had already influenced a particular minted table.
- Dashboard shard rows can show active shards, but not enough to reconstruct
  mint composition by area and source.

This makes postmortem analysis possible but unnecessarily indirect.

## Issue: dashboard speed can hide useful-vs-wasted work

The dashboard reports aggregate read speed and active shards, but the logs show
that busy shards can spend most of their time scanning rows that do not become
durable counted bytes.

Examples:

- Sources with millions of small-row skips can look active while useful bytes
  are low.
- Sources that later hit cap excess can look productive before their accepted
  bytes are removed from durable accounting.
- Late in the run, total in-flight bytes still moved around `18-20 MB/s` while
  durable bytes were flat.

This can make the live board look better or worse than actual durable corpus
progress, depending on which sources are active.

## Issue: current bins have mixed reliability

The bins are not equally trustworthy as representative training artifacts.

Observed reliability concerns:

- `100gb_weights.bin` is mostly in-flight preview data.
- `500gb_weights.bin` includes over `100 GB` of preview data.
- `1tb_weights.bin` includes over `400 GB` of preview data and overlaps with
  later cap-skipped source excess.
- `2tb_weights.bin` has much less preview contamination, but still reflects the
  source-level skew visible in durable totals.

The `2tb` table is the least affected by preview bytes, but it is not evidence
that the distribution problem is solved.
