use crate::linalg::lstsq::{
    faer_coordinate_descent, faer_recursive_lstsq, faer_rolling_lstsq, faer_rolling_skipping_lstsq, faer_solve_lstsq, faer_solve_lstsq_rcond, faer_solve_ridge, faer_solve_ridge_rcond, faer_weighted_lstsq, sklearn_coordinate_descent, ClosedFormLRMethods, LRMethods
};
use crate::utils::{to_frame, NullPolicy};
/// Least Squares using Faer and ndarray.
use core::f64;
use faer::{concat, prelude::*};
use faer_ext::{IntoFaer, IntoNdarray};
use itertools::Itertools;
use ndarray::{s, Array, Array2, Axis};
use polars::prelude as pl;
use polars::prelude::*;
use pyo3_polars::derive::polars_expr;
use rayon::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub(crate) struct LstsqKwargs {
    pub(crate) bias: bool,
    pub(crate) null_policy: String,
    pub(crate) solver: String,
    pub(crate) l1_reg: f64,
    pub(crate) l2_reg: f64,
    pub(crate) tol: f64,
    pub(crate) max_iter: usize,
    pub(crate) use_new_descent: bool,
    #[serde(default)]
    pub(crate) weighted: bool,
}

#[derive(Deserialize, Debug)]
pub(crate) struct MultiLstsqKwargs {
    pub(crate) bias: bool,
    pub(crate) null_policy: String,
    pub(crate) solver: String,
    pub(crate) last_target_idx: usize,
    pub(crate) l2_reg: f64,
}

// Sherman-William-Woodbury (Update, online versions) LstsqKwargs
#[derive(Deserialize, Debug)]
pub(crate) struct SWWLstsqKwargs {
    pub(crate) null_policy: String,
    pub(crate) n: usize,
    pub(crate) bias: bool,
    pub(crate) lambda: f64,
    pub(crate) min_size: usize,
}

fn report_output(_: &[Field]) -> PolarsResult<Field> {
    let features = Field::new("features".into(), DataType::String); // index of feature
    let beta = Field::new("beta".into(), DataType::Float64); // estimated value for this coefficient
    let stderr = Field::new("std_err".into(), DataType::Float64); // Std Err for this coefficient
    let t = Field::new("t".into(), DataType::Float64); // t value for this coefficient
    let p = Field::new("p>|t|".into(), DataType::Float64); // p value for this coefficient
    let ci_lower = Field::new("0.025".into(), DataType::Float64); // CI lower bound at 0.025
    let ci_upper = Field::new("0.975".into(), DataType::Float64); // CI upper bound at 0.975
    let v: Vec<Field> = vec![features, beta, stderr, t, p, ci_lower, ci_upper]; //  ci_lower, ci_upper
    Ok(Field::new("lstsq_report".into(), DataType::Struct(v)))
}

fn pred_residue_output(_: &[Field]) -> PolarsResult<Field> {
    let pred = Field::new("pred".into(), DataType::Float64);
    let residue = Field::new("resid".into(), DataType::Float64);
    let v = vec![pred, residue];
    Ok(Field::new("pred".into(), DataType::Struct(v)))
}

fn coeff_pred_output(_: &[Field]) -> PolarsResult<Field> {
    let coeffs = Field::new("coeffs".into(), DataType::List(Box::new(DataType::Float64)));
    let pred = Field::new("prediction".into(), DataType::Float64);
    let v: Vec<Field> = vec![coeffs, pred];
    Ok(Field::new("".into(), DataType::Struct(v)))
}

fn coeff_singular_values_output(_: &[Field]) -> PolarsResult<Field> {
    let coeffs = Field::new("coeffs".into(), DataType::List(Box::new(DataType::Float64)));
    let singular_values = Field::new(
        "singular_values".into(),
        DataType::List(Box::new(DataType::Float64)),
    );
    let v: Vec<Field> = vec![coeffs, singular_values];
    Ok(Field::new("".into(), DataType::Struct(v)))
}

fn coeff_output(_: &[Field]) -> PolarsResult<Field> {
    Ok(Field::new(
        "coeffs".into(),
        DataType::List(Box::new(DataType::Float64)),
    ))
}

