//! Query plan bindings

use std::cell::RefCell;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use sngram_types::{DfStats, GramKey, GramNeedle, PlanExpr, QueryPlan, ScanNeed};

use crate::scanning::PyScanSummary;
use crate::table::PyWeightTable;

/// A necessary per-entry condition testable against a scan summary
#[pyclass(frozen, name = "ScanNeed", module = "sngram")]
pub struct PyScanNeed {
    inner: ScanNeed,
}

#[pymethods]
impl PyScanNeed {
    /// True when a scan summary satisfies this condition
    fn satisfied_by(&self, summary: &PyScanSummary) -> bool {
        self.inner.satisfied_by(summary.inner())
    }

    fn __repr__(&self) -> String {
        format!("ScanNeed({:?})", self.inner)
    }
}

/// One node of a query plan: a boolean candidate query over index keys
#[pyclass(frozen, name = "QueryPlan", module = "sngram")]
pub struct PyQueryPlan {
    inner: PlanExpr,
}

impl PyQueryPlan {
    fn parts(&self) -> (&'static str, &[GramNeedle], &[ScanNeed], &[PlanExpr]) {
        match &self.inner {
            PlanExpr::All => ("all", &[], &[], &[]),
            PlanExpr::None => ("none", &[], &[], &[]),
            PlanExpr::AllOf {
                grams,
                needs,
                children,
            } => ("and", grams, needs, children),
            PlanExpr::AnyOf {
                grams,
                needs,
                children,
            } => ("or", grams, needs, children),
        }
    }
}

#[pymethods]
impl PyQueryPlan {
    /// "all" | "none" | "and" | "or"
    #[getter]
    fn op(&self) -> &'static str {
        self.parts().0
    }

    /// Key alternatives for each logical gram needle
    #[getter]
    fn grams(&self) -> Vec<Vec<u64>> {
        self.parts().1.iter().map(needle_keys).collect()
    }

    /// Scan-summary conditions at this node
    #[getter]
    fn needs(&self) -> Vec<PyScanNeed> {
        self.parts()
            .2
            .iter()
            .map(|need| PyScanNeed {
                inner: need.clone(),
            })
            .collect()
    }

    /// Nested expressions: all must hold under "and", any under "or"
    #[getter]
    fn children(&self) -> Vec<Self> {
        self.parts()
            .3
            .iter()
            .map(|child| Self {
                inner: child.clone(),
            })
            .collect()
    }

    /// Rendering of this expression
    #[getter]
    fn expr(&self) -> String {
        self.inner.to_string()
    }

    /// Total gram needles in this expression tree
    #[getter]
    fn gram_count(&self) -> usize {
        self.inner.gram_count()
    }

    /// A copy reordered and thinned by document frequency
    ///
    /// `df` maps one gram key to its entry count. Keys whose summed
    /// alternatives reach `stop_df` entries stop narrowing and drop when a
    /// stronger sibling remains.
    fn tune(&self, df: Bound<'_, PyAny>, total_entries: u64, stop_df: u64) -> PyResult<Self> {
        let stats = CallableDf {
            func: df,
            total: total_entries,
            failure: RefCell::new(None),
        };
        let mut plan = QueryPlan::new(self.inner.clone());
        plan.tune(&stats, stop_df);
        let tuned = Self {
            inner: plan.root().clone(),
        };
        stats.failure.into_inner().map_or_else(|| Ok(tuned), Err)
    }

    fn __repr__(&self) -> String {
        format!("QueryPlan({})", self.inner)
    }
}

/// Document-frequency stats backed by a Python callable
struct CallableDf<'py> {
    func: Bound<'py, PyAny>,
    total: u64,
    failure: RefCell<Option<PyErr>>,
}

impl DfStats for CallableDf<'_> {
    fn entry_count(&self, key: GramKey) -> u64 {
        self.func
            .call1((key.value(),))
            .and_then(|count| count.extract())
            .unwrap_or_else(|e| {
                self.failure.borrow_mut().get_or_insert(e);
                0
            })
    }

    fn total_entries(&self) -> u64 {
        self.total
    }
}

fn needle_keys(needle: &GramNeedle) -> Vec<u64> {
    needle.keys().map(GramKey::value).collect()
}

/// Fold a regex into a boolean gram query for index lookup
///
/// Infallible for valid patterns: a too-broad pattern yields op "all", an
/// impossible one yields op "none". Raises `ValueError` on an invalid regex.
#[pyfunction]
pub fn query(table: &PyWeightTable, pattern: &str) -> PyResult<PyQueryPlan> {
    let planned = sngram::query(table.inner(), pattern)
        .map_err(|e| PyValueError::new_err(format!("invalid pattern: {e}")))?;
    Ok(PyQueryPlan {
        inner: planned.root().clone(),
    })
}
