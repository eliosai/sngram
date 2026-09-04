---
name: rust-testing
description: Rust test ownership, test boundaries, shared testkits, provider emulators, Prism, fault injection and project testing libraries. Use for any Rust test, fixture, test container or test-only dependency.
metadata:
  origin: ECC
---

# Rust Testing

Read `tdd` before changing behavior. This skill owns Rust test placement, boundary selection, fixtures, infrastructure and testing libraries.

## Choose The Smallest Honest Boundary

Use the smallest test which crosses every boundary the behavior requires:

- Unit tests exercise local logic inside `#[cfg(test)] mod tests` in the file which owns that logic
- Crate integration tests exercise the public crate API from `crates/<name>/tests`
- Component tests run the production-shaped binary and call its public network boundary
- Infrastructure tests use the real pinned PostgreSQL, NATS, Valkey, RustFS or other service
- Provider contract tests send real requests to a pinned emulator or Prism
- Resilience tests inject provider outcomes through an emulator control API and transport failures through a network fault proxy
- Compile-fail tests belong to the proc-macro crate which owns the diagnostic
- Load tests prove correctness under pressure; CodSpeed Divan benchmarks measure performance

Do not make one test prove several layers when smaller tests can give each behavior one clear owner.

## Dependency Fidelity

Use external dependencies in this order:

1. The real production implementation in a hermetic container
2. A pinned stateful emulator maintained or patched by the project
3. Prism serving the pinned OpenAPI specification when no emulator exists and state is not under test

Direct Elios APIs always run as production-shaped binaries. Never replace a local API with an HTTP mock.

A provider emulator exposes the provider API and a separate test control API. Tests seed state, inject documented outcomes and inspect observed requests through the control API. Product code receives only the provider endpoint.

Prism proves request paths, methods, headers, bodies and schema acceptance. It does not prove state transitions, events, idempotency, retries or provider lifecycle behavior.

Use a network fault proxy for latency, connection loss, resets and partial delivery. Do not encode transport failures as provider responses.

Do not add `wiremock` or `mockall`.

## Testkit Ownership

`crates/testkit` owns reusable infrastructure mechanics, not domain fixtures or complete application stacks.

### Placement

Place test code at the narrowest level which owns it:

- One integration-test file uses it: keep it in that file
- Several integration-test files in one crate use it: place it in `crates/<name>/tests/fixtures`
- Several crates need the same primitive with the same semantics: move that primitive into `crates/testkit`

Promote the smallest reusable primitive. Repeated lines alone do not prove a shared abstraction.

### Structure

Group testkit modules by boundary type and then by concrete infrastructure domain:

```text
crates/testkit/src/
├── lib.rs
├── fleet/
│   ├── mod.rs
│   ├── inventory.rs
│   └── owner.rs
├── infra/
│   ├── mod.rs
│   ├── nats.rs
│   ├── postgres.rs
│   ├── rustfs.rs
│   └── valkey.rs
├── provider/
│   ├── mod.rs
│   └── prism.rs
├── fault/
│   ├── mod.rs
│   └── network.rs
└── resource/
    ├── mod.rs
    ├── identity.rs
    └── temporary.rs
```

`fleet` owns suite-scoped startup, inventory and teardown. `infra` runs real pinned production dependencies. `provider` runs reusable provider-contract tools and promoted emulators. `fault` controls transport failures. `resource` owns isolated names, paths and temporary resources.

A provider-specific emulator used by only one domain remains in that crate's fixtures. Promote it only when another crate needs the same provider behavior.

### Admission Rules

Every testkit addition must satisfy all of these rules:

- It owns infrastructure or test execution mechanics rather than business behavior
- It has at least two crate consumers, unless it is repository-wide by construction
- It preserves an acyclic dependency graph
- It exposes one typed resource owner with narrow setup, action and observation methods
- It owns every process, container, network and temporary resource it starts
- It reads image and provider pins from `infra/<domain>` rather than copying them
- It hides `testcontainers` and `testcontainers-modules` from domain tests
- It contains no domain expectations, sensible defaults or product scenarios
- It uses no modules named `common`, `helpers`, `support`, `misc` or `utils`
- It has tests for its own lifecycle, readiness and cleanup behavior

Tests own behavioral assertions. Testkit may fail while establishing infrastructure, but it never decides whether domain behavior is correct.

Do not add domain-named testkit features or production features which expose internals to tests.

## Container Fleet Lifecycle

Container-backed tests reuse one fleet for the selected suite run. Never start the same service once per test, and never keep the default fleet across runs.

Nextest runs each test in a separate process. The outer test command owns the fleet and passes endpoints to test processes. Tests own isolated logical namespaces, never container handles.

- Start only the services required by the selected suite
- Give every test a unique database or schema, subject prefix, bucket, key prefix, emulator namespace and temporary directory as applicable
- Never reset shared state while another test can use it
- Run destructive lifecycle, restart and partition tests in an exclusive fleet
- Keep images and build caches; remove containers, networks and volumes