/// Returns a Array2 ready for linear regression, and a mask, where true means the row doesn't contain null
#[inline(always)]
fn series_to_mat_for_lstsq(
    inputs: &[Series],
    add_bias: bool,
    null_policy: NullPolicy,
) -> PolarsResult<(Array2<f64>, BooleanChunked)> {
    let n_features = inputs.len().abs_diff(1);

    // minus 1 because target is also in inputs. Target is at position 0.
    let y_has_null = inputs[0].has_nulls();
    let has_null = inputs[1..].iter().any(|s| s.has_nulls()) | y_has_null;

    let mut df = to_frame(inputs)?;
    if df.is_empty() {
        return Err(PolarsError::ComputeError("Empty data".into()));
    }

    // MinMax scaling
    let all_cols_but_first = pl::col("*").exclude([df.get_column_names()[0].clone()]);
    df = df.lazy().with_column(
         (all_cols_but_first.clone() - all_cols_but_first.clone().min()) / (all_cols_but_first.clone().max() - all_cols_but_first.clone().min())
    ).collect()?;

    // Add a constant column if add_bias
    if add_bias {
        df = df.lazy().with_column(lit(1f64)).collect()?;
    }

    // In mask, true means not null.
    let y_name = inputs[0].name();
    let init_mask = inputs[0].is_not_null();
    let (df, mask) = if has_null {
        match null_policy {
            // Like ignore, skip_window takes the raw data. The actual skip is done in the underlying function in linalg.
            NullPolicy::IGNORE | NullPolicy::SKIP_WINDOW => {
                // false, because it has nulls
                Ok((df, BooleanChunked::from_slice("".into(), &[false])))
            }
            NullPolicy::RAISE => Err(PolarsError::ComputeError("Nulls found in data".into())),
            NullPolicy::SKIP => {
                let init_mask = inputs[0].is_not_null(); //0 always exist
                let mask = inputs[1..]
                    .iter()
                    .fold(init_mask, |acc, s| acc & s.is_not_null());

                df = df.filter(&mask).unwrap();
                Ok((df, mask))
            }
            NullPolicy::FILL(x) => {
                df = df
                    .lazy()
                    .with_columns([pl::col("*")
                        .exclude([y_name.clone()])
                        .cast(DataType::Float64)
                        .fill_null(lit(x))])
                    .collect()?;

                if y_has_null {
                    df = df.filter(&init_mask).unwrap();
                    Ok((df, init_mask))
                } else {
                    // all filled, no nulls
                    let mask = BooleanChunked::from_slice("".into(), &[true]);
                    Ok((df, mask))
                }
            }
            NullPolicy::FILL_WINDOW(x) => {
                df = df
                    .lazy()
                    .with_columns([pl::col("*")
                        .exclude([y_name.clone()])
                        .cast(DataType::Float64)
                        .fill_null(lit(x))])
                    .collect()?;

                if y_has_null {
                    // Unlike fill, this doesn't drop y's nulls
                    Ok((df, BooleanChunked::from_slice("".into(), &[false])))
                } else {
                    // all filled, no nulls
                    let mask = BooleanChunked::from_slice("".into(), &[true]);
                    Ok((df, mask))
                }
            }
        }
    } else {
        // In this case, the (!mask).any() is never true, which means there is no null.
        let mask = BooleanChunked::from_slice("".into(), &[true]);
        Ok((df, mask))
    }?;

    if df.height() < n_features {
        Err(PolarsError::ComputeError(
            "#Data < #features. No conclusive result.".into(),
        ))
    } else {
        let mat = df.to_ndarray::<Float64Type>(IndexOrder::Fortran)?;
        Ok((mat, mask))
    }
}

