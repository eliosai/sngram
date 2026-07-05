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

use sngram::learn::BigramCounter;
use std::io::Cursor;

use sngram_types::{Gram, GramNeedle, PlanExpr, QueryPlan, ScanEvent, ScanNeed};

type PyScannedGram = (usize, usize, u64);

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

/// Sparse grams of `data` as `(content_start, content_end, key)` tuples.
#[pyfunction]
fn scan(py: Python<'_>, table: &PyWeightTable, data: &[u8]) -> PyResult<Vec<PyScannedGram>> {
    let table = &table.inner;
    py.detach(|| {
        let mut out = Vec::new();
        sngram::scan(table, Cursor::new(data), |event| {
            if let ScanEvent::Gram(gram) = event {
                out.push((gram.span.start, gram.span.end, gram.key.value()));
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
                out.extend_from_slice(&gram.key.value().to_le_bytes());
            }
        })
        .map_err(|e| PyRuntimeError::new_err(format!("scan failed: {e}")))?;
        Ok::<_, PyErr>(out)
    })?;
    Ok(PyBytes::new(py, &bytes))
}

/// The raw unkeyed 64-bit key of one gram's bytes.
///
/// Scan events already carry their final index key. Use that key directly:
/// virtual sentinel grams and case-folded supplement grams are intentionally
/// not equivalent to hashing only the returned content span.
#[pyfunction]
fn gram_hash(data: &[u8]) -> u64 {
    Gram::from(data).hash()
}

/// One node of a query plan: a boolean candidate query over index keys.
#[pyclass(frozen, get_all, name = "QueryPlan", module = "sngram")]
pub struct PyQueryPlan {
    /// "all" | "none" | "and" | "or"
    op: String,
    /// key alternatives for each logical gram needle
    grams: Vec<Vec<u64>>,
    /// scan-summary needs, rendered for inspection
    needs: Vec<String>,
    /// children (all must hold under "and", any under "or")
    #[allow(
        clippy::use_self,
        reason = "pyclass get_all macro cannot name Self here"
    )]
    children: Vec<Py<PyQueryPlan>>,
    /// rendering of the whole plan
    expr: String,
}

#[pymethods]
impl PyQueryPlan {
    fn __repr__(&self) -> String {
        format!("QueryPlan({})", self.expr)
    }
}

fn convert_plan(py: Python<'_>, plan: &QueryPlan) -> PyResult<PyQueryPlan> {
    convert_expr(py, plan.root())
}

fn convert_expr(py: Python<'_>, expr: &PlanExpr) -> PyResult<PyQueryPlan> {
    let parts = plan_parts(expr);
    Ok(PyQueryPlan {
        op: parts.op.to_owned(),
        grams: parts.grams.iter().map(needle_keys).collect(),
        needs: render_needs(parts.needs),
        children: convert_children(py, parts.children)?,
        expr: expr.to_string(),
    })
}

struct PlanParts<'a> {
    op: &'static str,
    grams: &'a [GramNeedle],
    needs: &'a [ScanNeed],
    children: &'a [PlanExpr],
}

fn plan_parts(expr: &PlanExpr) -> PlanParts<'_> {
    match expr {
        PlanExpr::All => ("all", &[][..], &[][..], &[][..]),
        PlanExpr::None => ("none", &[][..], &[][..], &[][..]),
        PlanExpr::AllOf {
            grams,
            needs,
            children,
        } => (
            "and",
            grams.as_slice(),
            needs.as_slice(),
            children.as_slice(),
        ),
        PlanExpr::AnyOf {
            grams,
            needs,
            children,
        } => (
            "or",
            grams.as_slice(),
            needs.as_slice(),
            children.as_slice(),
        ),
    }
    .into()
}

impl<'a>
    From<(
        &'static str,
        &'a [GramNeedle],
        &'a [ScanNeed],
        &'a [PlanExpr],
    )> for PlanParts<'a>
{
    fn from(
        (op, grams, needs, children): (
            &'static str,
            &'a [GramNeedle],
            &'a [ScanNeed],
            &'a [PlanExpr],
        ),
    ) -> Self {
        Self {
            op,
            grams,
            needs,
            children,
        }
    }
}

fn render_needs(needs: &[ScanNeed]) -> Vec<String> {
    needs.iter().map(|need| format!("{need:?}")).collect()
}

fn convert_children(py: Python<'_>, children: &[PlanExpr]) -> PyResult<Vec<Py<PyQueryPlan>>> {
    children
        .iter()
        .map(|child| convert_expr(py, child).and_then(|plan| Py::new(py, plan)))
        .collect()
}

fn needle_keys(needle: &GramNeedle) -> Vec<u64> {
    needle.keys().map(sngram_types::GramKey::value).collect()
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

    /// Fold a completed staging counter into this counter. Thread-safe.
    fn merge(&self, py: Python<'_>, other: &Self) {
        let inner = &self.inner;
        let o = &other.inner;
        py.detach(|| inner.merge(o));
    }

    /// Count one document directly (convenience; prefer tallies in workers).
    fn process(&self, py: Python<'_>, data: &[u8]) {
        let inner = &self.inner;
        py.detach(|| inner.process(data));
    }

    /// Count every row of an Arrow object, per row (no pair straddles rows).
    ///
    /// Accepts anything exporting the Arrow `PyCapsule` interface with a
    /// struct/record-batch schema: a `pyarrow.Table`, `RecordBatch`, or
    /// `RecordBatchReader`. All string/binary columns are counted; nulls are
    /// skipped. The GIL is released for the whole call. Returns the number of
    /// text bytes counted.
    fn count_arrow(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<u64> {
        arrow_ffi::count_arrow(py, data, &self.inner)
    }

    /// Current count for one byte pair.
    fn count(&self, c1: u8, c2: u8) -> u64 {
        self.inner.count(c1, c2)
    }

    /// Record `n` completed files/shards.
    fn add_files(&self, n: u64) {
        self.inner.add_files(n);
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

    /// All 65,536 pair counts as little-endian u64 bytes — for checkpointing.
    fn snapshot<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.snapshot())
    }

    /// Restore a checkpoint into a fresh counter: pair counts from
    /// `snapshot()` bytes plus the saved totals.
    fn restore(&self, counts: &[u8], pairs: u64, bytes: u64, files: u64) -> PyResult<()> {
        self.inner
            .restore(counts, pairs, bytes, files)
            .map_err(|e| PyValueError::new_err(e.to_string()))
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
    m.add_class::<PyBigramCounter>()?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(scan_hashes, m)?)?;
    m.add_function(wrap_pyfunction!(gram_hash, m)?)?;
    m.add_function(wrap_pyfunction!(query, m)?)?;
    Ok(())
}
