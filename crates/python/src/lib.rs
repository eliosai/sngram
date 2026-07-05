//! Python bindings for sngram.
//!
//! Exposes the scan/query core and the training counters. The counting hot
//! path accepts Arrow data through the Arrow `PyCapsule` C interface
//! (`__arrow_c_stream__` / `__arrow_c_array__`), crosses the FFI once per
//! record batch, and counts with the GIL released — Python never touches a
//! row.

#![allow(
    clippy::needless_pass_by_value,
    reason = "pyo3 extracts owned argument values"
)]
#![allow(clippy::missing_const_for_fn, reason = "pymethods cannot be const")]

mod arrow_ffi;

use std::path::PathBuf;

use pyo3::exceptions::{PyIOError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use sngram::learn::{BigramCounter, LocalTally};
use std::io::Cursor;

use sngram::types::{QueryExpr, QueryPlan};
use sngram_types::{Gram, GramSpace, ScanEvent};

type PyScannedGram = (u8, usize, usize, usize, usize, u8, u64);

/// 256x256 byte-pair weight table.
#[pyclass(frozen, name = "WeightTable", module = "sngram")]
pub struct PyWeightTable {
    inner: sngram_types::WeightTable,
}

#[pymethods]
impl PyWeightTable {
    /// Load a table from its 262,160-byte SPNG binary.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        sngram_types::WeightTable::from_bytes(data)
            .map(|inner| Self { inner })
            .map_err(|e| PyValueError::new_err(format!("invalid weight table: {e}")))
    }

    /// Load a table from a `.bin` file path.
    #[staticmethod]
    fn from_path(path: PathBuf) -> PyResult<Self> {
        let data = std::fs::read(&path)
            .map_err(|e| PyIOError::new_err(format!("reading {}: {e}", path.display())))?;
        Self::from_bytes(&data)
    }

    /// Weight of the byte pair (c1, c2).
    fn weight(&self, c1: u8, c2: u8) -> u32 {
        self.inner.weight(c1, c2)
    }

    /// Table format version.
    #[getter]
    fn version(&self) -> u32 {
        self.inner.version()
    }

    fn __repr__(&self) -> String {
        format!("WeightTable(version={})", self.inner.version())
    }
}

/// Sparse grams of `data` as `(space, scanned_start, scanned_end,
/// content_start, content_end, boundary_bits, hash)` tuples.
#[pyfunction]
fn scan(py: Python<'_>, table: &PyWeightTable, data: &[u8]) -> PyResult<Vec<PyScannedGram>> {
    let table = &table.inner;
    py.detach(|| {
        let mut out = Vec::new();
        sngram::scan(table, Cursor::new(data), |event| {
            if let ScanEvent::Gram(gram) = event {
                let space = match gram.space {
                    GramSpace::Primary => 0,
                    GramSpace::Folded => 1,
                };
                out.push((
                    space,
                    gram.scanned_start,
                    gram.scanned_end,
                    gram.content_start,
                    gram.content_end,
                    gram.boundary.bits(),
                    gram.hash,
                ));
            }
        })
        .map_err(|e| PyRuntimeError::new_err(format!("scan failed: {e}")))?;
        Ok::<_, PyErr>(out)
    })
}

/// Sparse-gram index keys of `data`: little-endian u64s, one per gram.
///
/// Zero-copy view from Python: `np.frombuffer(buf, dtype=np.uint64)`.
#[pyfunction]
fn scan_hashes<'py>(
    py: Python<'py>,
    table: &PyWeightTable,
    data: &[u8],
) -> PyResult<Bound<'py, PyBytes>> {
    let table = &table.inner;
    let bytes: Vec<u8> = py.detach(|| {
        let mut out = Vec::new();
        sngram::scan(table, Cursor::new(data), |event| {
            if let ScanEvent::Gram(gram) = event {
                out.extend_from_slice(&gram.hash.to_le_bytes());
            }
        })
        .map_err(|e| PyRuntimeError::new_err(format!("scan failed: {e}")))?;
        Ok::<_, PyErr>(out)
    })?;
    Ok(PyBytes::new(py, &bytes))
}

/// The 64-bit index key of one gram's bytes — identical to the hash `scan`
/// emits for the same bytes, and to `QueryPlan.gram_hashes` entries.
#[pyfunction]
fn gram_hash(data: &[u8]) -> u64 {
    Gram::from(data).hash()
}

