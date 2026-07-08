# eg

A high performance ripgrep alternative (up to 13x faster) using a sparse ngram index. `eg` carries rg's search path with the sparse ngram index integration. Indexing and index maintanaince are internalized via a background process.

## Installetion

```sh
cargo install elgrep
```

## Usage
The CLIs API is 1:1 identical to ripgrep's with a few flag additions.
By default a query will use the index, if it does not yet exist (first scan for a root directory) then we block and index (which may take some time) before the first query, after the inital indexing which is blocking the index is maintained in the background and synced for all fs mutation in scope. 

```sh
eg 'max_\w+_size' ~/src/linux
eg --no-index 'max_\w+_size' ~/src/linux   # plain scan, no index used
```

## Benchmarking
Using the `--bench` flag you can see the performance and gains made by the index. You can run `eg --bench` for a whole ~300 regex suite to run and see final summary data, or you can run `eg QUERY --flags --bench` for a bench for a certain invocation.

```sh
target/release/eg --bench 'max_\w+_size' ~/linux   # one query
cd ~/src/linux && target/release/eg --bench             # run the embeeded suite
```
