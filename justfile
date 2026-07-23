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
      build) cargo build -p elgrep --offline {{args}} ;; \
      check) cargo check -p elgrep --offline {{args}} ;; \
      test) cargo test -p elgrep --offline {{args}} ;; \
      clippy) cargo clippy -p elgrep --all-targets --offline -- -D warnings {{args}} ;; \
      bench) cargo bench -p elgrep --bench index --offline {{args}} ;; \
      release) RUSTFLAGS="-C target-cpu=native" cargo build -p elgrep --release --offline {{args}} ;; \
      run) cargo run -p elgrep --offline -- {{args}} ;; \
      *) printf '%s\n' 'usage: just eg build|check|test|clippy|bench|release|run [args...]'; exit 2 ;; \
    esac