Label every runtime resource with the testkit, suite and run ID. Use a unique Compose project name when Compose creates the fleet.

Before startup, remove stale resources belonging to dead testkit runs. On normal exit, failure, interruption and cancellation, remove every resource owned by the current run. Then inventory the run and fail if anything remains. The next invocation must recover resources left by an untrappable crash.

Test cleanup must be scoped by exact ownership labels. Never run a broad Docker prune or remove an unlabeled resource.

Never extend a resource lifetime with `std::mem::forget`, `Box::leak`, a detached task or an equivalent leak. The outer command retains the fleet owner until verified teardown completes.

Use nextest test groups and explicit CPU, memory and process limits to bound heavy suites. Enforce a hard fleet-size limit and refuse startup when stale resources exceed it. Use health or protocol readiness checks, never sleeps.

Test the fleet lifecycle itself for successful cleanup, partial startup, interruption, repeated cleanup and refusal to touch foreign resources.

## Unit Tests

Every unit test lives in `#[cfg(test)] mod tests` inside the `.rs` file which owns the tested logic.

Never create `src/**/tests.rs`, `src/**/tests/`, or a production-named source file containing only tests. A local test helper stays inside the same test module.

Use plain `#[test]` for one scenario. Use `rstest` when several cases exercise the same behavior. Assert the exact returned value or typed error.

```rust
#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::{BatchSizeError, validate_batch_size};

    #[rstest]
    #[case(0, Err(BatchSizeError::Empty))]
    #[case(1, Ok(1))]
    #[case(500, Ok(500))]
    #[case(
        501,
        Err(BatchSizeError::TooLarge {
            requested: 501,
            maximum: 500,
        }),
    )]
    fn validates_batch_size(
        #[case] requested: usize,
        #[case] expected: Result<usize, BatchSizeError>,
    ) {
        assert_eq!(validate_batch_size(requested), expected);
    }
}
```

Test every logical clause, boundary and typed failure the module owns. Every assertion must observe behavior and be able to fail.

## Strict Assertions

A test accepts one exact behavior and rejects every observable alternative. A permissive assertion can make broken behavior look correct.

- Compare complete values, errors, collections and protocol messages whenever the contract makes them deterministic
- Assert both inclusion and exclusion; a test named for an exclusion proves the excluded value is absent
- Assert exact collection length and members, not `contains`, `any`, a non-empty check or a minimum count
- Assert the exact request method, path, query, headers, body and call count for a network write
- Assert exact error variants and fields, not `is_err`, an error class or a wildcard match
- Assert exact success values, not `is_ok`, `is_some` or the presence of one field
- Never discard the result or observation the test exists to judge
- Use ranges, sets of accepted values or redaction only where the contract itself permits variation, then assert every invariant and boundary it defines
- Control timestamps, identifiers, ordering and randomness when possible rather than weakening the assertion

Before accepting a test, change the guarded behavior to a plausible wrong result and watch that test fail for the intended reason. Restore the behavior and watch it pass.

## Error Tests

Assert the complete success value or exact typed error. Never stop at `is_ok`, `is_err`, a broad wildcard match or an error class.

Use `pretty_assertions::assert_eq` when the error supports equality:

```rust
#[test]
fn reports_the_invalid_config_location() {
    let error = parse_config("}{invalid")
        .expect_err("the invalid document must be refused");

    assert_eq!(
        error,
        ConfigError::Parse {
            line: 1,
            column: 1,
            message: "expected a key".to_owned(),
        },
    );
}
```

When an error cannot support equality, destructure one exact variant and every relevant field without `..`:

```rust
#[test]
fn preserves_the_provider_refusal() {
    let error = send_request()
        .expect_err("the refused request must fail");

    match error {
        ProviderError::Refused {
            status,
            code,
            retry_after,
        } => {
            assert_eq!((status, code, retry_after), (429, "rate_limit", Some(30)));
        }
        other => panic!("expected a provider refusal, received {other:?}"),
    }
}
```

Production failures return `Result`. Do not use `#[should_panic]` to turn an ordinary failure path into a passing test. Panic assertions are valid only for an API whose explicit contract is to panic; Foundations production APIs have no such contract.

## Integration Tests

Integration tests live in `crates/<name>/tests` and exercise only the public crate API. Never call internal handlers or expose production internals for integration tests.

Use one top-level integration-test target for the crate, then group tests into semantic modules. The directory tree should represent the crate's behavior and failure domains.

```text
crates/billing/tests/
├── billing.rs
├── fixtures/
│   ├── mod.rs
│   └── billing.rs
├── purchase/
│   ├── mod.rs
│   ├── idempotency.rs
│   └── pricing.rs
├── provider/
│   ├── mod.rs
│   ├── metronome.rs
│   └── stripe.rs
└── recovery/
    ├── mod.rs
    ├── lost_response.rs
    └── retry.rs
```

The top-level target declares each semantic area through normal module resolution:

