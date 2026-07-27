//! Python callbacks used by Rust traits and constructors

use std::cell::RefCell;

use pyo3::prelude::*;

pub struct PythonCallback<'py> {
    callable: Bound<'py, PyAny>,
    failure: RefCell<Option<PyErr>>,
}

impl<'py> PythonCallback<'py> {
    pub const fn new(callable: Bound<'py, PyAny>) -> Self {
        Self {
            callable,
            failure: RefCell::new(None),
        }
    }

    pub fn call_count(&self, key: u64) -> u64 {
        self.callable
            .call1((key,))
            .and_then(|value| value.extract())
            .unwrap_or_else(|error| self.fail(error))
    }

    pub fn call_weight(&self, first: u8, second: u8) -> u32 {
        self.callable
            .call1((first, second))
            .and_then(|value| value.extract())
            .unwrap_or_else(|error| self.fail(error))
    }

    pub fn finish<T>(self, value: T) -> PyResult<T> {
        self.failure.into_inner().map_or(Ok(value), Err)
    }

    fn fail<T: Default>(&self, error: PyErr) -> T {
        self.failure.borrow_mut().get_or_insert(error);
        T::default()
    }
}