/// One node of a query plan: a boolean gram query over index keys.
#[pyclass(frozen, get_all, name = "QueryPlan", module = "sngram")]
pub struct PyQueryPlan {
    /// "all" | "none" | "and" | "or"
    op: String,
    /// gram byte strings of this node's bag
    grams: Vec<Py<PyBytes>>,
    /// 64-bit index keys of `grams`, same order
    gram_hashes: Vec<u64>,
    /// sub-plans (all must hold under "and", any under "or")
    #[allow(
        clippy::use_self,
        reason = "pyclass get_all macro cannot name Self here"
    )]
    sub: Vec<Py<PyQueryPlan>>,
    /// codesearch-style rendering of the whole plan
    expr: String,
}

#[pymethods]
impl PyQueryPlan {
    fn __repr__(&self) -> String {
        format!("QueryPlan({})", self.expr)
    }
}

fn convert_plan(py: Python<'_>, plan: &QueryPlan) -> PyResult<PyQueryPlan> {
    convert_expr(py, plan.expr())
}

fn convert_expr(py: Python<'_>, expr: &QueryExpr) -> PyResult<PyQueryPlan> {
    let (op, grams, sub) = match expr {
        QueryExpr::All => ("all", &[][..], &[][..]),
        QueryExpr::None => ("none", &[][..], &[][..]),
        QueryExpr::And { grams, sub } => ("and", grams.as_slice(), sub.as_slice()),
        QueryExpr::Or { grams, sub } => ("or", grams.as_slice(), sub.as_slice()),
    };
    Ok(PyQueryPlan {
        op: op.to_owned(),
        gram_hashes: grams.iter().map(Gram::hash).collect(),
        grams: grams
            .iter()
            .map(|g| PyBytes::new(py, g.as_bytes()).unbind())
            .collect(),
        sub: sub
            .iter()
            .map(|child| convert_expr(py, child).and_then(|plan| Py::new(py, plan)))
            .collect::<PyResult<_>>()?,
        expr: expr.to_string(),
    })
}

/// Fold a regex into a boolean gram query for index lookup.
///
/// Infallible for valid patterns: a too-broad pattern yields op "all", an
/// impossible one yields op "none". Raises `ValueError` on an invalid regex.
#[pyfunction]
fn query(py: Python<'_>, table: &PyWeightTable, pattern: &str) -> PyResult<PyQueryPlan> {
    let planned = sngram::query(&table.inner, pattern)
        .map_err(|e| PyValueError::new_err(format!("invalid pattern: {e}")))?;
    convert_plan(py, &planned)
}

/// Per-worker byte-pair tally; counts without atomics, merged into a shared
/// `BigramCounter` when a unit of work (one shard) completes.
#[pyclass(name = "LocalTally", module = "sngram")]
pub struct PyLocalTally {
    inner: LocalTally,
}

#[pymethods]
impl PyLocalTally {
    #[new]
    fn new() -> Self {
        Self {
            inner: LocalTally::new(),
        }
    }

    /// Count every overlapping byte pair of one document.
    fn count(&mut self, py: Python<'_>, data: &[u8]) {
        let inner = &mut self.inner;
        py.detach(|| inner.count_buffer(data));
    }

    /// Count every row of an Arrow object, per row (no pair straddles rows).
    ///
    /// Accepts anything exporting the Arrow `PyCapsule` interface with a
    /// struct/record-batch schema: a `pyarrow.Table`, `RecordBatch`, or
    /// `RecordBatchReader`. All string/binary columns are counted; nulls are
    /// skipped. The GIL is released for the whole call. Returns the number of
    /// text bytes counted.
    fn count_arrow(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<u64> {
        arrow_ffi::count_arrow(py, data, &mut self.inner)
    }

    /// Text bytes counted so far.
    #[getter]
    fn bytes_counted(&self) -> u64 {
        self.inner.bytes()
    }

    /// Fold another tally's counts (and byte/pair totals) into this one.
    ///
    /// Lets a worker count each parquet row group into a throwaway sub-tally and
    /// commit it into a per-file tally only once the row group has streamed
    /// cleanly: a mid-file connection drop discards just the in-progress row
    /// group and retries it, instead of re-reading the whole multi-GB shard.
    fn add_from(&mut self, py: Python<'_>, other: &Self) {
        let inner = &mut self.inner;
        let o = &other.inner;
        py.detach(|| inner.add_from(o));
    }
}

/// Shared, lock-free byte-pair counter: the training accumulator.
#[pyclass(frozen, name = "BigramCounter", module = "sngram")]
pub struct PyBigramCounter {
    inner: BigramCounter,
}

#[pymethods]
impl PyBigramCounter {
    #[new]
    fn new() -> Self {
        Self {
            inner: BigramCounter::new(),
        }
    }