#[polars_expr(output_type_func=coeff_output)]
fn pl_lstsq(inputs: &[Series], kwargs: LstsqKwargs) -> PolarsResult<Series> {
    if inputs[0].len() < 36 {
        let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
            ListPrimitiveChunkedBuilder::new(
                "coeffs".into(),
                1,
                1,
                DataType::Float64,
            );
        let out = builder.finish();
        return Ok(out.into_series());
    }
    let add_bias = kwargs.bias;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    let weighted = kwargs.weighted;
    let data_for_matrix = if weighted { &inputs[1..] } else { inputs };

    match series_to_mat_for_lstsq(data_for_matrix, add_bias, null_policy) {
        Ok((mat, _)) => {
            // Solving Least Square
            let x = mat.slice(s![.., 1..]).into_faer();
            let y = mat.slice(s![.., 0..1]).into_faer();
            let x_arr = x.into_ndarray();
            let x_centered = &x_arr - &x_arr.mean_axis(Axis(0)).unwrap();
            let y_arr = y.into_ndarray();
            let y_centered = &y_arr - y_arr.mean().unwrap();
            const EPSILON: f64 = 0.001;
            let Xy = x_centered.t().dot(&y_centered);
            const L1_RATIO: f64 = 0.5;
            const NUM_ALPHAS: usize = 100;
            let alpha_max = (Xy.map(|x| x.abs()).iter().fold(-1., |acc, elem| elem.max(acc)) / (36. * L1_RATIO)).max(
                f64::MIN_POSITIVE
            );
            let alphas = ndarray::Array::geomspace(alpha_max, (alpha_max * EPSILON).max(f64::MIN_POSITIVE), NUM_ALPHAS).unwrap();
            const NUM_SPLITS: usize = 5;
            let mut folds: Vec<(Mat<f64>, Mat<f64>, Mat<f64>, Mat<f64>)> = Vec::new();
            for split_num in 1..=NUM_SPLITS {
                let (x_train, y_train, x_test, y_test) = match split_num % NUM_SPLITS {
                    0 => {(
                        x.subrows(8, 36 - 8).to_owned(),
                        y.subrows(8, 36 - 8).to_owned(),
                        x.subrows(0, 8).to_owned(),
                        y.subrows(0, 8).to_owned()
                    )},
                    1 => {
                        let x_train1 = x.subrows(0, 8);
                        let y_train1 = y.subrows(0, 8);
                        let x_train2 = x.subrows(15, 36-15);
                        let y_train2 = y.subrows(15, 36-15);
                        let x_train_all = concat![[x_train1], [x_train2]];
                        let y_train_all = concat![[y_train1], [y_train2]];
                        (
                            x_train_all,
                            y_train_all,
                            x.subrows(8, 7).to_owned(),
                            y.subrows(8, 7).to_owned(),
                        )
                    },
                    2 => {
                        let x_train1 = x.subrows(0, 8+7);
                        let y_train1 = y.subrows(0, 8+7);
                        let x_train2 = x.subrows(22, 36-22);
                        let y_train2 = y.subrows(22, 36-22);
                        let x_train_all = concat![[x_train1], [x_train2]];
                        let y_train_all = concat![[y_train1], [y_train2]];
                        (
                            x_train_all,
                            y_train_all,
                            x.subrows(15, 7).to_owned(),
                            y.subrows(15, 7).to_owned(),
                        )
                    },
                    3 => {
                        let x_train1 = x.subrows(0, 8+7*2);
                        let y_train1 = y.subrows(0, 8+7*2);
                        let x_train2 = x.subrows(29, 36-29);
                        let y_train2 = y.subrows(29, 36-29);
                        let x_train_all = concat![[x_train1], [x_train2]];
                        let y_train_all = concat![[y_train1], [y_train2]];
                        (
                            x_train_all,
                            y_train_all,
                            x.subrows(22, 7).to_owned(),
                            y.subrows(22, 7).to_owned(),
                        )
                    },
                    4 => {(
                        x.subrows(0, 36 - 7).to_owned(),
                        y.subrows(0, 36 - 7).to_owned(),
                        x.subrows(29, 7).to_owned(),
                        y.subrows(29, 7).to_owned()
                    )},
                    _ => panic!("Not supposed to reach this arm of split_num branch.")
                };
                folds.push((x_train, y_train, x_test, y_test));
            }
            let mut mse_contribs: Vec<ndarray::ArrayBase<ndarray::OwnedRepr<f64>, ndarray::Dim<[usize; 1]>>> = Vec::new();
            folds.into_par_iter().map(
                |(x_train, y_train, x_test, y_test)| {
                    let mut v: ndarray::ArrayBase<ndarray::OwnedRepr<f64>, ndarray::Dim<[usize; 1]>> = Array::zeros(NUM_ALPHAS);
                    // let mut warm_start_beta = None;
                    let xtx: Mat<f64> = x_train.transpose() * &x_train;
                    let xty: Mat<f64> = x_train.transpose() * &y_train;
                    let m: f64 = x_train.nrows() as f64;
                    let norms = x_train
                        .col_iter()
                        .map(|c| c.squared_norm_l2())
                        .collect::<Vec<_>>();
                

                    // Begin Gram matrix descent stuff
                    let n1 = x_train.ncols() - 1;
                    let x_train_arr: ndarray::ArrayBase<ndarray::ViewRepr<&f64>, ndarray::Dim<[usize; 2]>> = x_train.as_ref().into_ndarray().slice_move(s![.., ..n1]);
                    let x_train_offset = x_train_arr.mean_axis(Axis(0)).unwrap();
                    let x_train_centered = &x_train_arr - &x_train_offset;
                    let x_train_centered_mat: MatRef<'_, f64> = x_train_centered.view().into_faer();

                    let y_bar = y_train.as_ref().into_ndarray().mean().unwrap();
                    let y_train_centered_mat = &y_train - Mat::<f64>::ones(y_train.nrows(), 1) * y_bar;

                    let q = x_train_centered_mat.transpose() * y_train_centered_mat;
                    let Q: Mat<f64> = x_train_centered_mat.transpose() * x_train_centered_mat;
                    // End Gram matrix descent stuff
                    for (i, alpha) in alphas.iter().enumerate() {
                        let reg_val = alpha * L1_RATIO;
                        let candidate_coeffs = if kwargs.use_new_descent {
                            let non_intercept_betas = sklearn_coordinate_descent(
                                reg_val * m,
                                reg_val * m,
                                kwargs.tol, 
                                kwargs.max_iter, 
                                q.as_ref(), 
                                Q.as_ref()
                            );
                            let intercept = (y_bar - &x_train_offset.dot(&non_intercept_betas.as_ref().into_ndarray())).get(0).unwrap().to_owned();
                            let res = concat![[non_intercept_betas], [mat![[intercept]]]];
                            res
                        } else {
                            faer_coordinate_descent(
                                x_train.as_ref(),
                                y_train.as_ref(),
                                reg_val, // l1 
                                reg_val, // l2
                                add_bias,
                                kwargs.tol,
                                kwargs.max_iter,
                                None,
                                Some(rand::thread_rng()),
                                xtx.as_ref(),
                                xty.as_ref(),
                                norms.iter().map(|elem| elem + m * reg_val).collect_vec(),
                            )
                        };
                        let pred = &x_test * &candidate_coeffs;
                        let resid = &y_test - &pred;
                        let this_mse = resid.squared_norm_l2() / resid.nrows() as f64;
                        v[i] = this_mse;
                    }
                    v
                }
            ).collect_into_vec(mse_contribs.as_mut());
            let mut mses: ndarray::ArrayBase<ndarray::OwnedRepr<f64>, ndarray::Dim<[usize; 1]>> = Array::zeros(NUM_ALPHAS);
            for partial in mse_contribs {
                for (i, contrib) in partial.iter().enumerate() {
                    mses[i] += contrib
                }
            }
            let min_mse_alpha = mses.iter().zip(alphas).min_by(
                |a, b| {
                    if a.0 < b.0 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
            ).unwrap().1;
            // eprintln!("min_mse_alpha: {}", min_mse_alpha);

            let chosen_penalty = min_mse_alpha * L1_RATIO;
            let m = x.nrows() as f64;


            // Begin Gram matrix descent stuff
            let n1 = x.ncols() - 1;
            let x_arr: ndarray::ArrayBase<ndarray::ViewRepr<&f64>, ndarray::Dim<[usize; 2]>> = x.as_ref().into_ndarray().slice_move(s![.., ..n1]);
            let x_offset = x_arr.mean_axis(Axis(0)).unwrap();
            let x_centered = &x_arr - &x_offset;
            let x_centered_mat: MatRef<'_, f64> = x_centered.view().into_faer();

            let y_bar = y.as_ref().into_ndarray().mean().unwrap();
            let y_centered_mat = &y - Mat::<f64>::ones(y.nrows(), 1) * y_bar;

            let q = x_centered_mat.transpose() * y_centered_mat;
            let Q: Mat<f64> = x_centered_mat.transpose() * x_centered_mat;
            // End Gram matrix descent stuff

            let norms = x
                .col_iter()
                .map(|c| c.squared_norm_l2())
                .collect::<Vec<_>>();
            let coeffs = if kwargs.use_new_descent {
                let non_intercept_betas = sklearn_coordinate_descent(
                    chosen_penalty * m,
                    chosen_penalty * m,
                    kwargs.tol, 
                    kwargs.max_iter, 
                    q.as_ref(), 
                    Q.as_ref()
                );
                let intercept = (y_bar - &x_offset.dot(&non_intercept_betas.as_ref().into_ndarray())).get(0).unwrap().to_owned();
                let res = concat![[non_intercept_betas], [mat![[intercept]]]];
                res
            } else { 
                faer_coordinate_descent(
                    x,
                    y,
                    chosen_penalty,
                    chosen_penalty,
                    add_bias,
                    kwargs.tol,
                    kwargs.max_iter,
                    None,
                    None,
                    (x.transpose() * x).as_ref(),
                    (x.transpose() * y).as_ref(),
                    norms.iter().map(|elem| elem + m * chosen_penalty).collect_vec(),
                )
            };
            let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
                ListPrimitiveChunkedBuilder::new(
                    "coeffs".into(),
                    1,
                    coeffs.nrows(),
                    DataType::Float64,
                );

            builder.append_slice(coeffs.col_as_slice(0));
            let out = builder.finish();
            Ok(out.into_series())
        }
        Err(e) => Err(e),
    }
}

fn series_to_mat_for_multi_lstsq(
    inputs: &[Series],
    last_target_idx: usize,
    add_bias: bool,
    null_policy: NullPolicy,
) -> PolarsResult<Array2<f64>> {
    let y_has_null = inputs[..last_target_idx]
        .iter()
        .fold(false, |acc, s| s.has_nulls() | acc);
    let has_null = inputs[last_target_idx..].iter().any(|s| s.has_nulls()) | y_has_null;

    let mut df = if has_null {
        match null_policy {
            NullPolicy::RAISE => Err(PolarsError::ComputeError("Nulls found in data".into())),

            NullPolicy::FILL(x) => {
                let df = DataFrame::new(inputs.iter().map(|s| s.clone().into_column()).collect())?;
                if y_has_null {
                    // This is because for predictions, it becomes too complicated when different targets have nulls.
                    // There will be too many masks we need to keep track of.
                    // This can be dealt with but I won't implement it for now.
                    Err(PolarsError::ComputeError(
                        "Filling null doesn't work for multi-target lstsq when there are nulls in any of the targets.".into(),
                    ))
                } else {
                    let y_names = inputs[..last_target_idx]
                        .iter()
                        .map(|s| s.name().clone())
                        .collect::<Vec<_>>();

                    df.lazy()
                        .with_columns([pl::col("*")
                            .exclude(y_names)
                            .cast(DataType::Float64)
                            .fill_null(lit(x))])
                        .collect()
                }
            }
            _ => Err(PolarsError::ComputeError(
                "The null policy is not supported by multi-target linear regression.".into(),
            )),
        }
    } else {
        DataFrame::new(inputs.iter().map(|s| s.clone().into_column()).collect())
    }?;

    if add_bias {
        df = df.lazy().with_column(lit(1f64)).collect()?;
    }

    if df.is_empty() {
        Err(PolarsError::ComputeError("Empty data".into()))
    } else {
        df.to_ndarray::<Float64Type>(IndexOrder::Fortran)
    }
}

// Strictly speaking, this output type is not correct. Should be struct of last_target_idx many
// coeff_outputs
#[polars_expr(output_type_func=coeff_output)]
fn pl_lstsq_multi(inputs: &[Series], kwargs: MultiLstsqKwargs) -> PolarsResult<Series> {
    let add_bias = kwargs.bias;
    let solver = kwargs.solver.as_str().into();
    let last_target_idx = kwargs.last_target_idx;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    let y_names = inputs[..last_target_idx]
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>();
    let mat = series_to_mat_for_multi_lstsq(inputs, last_target_idx, add_bias, null_policy)?;

    let y = mat.slice(s![.., 0..last_target_idx]).into_faer();
    let x = mat.slice(s![.., last_target_idx..]).into_faer();

    let coeffs = match LRMethods::from((0., kwargs.l2_reg)) {
        LRMethods::Normal => Ok(faer_solve_lstsq(x, y, solver)),
        LRMethods::L2 => Ok(faer_solve_ridge(x, y, kwargs.l2_reg, add_bias, solver)),
        _ => Err(PolarsError::ComputeError(
            "The method is not supported.".into(),
        )),
    }?;

    let df_out = unsafe {
        DataFrame::new_no_checks(
            x.nrows(),
            y_names
                .into_iter()
                .enumerate()
                .map(|(i, y)| {
                    let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
                        ListPrimitiveChunkedBuilder::new(
                            y.clone(),
                            1,
                            coeffs.nrows(),
                            DataType::Float64,
                        );
                    builder.append_slice(coeffs.col_as_slice(i));
                    let out = builder.finish();
                    out.into_column()
                })
                .collect::<Vec<_>>(),
        )
    };
    Ok(df_out.into_struct("coeffs".into()).into_series())
}

// Strictly speaking, this output type is not correct.
#[polars_expr(output_type_func=pred_residue_output)]
fn pl_lstsq_multi_pred(inputs: &[Series], kwargs: MultiLstsqKwargs) -> PolarsResult<Series> {
    let add_bias = kwargs.bias;
    let solver = kwargs.solver.as_str().into();
    let last_target_idx = kwargs.last_target_idx;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    let y_names = inputs[..last_target_idx]
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>();
    let mat = series_to_mat_for_multi_lstsq(inputs, last_target_idx, add_bias, null_policy)?;

    let y = mat.slice(s![.., 0..last_target_idx]).into_faer();
    let x = mat.slice(s![.., last_target_idx..]).into_faer();

    let coeffs = match LRMethods::from((0., kwargs.l2_reg)) {
        LRMethods::Normal => Ok(faer_solve_lstsq(x, y, solver)),
        LRMethods::L2 => Ok(faer_solve_ridge(x, y, kwargs.l2_reg, add_bias, solver)),
        _ => Err(PolarsError::ComputeError(
            "The method is not supported.".into(),
        )),
    }?;

    let pred = x * &coeffs;
    let resid = y - &pred;

    let mut s = Vec::with_capacity(y_names.len() * 2);
    for (i, y) in y_names.into_iter().enumerate() {
        let pred_name = format!("{}_pred", y);
        let resid_name = format!("{}_resid", y);
        let p = Float64Chunked::from_slice(pred_name.into(), pred.col_as_slice(i));
        let r = Float64Chunked::from_slice(resid_name.into(), resid.col_as_slice(i));
        s.push(p.into_column());
        s.push(r.into_column());
    }
    let df_out = unsafe { DataFrame::new_no_checks(pred.nrows(), s) };
    Ok(df_out.into_struct("all_preds".into()).into_series())
}

#[polars_expr(output_type_func=coeff_singular_values_output)]
fn pl_lstsq_w_rcond(inputs: &[Series], kwargs: LstsqKwargs) -> PolarsResult<Series> {
    let add_bias = kwargs.bias;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    let method = if kwargs.l2_reg > 0. {
        ClosedFormLRMethods::L2
    } else {
        ClosedFormLRMethods::Normal
    };

    // Target y is at index 0
    match series_to_mat_for_lstsq(inputs, add_bias, null_policy) {
        Ok((mat, _)) => {
            // rcond will be passed as tol
            let rcond = kwargs
                .tol
                .max(f64::EPSILON * (inputs.len().max(mat.len())) as f64);

            // Solving Least Square
            let x = mat.slice(s![.., 1..]).into_faer();
            let y = mat.slice(s![.., 0..1]).into_faer();

            //     // faer_solve_ridge_rcond
            let (coeffs, singular_values) = match method {
                ClosedFormLRMethods::Normal => faer_solve_lstsq_rcond(x, y, rcond),
                ClosedFormLRMethods::L2 => {
                    faer_solve_ridge_rcond(x, y, kwargs.l2_reg, add_bias, rcond)
                }
            };

            let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
                ListPrimitiveChunkedBuilder::new(
                    "coeffs".into(),
                    1,
                    coeffs.nrows(),
                    DataType::Float64,
                );

            builder.append_slice(coeffs.col_as_slice(0));
            let coeffs_ca = builder.finish();

            let mut sv_builder: ListPrimitiveChunkedBuilder<Float64Type> =
                ListPrimitiveChunkedBuilder::new(
                    "singular_values".into(),
                    1,
                    singular_values.len(),
                    DataType::Float64,
                );

            sv_builder.append_slice(&singular_values);
            let coeffs_sv = sv_builder.finish();

            let ca = StructChunked::from_columns(
                "".into(),
                coeffs_sv.len(),
                &[coeffs_ca.into_column(), coeffs_sv.into_column()],
            )?;
            Ok(ca.into_series())
        }
        Err(e) => Err(e),
    }
}

#[polars_expr(output_type_func=report_output)]
fn pl_lstsq_report(inputs: &[Series], kwargs: LstsqKwargs) -> PolarsResult<Series> {
    let add_bias = kwargs.bias;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;
    // index 0 is target y. Skip
    let mut name_builder =
        StringChunkedBuilder::new("features".into(), inputs.len() + (add_bias) as usize);
    for s in inputs[1..].iter().map(|s| s.name()) {
        name_builder.append_value(s);
    }
    if add_bias {
        name_builder.append_value("__bias__");
    }
    // Copy data
    // Target y is at index 0
    match series_to_mat_for_lstsq(inputs, add_bias, null_policy) {
        Ok((mat, _)) => {
            let ncols = mat.ncols() - 1;
            let nrows = mat.nrows();

            let x = mat.slice(s![0..nrows, 1..]).into_faer();
            let y = mat.slice(s![0..nrows, 0..1]).into_faer();
            // Solving Least Square
            let xtx = x.transpose() * &x;
            let xtx_qr = xtx.col_piv_qr();
            let xtx_inv = xtx_qr.inverse();
            let coeffs = &xtx_inv * x.transpose() * y;
            let betas = coeffs.col_as_slice(0);
            // Degree of Freedom
            let dof = nrows as f64 - ncols as f64;
            // Residue
            let res = y - x * &coeffs;
            // total residue, sum of squares
            let mse = (res.transpose() * &res).read(0, 0) / dof;
            // std err
            let std_err = (0..ncols)
                .map(|i| (mse * xtx_inv.read(i, i)).sqrt())
                .collect_vec();
            // T values
            let t_values = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b / se)
                .collect_vec();
            // P values
            let p_values = t_values
                .iter()
                .map(
                    |t| match crate::stats_utils::beta::student_t_sf(t.abs(), dof) {
                        Ok(p) => 2.0 * p,
                        Err(_) => f64::NAN,
                    },
                )
                .collect_vec();

            let t_alpha = crate::stats_utils::beta::student_t_ppf(0.975, dof);
            let ci_lower = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b - t_alpha * se)
                .collect_vec();
            let ci_upper = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b + t_alpha * se)
                .collect_vec();

            // Finalize
            let names_ca = name_builder.finish();
            let names_series = names_ca.into_series();
            let coeffs_series = Float64Chunked::from_slice("beta".into(), betas);
            let coeffs_series = coeffs_series.into_series();
            let stderr_series = Float64Chunked::from_vec("std_err".into(), std_err);
            let stderr_series = stderr_series.into_series();
            let t_series = Float64Chunked::from_vec("t".into(), t_values);
            let t_series = t_series.into_series();
            let p_series = Float64Chunked::from_vec("p>|t|".into(), p_values);
            let p_series = p_series.into_series();
            let lower = Float64Chunked::from_vec("0.025".into(), ci_lower);
            let lower = lower.into_series();
            let upper = Float64Chunked::from_vec("0.975".into(), ci_upper);
            let upper = upper.into_series();
            let out = StructChunked::from_series(
                "lstsq_report".into(),
                names_series.len(),
                [
                    &names_series,
                    &coeffs_series,
                    &stderr_series,
                    &t_series,
                    &p_series,
                    &lower,
                    &upper,
                ]
                .into_iter(),
            )?;
            Ok(out.into_series())
        }
        Err(e) => Err(e),
    }
}

