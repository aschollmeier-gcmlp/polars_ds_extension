#![feature(float_gamma, float_next_up_down)]

mod arkadia;
mod linalg;
mod num_ext;
mod stats;
mod stats_utils;
mod str_ext;
mod utils;

mod pymodels;

use pyo3::{pymodule, types::PyModule, Bound, PyResult, Python};

#[pymodule]
#[pyo3(name = "_polars_ds_elastic_net")]
fn _polars_ds(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<pymodels::py_lr::PyLR>()?;
    m.add_class::<pymodels::py_lr::PyOnlineLR>()?;
    m.add_class::<pymodels::py_kdt::PyKDT>()?;
    Ok(())
}
