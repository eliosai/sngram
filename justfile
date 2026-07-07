set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    just --list

# fp suite against a corpus directory
suite corpus=".":
    cd {{corpus}} && {{justfile_directory()}}/target/release/eg --bench

# fp suite against the guard corpus
guard:
    just suite ~/ripos/gitoxide

eg action="help" *args:
    case "{{action}}" in \
      build) cargo build -p eg --offline {{args}} ;; \
      check) cargo check -p eg --offline {{args}} ;; \
      test) cargo test -p eg --offline {{args}} ;; \
      clippy) cargo clippy -p eg --all-targets --offline -- -D warnings {{args}} ;; \
      bench) cargo bench -p eg --bench index --offline {{args}} ;; \
      release) RUSTFLAGS="-C target-cpu=native" cargo build -p eg --release --offline {{args}} ;; \
      run) cargo run -p eg --offline -- {{args}} ;; \
      *) printf '%s\n' 'usage: just eg build|check|test|clippy|bench|release|run [args...]'; exit 2 ;; \
    esac