#[polars_expr(output_type_func=report_output)]
fn pl_wls_report(inputs: &[Series], kwargs: LstsqKwargs) -> PolarsResult<Series> {
    let add_bias = kwargs.bias;
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    let weights = inputs[0].f64().unwrap();
    let weights = weights.cont_slice().unwrap();
    // index 0 is weights, 1 is target y. Skip them
    let mut name_builder =
        StringChunkedBuilder::new("features".into(), inputs.len() + (add_bias) as usize);
    for s in inputs[2..].iter().map(|s| s.name()) {
        name_builder.append_value(s);
    }
    if add_bias {
        name_builder.append_value("__bias__");
    }
    // Copy data
    // Target y is at index 1, weights 0
    match series_to_mat_for_lstsq(&inputs[1..], add_bias, null_policy) {
        Ok((mat, _)) => {
            let ncols = mat.ncols() - 1;
            let nrows = mat.nrows();

            let x = mat.slice(s![0..nrows, 1..]).into_faer();
            let y = mat.slice(s![0..nrows, 0..1]).into_faer();

            let w = faer::mat::from_row_major_slice(weights, x.nrows(), 1);
            let w = w.column_vector_as_diagonal();
            let xt = x.transpose();

            let xtwx = xt * w * x;
            let xtwy = xt * w * y;
            let qr = xtwx.col_piv_qr();
            let xtwx_inv = qr.inverse();
            let coeffs = qr.solve(xtwy);
            let betas = coeffs.col_as_slice(0);

            // Degree of Freedom
            let dof = nrows as f64 - ncols as f64;
            // Residue
            let res = y - x * &coeffs;
            let mse =
                (0..y.nrows()).fold(0., |acc, i| acc + weights[i] * res.read(i, 0).powi(2)) / dof;
            // std err
            let std_err = (0..ncols)
                .map(|i| (mse * xtwx_inv.read(i, i)).sqrt())
                .collect_vec();
            // T values
            let t_values = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b / se)
                .collect_vec();
            // P values
            let p_values = t_values
                .iter()
                .map(
                    |t| match crate::stats_utils::beta::student_t_sf(t.abs(), dof) {
                        Ok(p) => 2.0 * p,
                        Err(_) => f64::NAN,
                    },
                )
                .collect_vec();

            let t_alpha = crate::stats_utils::beta::student_t_ppf(0.975, dof);
            let ci_lower = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b - t_alpha * se)
                .collect_vec();
            let ci_upper = betas
                .iter()
                .zip(std_err.iter())
                .map(|(b, se)| b + t_alpha * se)
                .collect_vec();
            // Finalize
            let names_ca = name_builder.finish();
            let names_series = names_ca.into_series();
            let coeffs_series = Float64Chunked::from_slice("beta".into(), betas);
            let coeffs_series = coeffs_series.into_series();
            let stderr_series = Float64Chunked::from_vec("std_err".into(), std_err);
            let stderr_series = stderr_series.into_series();
            let t_series = Float64Chunked::from_vec("t".into(), t_values);
            let t_series = t_series.into_series();
            let p_series = Float64Chunked::from_vec("p>|t|".into(), p_values);
            let p_series = p_series.into_series();
            let lower = Float64Chunked::from_vec("0.025".into(), ci_lower);
            let lower = lower.into_series();
            let upper = Float64Chunked::from_vec("0.975".into(), ci_upper);
            let upper = upper.into_series();
            let out = StructChunked::from_series(
                "lstsq_report".into(),
                names_series.len(),
                [
                    &names_series,
                    &coeffs_series,
                    &stderr_series,
                    &t_series,
                    &p_series,
                    &lower,
                    &upper,
                ]
                .into_iter(),
            )?;
            Ok(out.into_series())
        }
        Err(e) => Err(e),
    }
}

