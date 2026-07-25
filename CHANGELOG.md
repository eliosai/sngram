# Changelog

## 0.7.0

Indexes built by earlier versions are rebuilt on first use: the postings
schema moved from 16 to 20.

### Correctness

- Indexed search covers the bytes before a file's first NUL. A file with a
  single NUL near its end used to be dropped whole, so a 3.4 MB protobuf
  whose only NUL sat 82 bytes from the end never matched.
- Binary files are searched deterministically. Whether a match near the end
  of such a file was reported used to depend on how wide a directory tree
  the search covered.
- Indexed output prints the path spelling the query asked for. `eg PATTERN ../`
  returned nothing at all when an index existed.
- Tiny binary prefixes are decided from bytes held in the index rather than
  forced into every candidate set.
- Anchored patterns carry line-start and line-end requirements into the plan.

### Performance

- Index build is about five times faster: scan work is split by bytes rather
  than file count, and the postings merge runs in parallel.
- `sngram::scan` runs at about 208 MiB/s on code, up from 90.
- Query plan construction drops from 65 ms to 4.4 ms on its worst case.

### Training

- The corpus is The Stack v3, read directly from parquet with file content
  inline. The separate object store fetch is gone.
- Sampling takes whole repositories with their natural file mix. Vendored
  files are counted.
- A full pass over 15.9 TB of decoded source takes about 13 hours at roughly
  340 MB/s, holding about 2.2 GB of memory.

## Known issues

- `--heading` output from an indexed search separates file blocks with two
  blank lines where a scan uses one, and `-A`/`-B` repeat the `--` marker
  the same way. Each candidate file is printed into its own buffer while
  the printer keeps its separator state, so both the trailing and the
  leading separator are written. No match is added or lost, and every
  other output mode is byte identical between the two paths.