    /// Fold a completed tally into the shared counts. Thread-safe.
    fn merge(&self, py: Python<'_>, tally: &PyLocalTally) {
        let inner = &self.inner;
        let t = &tally.inner;
        py.detach(|| inner.merge(t));
    }

    /// Count one document directly (convenience; prefer tallies in workers).
    fn process(&self, py: Python<'_>, data: &[u8]) {
        let inner = &self.inner;
        py.detach(|| inner.process(data));
    }

    /// Current count for one byte pair.
    fn count(&self, c1: u8, c2: u8) -> u64 {
        self.inner.count(c1, c2)
    }

    /// Record `n` completed files/shards.
    fn add_files(&self, n: u64) {
        self.inner.add_files(n);
    }

    /// Record `n` compressed bytes downloaded.
    fn add_downloaded(&self, n: u64) {
        self.inner.add_downloaded(n);
    }

    #[getter]
    fn pairs_processed(&self) -> u64 {
        self.inner.pairs_processed()
    }

    #[getter]
    fn bytes_processed(&self) -> u64 {
        self.inner.bytes_processed()
    }

    #[getter]
    fn files_processed(&self) -> u64 {
        self.inner.files_processed()
    }

    #[getter]
    fn downloaded_bytes(&self) -> u64 {
        self.inner.downloaded_bytes()
    }

    /// All 65,536 pair counts as little-endian u64 bytes — for checkpointing.
    fn snapshot<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let counts = self.inner.counts_vec();
        let mut out = Vec::with_capacity(counts.len() * 8);
        for c in counts {
            out.extend_from_slice(&c.to_le_bytes());
        }
        PyBytes::new(py, &out)
    }

    /// Restore a checkpoint into a fresh counter: pair counts from
    /// `snapshot()` bytes plus the saved totals.
    fn restore(&self, counts: &[u8], pairs: u64, bytes: u64, files: u64) -> PyResult<()> {
        if counts.len() != 65_536 * 8 {
            return Err(PyValueError::new_err(format!(
                "snapshot must be {} bytes, got {}",
                65_536 * 8,
                counts.len()
            )));
        }
        for (i, chunk) in counts.chunks_exact(8).enumerate() {
            let n = u64::from_le_bytes(
                chunk
                    .try_into()
                    .map_err(|_| PyValueError::new_err("snapshot chunk conversion failed"))?,
            );
            if n > 0 {
                #[allow(clippy::cast_possible_truncation, reason = "i < 65536")]
                self.inner.add((i >> 8) as u8, (i & 0xFF) as u8, n);
            }
        }
        self.inner.add_pairs(pairs);
        self.inner.add_bytes(bytes);
        self.inner.add_files(files);
        Ok(())
    }

    /// Serialize the learned weight table (SPNG `.bin` bytes, loadable by
    /// `WeightTable.from_bytes` and the Rust crates).
    fn to_table_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let inner = &self.inner;
        let bytes = py.detach(|| inner.to_table_bytes());
        PyBytes::new(py, &bytes)
    }

    fn __repr__(&self) -> String {
        format!(
            "BigramCounter(pairs={}, bytes={}, files={})",
            self.inner.pairs_processed(),
            self.inner.bytes_processed(),
            self.inner.files_processed()
        )
    }
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyWeightTable>()?;
    m.add_class::<PyQueryPlan>()?;
    m.add_class::<PyLocalTally>()?;
    m.add_class::<PyBigramCounter>()?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(scan_hashes, m)?)?;
    m.add_function(wrap_pyfunction!(gram_hash, m)?)?;
    m.add_function(wrap_pyfunction!(query, m)?)?;
    Ok(())
}