// --- Rolling and Recursive

#[polars_expr(output_type_func=coeff_pred_output)]
fn pl_recursive_lstsq(inputs: &[Series], kwargs: SWWLstsqKwargs) -> PolarsResult<Series> {
    let n = kwargs.n; // Gauranteed n >= 1
    let add_bias = kwargs.bias;

    // Gauranteed in Python that this won't be SKIP. SKIP doesn't work now.
    let null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    // Target y is at index 0
    match series_to_mat_for_lstsq(inputs, add_bias, null_policy) {
        Ok((mat, mask)) => {
            // Solving Least Square
            let x = mat.slice(s![.., 1..]).into_faer();
            let y = mat.slice(s![.., 0..1]).into_faer();

            let coeffs = faer_recursive_lstsq(x, y, n, kwargs.lambda);
            let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
                ListPrimitiveChunkedBuilder::new(
                    "coeffs".into(),
                    mat.nrows(),
                    mat.ncols(),
                    DataType::Float64,
                );
            let mut pred_builder: PrimitiveChunkedBuilder<Float64Type> =
                PrimitiveChunkedBuilder::new("pred".into(), mat.nrows());

            // Fill or Skip strategy can drop nulls. Fill will drop null when y has nulls.
            // Skip will drop nulls whenever there is a null in the row.
            // Mask true means the row doesn't have nulls
            let has_nulls = (!&mask).any();
            if has_nulls {
                // Find the first index where we get n non-nulls.
                let mut new_n = 0;
                let mut m = 0;
                for v in mask.into_no_null_iter() {
                    new_n += v as usize;
                    if new_n >= n {
                        break;
                    }
                    m += 1;
                }
                for _ in 0..m {
                    builder.append_null();
                    pred_builder.append_null();
                }
                let mut i = 0;
                for should_keep in mask.into_no_null_iter().skip(m) {
                    if should_keep {
                        let coefficients = &coeffs[i];
                        let row = x.get(i..i + 1, ..);
                        let pred = (row * coefficients).read(0, 0);
                        let coef = coefficients.col_as_slice(0);
                        builder.append_slice(coef);
                        pred_builder.append_value(pred);
                        i += 1;
                    } else {
                        builder.append_null();
                        pred_builder.append_null();
                    }
                }
            } else {
                // n always > 1, guaranteed in Python
                let m = n.abs_diff(1);
                for _ in 0..m {
                    builder.append_null();
                    pred_builder.append_null();
                }
                for (i, coefficients) in coeffs.into_iter().enumerate() {
                    let row = x.get(m + i..m + i + 1, ..);
                    let pred = (row * &coefficients).read(0, 0);
                    let coef = coefficients.col_as_slice(0);
                    builder.append_slice(coef);
                    pred_builder.append_value(pred);
                }
            }

            let coef_out = builder.finish();
            let pred_out = pred_builder.finish();
            let ca = StructChunked::from_series(
                "".into(),
                coef_out.len(),
                [&coef_out.into_series(), &pred_out.into_series()].into_iter(),
            )?;
            Ok(ca.into_series())
        }
        Err(e) => Err(e),
    }
}