```rust
mod fixtures;
mod provider;
mod purchase;
mod recovery;
```

Every directory module contains `mod.rs`. Never pair `x.rs` with `x/`, use numbered test files, or assemble tests with `#[path = "..."]`.

Keep a helper inside its semantic test module until another area needs it. Fixtures shared across areas live in `tests/fixtures`.

Tests own expectations. Fixtures provide setup, actions and observations while owning only their per-test logical resources, never fleet container handles.

```rust
use pretty_assertions::assert_eq;

use crate::fixtures::BillingFixture;

#[tokio::test]
async fn records_a_completed_purchase_once() {
    let fixture = BillingFixture::acquire().await;
    let purchase = fixture.purchase().await;
    let recorded = fixture.recorded_purchases().await;

    assert_eq!(recorded, vec![purchase]);
}
```

Split a crate into multiple integration targets only when the targets require different harnesses, features or execution profiles.

## Async Tests

Read `rust-async-patterns` before testing asynchronous behavior.

Every async test asserts the exact task result and final observable state. A test does not pass merely because the future completed, timed out or avoided a panic.

Use `tokio::time::timeout` as a deadlock guard around the operation, then assert the complete inner result:

```rust
use std::time::Duration;

use pretty_assertions::assert_eq;

#[tokio::test]
async fn returns_the_complete_batch() {
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fetch_batch(),
    )
    .await
    .expect("the batch must complete before the test deadline");

    assert_eq!(
        result,
        Ok(vec![
            Event::new("event_one", 1),
            Event::new("event_two", 2),
        ]),
    );
}
```

Async tests follow these rules:

- Never use `sleep` to coordinate tasks
- Synchronize with `Barrier`, `Notify`, channels or an observed public state
- Await every spawned task and assert its exact returned value
- Assert final persisted and emitted state after cancellation or failure
- Assert that forbidden writes, events and retries are absent
- Use paused Tokio time for timer logic when the crate enables `test-util`
- Use real time only as a generous test deadline
- Test both sides of every race by controlling the ordering explicitly
- Never leave a detached task running after the test ends

A cancellation test proves the cancellation result, cleanup and absence of work after cancellation. A concurrency test proves every returned result, the exact final state and the absence of duplicate effects.

## Testing Libraries

Use `rstest` for local case composition, `proptest` for generated boundaries, `pretty_assertions` for complete value comparisons and `insta` only for stable structured contracts.

## Property-Based Testing with `proptest`

### Basic Property Tests

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn encode_decode_roundtrip(input in ".*") {
        let encoded = encode(&input);
        let decoded = decode(&encoded);

        prop_assert_eq!(decoded, Ok(input));
    }

    #[test]
    fn sort_preserves_length(mut vec in prop::collection::vec(any::<i32>(), 0..100)) {
        let original_len = vec.len();
        vec.sort();

        prop_assert_eq!(vec.len(), original_len);
    }

    #[test]
    fn sort_produces_ordered_output(mut vec in prop::collection::vec(any::<i32>(), 0..100)) {
        vec.sort();
        for window in vec.windows(2) {
            prop_assert!(window[0] <= window[1]);
        }
    }
}
```

### Custom Strategies

```rust
use proptest::prelude::*;

fn valid_email() -> impl Strategy<Value = String> {
    ("[a-z]{1,10}", "[a-z]{1,5}")
        .prop_map(|(user, domain)| format!("{user}@{domain}.com"))
}

proptest! {
    #[test]
    fn accepts_valid_emails(email in valid_email()) {
        prop_assert_eq!(validate_email(&email), Ok(()));
    }
}
```

## Snapshot Testing with `insta`

Use `insta` only when the complete representation of a stable structured contract matters. Prefer `pretty_assertions` for small values and ordinary domain results.

Convert runtime values into a deliberate snapshot type before asserting:

```rust
#[test]
fn snapshots_the_provider_contract() {
    let response = provider_response();
    let snapshot = ProviderContractSnapshot::from(response);

    insta::assert_json_snapshot!("provider_contract", snapshot);
}
```

The snapshot type excludes secrets and replaces ports, timestamps, identifiers and local paths with stable markers. Review the entire snapshot diff before accepting it.

## Benchmarks

Foundations uses CodSpeed Divan, never Criterion. Read `codspeed-setup-harness` before adding a benchmark and `codspeed-optimize` before optimizing one.

Keep `harness = false` in the owning crate manifest. Put setup outside the measured closure, assert the returned value inside the benchmark and clean up through the public boundary. Service benchmarks use CodSpeed walltime because they include container and network I/O.

## Project Commands

Cargo-nextest runs every non-doctest Rust test through the repository recipes:

- `just unit-test` runs workspace library tests
- `just test <crate>` runs one crate
- `just test-ci <crate>` runs one crate under the CI profile
- `just test-doc` runs doctests

Use these recipes instead of raw `cargo test`; they own the required profiles, wrappers and infrastructure lifecycle.