#[polars_expr(output_type_func=coeff_pred_output)] // They share the same output type
fn pl_rolling_lstsq(inputs: &[Series], kwargs: SWWLstsqKwargs) -> PolarsResult<Series> {
    let n = kwargs.n; // Gauranteed n >= 2
    let add_bias = kwargs.bias;

    let mut null_policy = NullPolicy::try_from(kwargs.null_policy)
        .map_err(|e| PolarsError::ComputeError(e.into()))?;

    // For SKIP, we use SKIP_WINDOW. For FILL(x), use FILL_WINDOW
    null_policy = match null_policy {
        NullPolicy::SKIP => NullPolicy::SKIP_WINDOW,
        NullPolicy::FILL(x) => NullPolicy::FILL_WINDOW(x),
        _ => null_policy,
    };

    // Target y is at index 0
    match series_to_mat_for_lstsq(inputs, add_bias, null_policy) {
        Ok((mat, mask)) => {
            let should_skip = match null_policy {
                NullPolicy::SKIP_WINDOW | NullPolicy::FILL_WINDOW(_) => (!&mask).any(),
                _ => false, // raise, ignore
            };
            let x = mat.slice(s![.., 1..]).into_faer();
            let y = mat.slice(s![.., 0..1]).into_faer();
            let coeffs = if should_skip {
                faer_rolling_skipping_lstsq(x, y, n, kwargs.min_size, kwargs.lambda)
            } else {
                faer_rolling_lstsq(x, y, n, kwargs.lambda)
            };

            let mut builder: ListPrimitiveChunkedBuilder<Float64Type> =
                ListPrimitiveChunkedBuilder::new(
                    "coeffs".into(),
                    mat.nrows(),
                    mat.ncols(),
                    DataType::Float64,
                );
            let mut pred_builder: PrimitiveChunkedBuilder<Float64Type> =
                PrimitiveChunkedBuilder::new("pred".into(), mat.nrows());

            let m = n - 1; // n >= 2 guaranteed in Python
            for _ in 0..m {
                builder.append_null();
                pred_builder.append_null();
            }

            if should_skip {
                // Skipped rows will have coeffs with shape (0, 0)
                for (i, coefficients) in coeffs.into_iter().enumerate() {
                    if coefficients.shape() == (0, 0) {
                        builder.append_null();
                        pred_builder.append_null();
                    } else {
                        let row = x.get(m + i..m + i + 1, ..);
                        let pred = (row * &coefficients).read(0, 0);
                        let coef = coefficients.col_as_slice(0);
                        builder.append_slice(coef);
                        pred_builder.append_value(pred);
                    }
                }
            } else {
                // nothing is skipped. All coeffs must be valid.
                for (i, coefficients) in coeffs.into_iter().enumerate() {
                    let row = x.get(m + i..m + i + 1, ..);
                    let pred = (row * &coefficients).read(0, 0);
                    let coef = coefficients.col_as_slice(0);
                    builder.append_slice(coef);
                    pred_builder.append_value(pred);
                }
            }

            let coef_out = builder.finish();
            let pred_out = pred_builder.finish();
            let ca = StructChunked::from_series(
                "".into(),
                coef_out.len(),
                [&coef_out.into_series(), &pred_out.into_series()].into_iter(),
            )?;
            Ok(ca.into_series())
        }
        Err(e) => Err(e),
    }
}
