// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{BTreeSet, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_recursion::async_recursion;
use catalog::table_source::DfTableSourceProvider;
use datafusion::common::{DFSchemaRef, OwnedTableReference, Result as DfResult};
use datafusion::datasource::DefaultTableSource;
use datafusion::logical_expr::expr::AggregateFunction;
use datafusion::logical_expr::expr_rewriter::normalize_cols;
use datafusion::logical_expr::{
    AggregateFunction as AggregateFunctionEnum, BinaryExpr, BuiltinScalarFunction, Cast, Extension,
    LogicalPlan, LogicalPlanBuilder, Operator, ScalarUDF,
};
use datafusion::optimizer::utils;
use datafusion::prelude::{Column, Expr as DfExpr, JoinType};
use datafusion::scalar::ScalarValue;
use datafusion::sql::TableReference;
use datatypes::arrow::datatypes::DataType as ArrowDataType;
use promql_parser::label::{MatchOp, Matcher, Matchers, METRIC_NAME};
use promql_parser::parser::{
    token, AggregateExpr, BinaryExpr as PromBinaryExpr, Call, EvalStmt, Expr as PromExpr, Function,
    LabelModifier, MatrixSelector, NumberLiteral, Offset, ParenExpr, StringLiteral, SubqueryExpr,
    TokenType, UnaryExpr, VectorSelector,
};
use snafu::{ensure, OptionExt, ResultExt};
use table::table::adapter::DfTableProviderAdapter;

use crate::error::{
    CatalogSnafu, ColumnNotFoundSnafu, DataFusionPlanningSnafu, ExpectExprSnafu,
    ExpectRangeSelectorSnafu, MultipleVectorSnafu, Result, TableNameNotFoundSnafu,
    TimeIndexNotFoundSnafu, UnexpectedPlanExprSnafu, UnexpectedTokenSnafu, UnknownTableSnafu,
    UnsupportedExprSnafu, ValueNotFoundSnafu, ZeroRangeSelectorSnafu,
};
use crate::extension_plan::{
    EmptyMetric, InstantManipulate, Millisecond, RangeManipulate, SeriesDivide, SeriesNormalize,
};
use crate::functions::{
    AbsentOverTime, AvgOverTime, Changes, CountOverTime, Delta, Deriv, HoltWinters, IDelta,
    Increase, LastOverTime, MaxOverTime, MinOverTime, PredictLinear, PresentOverTime,
    QuantileOverTime, Rate, Resets, StddevOverTime, StdvarOverTime, SumOverTime,
};

const LEFT_PLAN_JOIN_ALIAS: &str = "lhs";

/// `time()` function in PromQL.
const SPECIAL_TIME_FUNCTION: &str = "time";

/// default value column name for empty metric
const DEFAULT_FIELD_COLUMN: &str = "value";

/// Special modifier to project field columns under multi-field mode
const FIELD_COLUMN_MATCHER: &str = "__field__";

#[derive(Default, Debug, Clone)]
struct PromPlannerContext {
    // query parameters
    start: Millisecond,
    end: Millisecond,
    interval: Millisecond,
    lookback_delta: Millisecond,

    // planner states
    table_name: Option<String>,
    time_index_column: Option<String>,
    field_columns: Vec<String>,
    tag_columns: Vec<String>,
    field_column_matcher: Option<Vec<Matcher>>,
    /// The range in millisecond of range selector. None if there is no range selector.
    range: Option<Millisecond>,
}

impl PromPlannerContext {
    fn from_eval_stmt(stmt: &EvalStmt) -> Self {
        Self {
            start: stmt.start.duration_since(UNIX_EPOCH).unwrap().as_millis() as _,
            end: stmt.end.duration_since(UNIX_EPOCH).unwrap().as_millis() as _,
            interval: stmt.interval.as_millis() as _,
            lookback_delta: stmt.lookback_delta.as_millis() as _,
            ..Default::default()
        }
    }
}

pub struct PromPlanner {
    table_provider: DfTableSourceProvider,
    ctx: PromPlannerContext,
}

impl PromPlanner {
    pub async fn stmt_to_plan(
        table_provider: DfTableSourceProvider,
        stmt: EvalStmt,
    ) -> Result<LogicalPlan> {
        let mut planner = Self {
            table_provider,
            ctx: PromPlannerContext::from_eval_stmt(&stmt),
        };
        planner.prom_expr_to_plan(stmt.expr).await
    }

    #[async_recursion]
    pub async fn prom_expr_to_plan(&mut self, prom_expr: PromExpr) -> Result<LogicalPlan> {
        let res = match &prom_expr {
            PromExpr::Aggregate(AggregateExpr {
                op,
                expr,
                // TODO(ruihang): support param
                param: _param,
                modifier,
            }) => {
                let input = self.prom_expr_to_plan(*expr.clone()).await?;

                // calculate columns to group by
                // Need to append time index column into group by columns
                let group_exprs = modifier
                    .as_ref()
                    .map_or(Ok(vec![self.create_time_index_column_expr()?]), |m| {
                        self.agg_modifier_to_col(input.schema(), m)
                    })?;

                // convert op and value columns to aggregate exprs
                let aggr_exprs = self.create_aggregate_exprs(*op, &input)?;

                // remove time index column from context
                self.ctx.time_index_column = None;

                // create plan
                let group_sort_expr = group_exprs
                    .clone()
                    .into_iter()
                    .map(|expr| expr.sort(true, false));
                LogicalPlanBuilder::from(input)
                    .aggregate(group_exprs, aggr_exprs)
                    .context(DataFusionPlanningSnafu)?
                    .sort(group_sort_expr)
                    .context(DataFusionPlanningSnafu)?
                    .build()
                    .context(DataFusionPlanningSnafu)?
            }
            PromExpr::Unary(UnaryExpr { expr }) => {
                // Unary Expr in PromQL implys the `-` operator
                let input = self.prom_expr_to_plan(*expr.clone()).await?;
                self.projection_for_each_field_column(input, |col| {
                    Ok(DfExpr::Negative(Box::new(DfExpr::Column(col.into()))))
                })?
            }
            PromExpr::Binary(PromBinaryExpr {
                lhs,
                rhs,
                op,
                modifier,
            }) => {
                // if set to true, comparison operator will return 0/1 (for true/false) instead of
                // filter on the result column
                let should_return_bool = if let Some(m) = modifier {
                    m.return_bool
                } else {
                    false
                };
                let is_comparison_op = Self::is_token_a_comparison_op(*op);

                // we should build a filter plan here if the op is comparison op and need not
                // to return 0/1. Otherwise, we should build a projection plan
                match (
                    Self::try_build_literal_expr(lhs),
                    Self::try_build_literal_expr(rhs),
                ) {
                    // TODO(ruihang): handle literal-only expressions
                    (Some(_lhs), Some(_rhs)) => UnsupportedExprSnafu {
                        name: "Literal-only expression",
                    }
                    .fail()?,
                    // lhs is a literal, rhs is a column
                    (Some(expr), None) => {
                        let input = self.prom_expr_to_plan(*rhs.clone()).await?;
                        let bin_expr_builder = |col: &String| {
                            let mut binary_expr = DfExpr::BinaryExpr(BinaryExpr {
                                left: Box::new(expr.clone()),
                                op: Self::prom_token_to_binary_op(*op)?,
                                right: Box::new(DfExpr::Column(col.into())),
                            });
                            if is_comparison_op && should_return_bool {
                                binary_expr = DfExpr::Cast(Cast {
                                    expr: Box::new(binary_expr),
                                    data_type: ArrowDataType::Float64,
                                });
                            }
                            Ok(binary_expr)
                        };
                        if is_comparison_op && !should_return_bool {
                            self.filter_on_field_column(input, bin_expr_builder)?
                        } else {
                            self.projection_for_each_field_column(input, bin_expr_builder)?
                        }
                    }
                    // lhs is a column, rhs is a literal
                    (None, Some(expr)) => {
                        let input = self.prom_expr_to_plan(*lhs.clone()).await?;
                        let bin_expr_builder = |col: &String| {
                            let mut binary_expr = DfExpr::BinaryExpr(BinaryExpr {
                                left: Box::new(DfExpr::Column(col.into())),
                                op: Self::prom_token_to_binary_op(*op)?,
                                right: Box::new(expr.clone()),
                            });
                            if is_comparison_op && should_return_bool {
                                binary_expr = DfExpr::Cast(Cast {
                                    expr: Box::new(binary_expr),
                                    data_type: ArrowDataType::Float64,
                                });
                            }
                            Ok(binary_expr)
                        };
                        if is_comparison_op && !should_return_bool {
                            self.filter_on_field_column(input, bin_expr_builder)?
                        } else {
                            self.projection_for_each_field_column(input, bin_expr_builder)?
                        }
                    }
                    // both are columns. join them on time index
                    (None, None) => {
                        let left_input = self.prom_expr_to_plan(*lhs.clone()).await?;
                        let left_field_columns = self.ctx.field_columns.clone();
                        let left_schema = left_input.schema().clone();

                        let right_input = self.prom_expr_to_plan(*rhs.clone()).await?;
                        let right_field_columns = self.ctx.field_columns.clone();
                        let right_schema = right_input.schema().clone();

                        let mut field_columns =
                            left_field_columns.iter().zip(right_field_columns.iter());
                        // the new ctx.field_columns for the generated join plan
                        let join_plan = self.join_on_non_field_columns(left_input, right_input)?;
                        let bin_expr_builder = |_: &String| {
                            let (left_col_name, right_col_name) = field_columns.next().unwrap();
                            let left_col = left_schema
                                .field_with_name(None, left_col_name)
                                .context(DataFusionPlanningSnafu)?
                                .qualified_column();
                            let right_col = right_schema
                                .field_with_name(None, right_col_name)
                                .context(DataFusionPlanningSnafu)?
                                .qualified_column();

                            let mut binary_expr = DfExpr::BinaryExpr(BinaryExpr {
                                left: Box::new(DfExpr::Column(left_col)),
                                op: Self::prom_token_to_binary_op(*op)?,
                                right: Box::new(DfExpr::Column(right_col)),
                            });
                            if is_comparison_op && should_return_bool {
                                binary_expr = DfExpr::Cast(Cast {
                                    expr: Box::new(binary_expr),
                                    data_type: ArrowDataType::Float64,
                                });
                            }
                            Ok(binary_expr)
                        };
                        if is_comparison_op && !should_return_bool {
                            self.filter_on_field_column(join_plan, bin_expr_builder)?
                        } else {
                            self.projection_for_each_field_column(join_plan, bin_expr_builder)?
                        }
                    }
                }
            }
            PromExpr::Paren(ParenExpr { expr }) => self.prom_expr_to_plan(*expr.clone()).await?,
            PromExpr::Subquery(SubqueryExpr { .. }) => UnsupportedExprSnafu {
                name: "Prom Subquery",
            }
            .fail()?,
            PromExpr::NumberLiteral(NumberLiteral { .. }) => UnsupportedExprSnafu {
                name: "Prom Number Literal",
            }
            .fail()?,
            PromExpr::StringLiteral(StringLiteral { .. }) => UnsupportedExprSnafu {
                name: "Prom String Literal",
            }
            .fail()?,
            PromExpr::VectorSelector(VectorSelector {
                name: _,
                offset,
                matchers,
                at: _,
            }) => {
                let matchers = self.preprocess_label_matchers(matchers)?;
                self.setup_context().await?;
                let normalize = self
                    .selector_to_series_normalize_plan(offset, matchers, false)
                    .await?;
                let manipulate = InstantManipulate::new(
                    self.ctx.start,
                    self.ctx.end,
                    self.ctx.lookback_delta,
                    self.ctx.interval,
                    self.ctx
                        .time_index_column
                        .clone()
                        .expect("time index should be set in `setup_context`"),
                    self.ctx.field_columns.get(0).cloned(),
                    normalize,
                );
                LogicalPlan::Extension(Extension {
                    node: Arc::new(manipulate),
                })
            }
            PromExpr::MatrixSelector(MatrixSelector {
                vector_selector,
                range,
            }) => {
                let VectorSelector {
                    offset, matchers, ..
                } = vector_selector;
                let matchers = self.preprocess_label_matchers(matchers)?;
                self.setup_context().await?;

                ensure!(!range.is_zero(), ZeroRangeSelectorSnafu);
                let range_ms = range.as_millis() as _;
                self.ctx.range = Some(range_ms);

                let normalize = self
                    .selector_to_series_normalize_plan(offset, matchers, true)
                    .await?;
                let manipulate = RangeManipulate::new(
                    self.ctx.start,
                    self.ctx.end,
                    self.ctx.interval,
                    // TODO(ruihang): convert via Timestamp datatypes to support different time units
                    range_ms,
                    self.ctx
                        .time_index_column
                        .clone()
                        .expect("time index should be set in `setup_context`"),
                    self.ctx.field_columns.clone(),
                    normalize,
                )
                .context(DataFusionPlanningSnafu)?;

                LogicalPlan::Extension(Extension {
                    node: Arc::new(manipulate),
                })
            }
            PromExpr::Call(Call { func, args }) => {
                // TODO(ruihang): refactor this, transform the AST in advance to include an empty metric table.
                if func.name == SPECIAL_TIME_FUNCTION {
                    self.ctx.time_index_column = Some(SPECIAL_TIME_FUNCTION.to_string());
                    self.ctx.field_columns = vec![DEFAULT_FIELD_COLUMN.to_string()];
                    self.ctx.table_name = Some(String::new());

                    return Ok(LogicalPlan::Extension(Extension {
                        node: Arc::new(
                            EmptyMetric::new(
                                self.ctx.start,
                                self.ctx.end,
                                self.ctx.interval,
                                SPECIAL_TIME_FUNCTION.to_string(),
                                DEFAULT_FIELD_COLUMN.to_string(),
                            )
                            .context(DataFusionPlanningSnafu)?,
                        ),
                    }));
                }

                let args = self.create_function_args(&args.args)?;
                let input = self
                    .prom_expr_to_plan(args.input.with_context(|| ExpectExprSnafu {
                        expr: prom_expr.clone(),
                    })?)
                    .await?;
                let mut func_exprs = self.create_function_expr(func, args.literals)?;
                func_exprs.insert(0, self.create_time_index_column_expr()?);
                func_exprs.extend_from_slice(&self.create_tag_column_exprs()?);

                LogicalPlanBuilder::from(input)
                    .project(func_exprs)
                    .context(DataFusionPlanningSnafu)?
                    .filter(self.create_empty_values_filter_expr()?)
                    .context(DataFusionPlanningSnafu)?
                    .build()
                    .context(DataFusionPlanningSnafu)?
            }
            PromExpr::Extension(_) => UnsupportedExprSnafu {
                name: "Prom Extension",
            }
            .fail()?,
        };
        Ok(res)
    }

    /// Extract metric name from `__name__` matcher and set it into [PromPlannerContext].
    /// Returns a new [Matchers] that doesn't contains metric name matcher.
    fn preprocess_label_matchers(&mut self, label_matchers: &Matchers) -> Result<Matchers> {
        let mut matchers = HashSet::new();
        for matcher in &label_matchers.matchers {
            // TODO(ruihang): support other metric match ops
            if matcher.name == METRIC_NAME && matches!(matcher.op, MatchOp::Equal) {
                self.ctx.table_name = Some(matcher.value.clone());
            } else if matcher.name == FIELD_COLUMN_MATCHER {
                self.ctx
                    .field_column_matcher
                    .get_or_insert_default()
                    .push(matcher.clone());
            } else {
                matchers.insert(matcher.clone());
            }
        }
        Ok(Matchers { matchers })
    }

    async fn selector_to_series_normalize_plan(
        &mut self,
        offset: &Option<Offset>,
        label_matchers: Matchers,
        is_range_selector: bool,
    ) -> Result<LogicalPlan> {
        let table_name = self.ctx.table_name.clone().unwrap();

        // make filter exprs
        let offset_duration = match offset {
            Some(Offset::Pos(duration)) => duration.as_millis() as Millisecond,
            Some(Offset::Neg(duration)) => -(duration.as_millis() as Millisecond),
            None => 0,
        };
        let range_ms = self.ctx.range.unwrap_or_default();
        let mut scan_filters = self.matchers_to_expr(label_matchers.clone())?;
        scan_filters.push(self.create_time_index_column_expr()?.gt_eq(DfExpr::Literal(
            ScalarValue::TimestampMillisecond(
                Some(self.ctx.start - offset_duration - self.ctx.lookback_delta - range_ms),
                None,
            ),
        )));
        scan_filters.push(self.create_time_index_column_expr()?.lt_eq(DfExpr::Literal(
            ScalarValue::TimestampMillisecond(
                Some(self.ctx.end - offset_duration + self.ctx.lookback_delta),
                None,
            ),
        )));

        // make table scan with filter exprs
        let mut table_scan = self
            .create_table_scan_plan(&table_name, scan_filters.clone())
            .await?;

        // make a projection plan if there is any `__field__` matcher
        if let Some(field_matchers) = &self.ctx.field_column_matcher {
            let col_set = self.ctx.field_columns.iter().collect::<HashSet<_>>();
            // opt-in set
            let mut result_set = HashSet::new();
            // opt-out set
            let mut reverse_set = HashSet::new();
            for matcher in field_matchers {
                match &matcher.op {
                    MatchOp::Equal => {
                        if col_set.contains(&matcher.value) {
                            result_set.insert(matcher.value.clone());
                        } else {
                            return Err(ColumnNotFoundSnafu {
                                col: self.ctx.table_name.clone().unwrap(),
                            }
                            .build());
                        }
                    }
                    MatchOp::NotEqual => {
                        if col_set.contains(&matcher.value) {
                            reverse_set.insert(matcher.value.clone());
                        } else {
                            return Err(ValueNotFoundSnafu {
                                table: self.ctx.table_name.clone().unwrap(),
                            }
                            .build());
                        }
                    }
                    MatchOp::Re(regex) => {
                        for col in &self.ctx.field_columns {
                            if regex.is_match(col) {
                                result_set.insert(col.clone());
                            }
                        }
                    }
                    MatchOp::NotRe(regex) => {
                        for col in &self.ctx.field_columns {
                            if regex.is_match(col) {
                                reverse_set.insert(col.clone());
                            }
                        }
                    }
                }
            }
            // merge two set
            if result_set.is_empty() {
                result_set = col_set.into_iter().cloned().collect();
            }
            for col in reverse_set {
                result_set.remove(&col);
            }

            self.ctx.field_columns = result_set.iter().cloned().collect();
            let exprs = result_set
                .into_iter()
                .map(|col| DfExpr::Column(col.into()))
                .chain(self.create_tag_column_exprs()?.into_iter())
                .chain(Some(self.create_time_index_column_expr()?))
                .collect::<Vec<_>>();
            // reuse this variable for simplicity
            table_scan = LogicalPlanBuilder::from(table_scan)
                .project(exprs)
                .context(DataFusionPlanningSnafu)?
                .build()
                .context(DataFusionPlanningSnafu)?;
        }

        // make filter and sort plan
        let mut plan_builder = LogicalPlanBuilder::from(table_scan);
        let accurate_filters = self.matchers_to_expr(label_matchers)?;
        if !accurate_filters.is_empty() {
            plan_builder = plan_builder
                .filter(utils::conjunction(accurate_filters).unwrap())
                .context(DataFusionPlanningSnafu)?;
        }
        let sort_plan = plan_builder
            .sort(self.create_tag_and_time_index_column_sort_exprs()?)
            .context(DataFusionPlanningSnafu)?
            .build()
            .context(DataFusionPlanningSnafu)?;

        // make divide plan
        let divide_plan = LogicalPlan::Extension(Extension {
            node: Arc::new(SeriesDivide::new(self.ctx.tag_columns.clone(), sort_plan)),
        });

        // make series_normalize plan
        let series_normalize = SeriesNormalize::new(
            offset_duration,
            self.ctx
                .time_index_column
                .clone()
                .with_context(|| TimeIndexNotFoundSnafu { table: table_name })?,
            is_range_selector,
            divide_plan,
        );
        let logical_plan = LogicalPlan::Extension(Extension {
            node: Arc::new(series_normalize),
        });

        Ok(logical_plan)
    }

    /// Convert [AggModifier] to [Column] exprs for aggregation.
    /// Timestamp column and tag columns will be included.
    ///
    /// # Side effect
    ///
    /// This method will also change the tag columns in ctx.
    fn agg_modifier_to_col(
        &mut self,
        input_schema: &DFSchemaRef,
        modifier: &LabelModifier,
    ) -> Result<Vec<DfExpr>> {
        match modifier {
            LabelModifier::Include(labels) => {
                let mut exprs = Vec::with_capacity(labels.len());
                for label in labels {
                    // nonexistence label will be ignored
                    if let Ok(field) = input_schema.field_with_unqualified_name(label) {
                        exprs.push(DfExpr::Column(Column::from(field.name())));
                    }
                }

                // change the tag columns in context
                self.ctx.tag_columns = labels.iter().cloned().collect();

                // add timestamp column
                exprs.push(self.create_time_index_column_expr()?);

                Ok(exprs)
            }
            LabelModifier::Exclude(labels) => {
                let mut all_fields = input_schema
                    .fields()
                    .iter()
                    .map(|f| f.name())
                    .collect::<BTreeSet<_>>();

                // remove "without"-ed fields
                // nonexistence label will be ignored
                for label in labels {
                    all_fields.remove(label);
                }

                // remove time index and value fields
                if let Some(time_index) = &self.ctx.time_index_column {
                    all_fields.remove(time_index);
                }
                for value in &self.ctx.field_columns {
                    all_fields.remove(value);
                }

                // change the tag columns in context
                self.ctx.tag_columns = all_fields.iter().map(|col| (*col).clone()).collect();

                // collect remaining fields and convert to col expr
                let mut exprs = all_fields
                    .into_iter()
                    .map(|c| DfExpr::Column(Column::from(c)))
                    .collect::<Vec<_>>();

                // add timestamp column
                exprs.push(self.create_time_index_column_expr()?);

                Ok(exprs)
            }
        }
    }

    // TODO(ruihang): ignore `MetricNameLabel` (`__name__`) matcher
    fn matchers_to_expr(&self, label_matchers: Matchers) -> Result<Vec<DfExpr>> {
        let mut exprs = Vec::with_capacity(label_matchers.matchers.len());
        for matcher in label_matchers.matchers {
            let col = DfExpr::Column(Column::from_name(matcher.name));
            let lit = DfExpr::Literal(ScalarValue::Utf8(Some(matcher.value)));
            let expr = match matcher.op {
                MatchOp::Equal => col.eq(lit),
                MatchOp::NotEqual => col.not_eq(lit),
                MatchOp::Re(_) => DfExpr::BinaryExpr(BinaryExpr {
                    left: Box::new(col),
                    op: Operator::RegexMatch,
                    right: Box::new(lit),
                }),
                MatchOp::NotRe(_) => DfExpr::BinaryExpr(BinaryExpr {
                    left: Box::new(col),
                    op: Operator::RegexNotMatch,
                    right: Box::new(lit),
                }),
            };
            exprs.push(expr);
        }

        Ok(exprs)
    }

    async fn create_table_scan_plan(
        &mut self,
        table_name: &str,
        filter: Vec<DfExpr>,
    ) -> Result<LogicalPlan> {
        let table_ref = OwnedTableReference::bare(table_name.to_string());
        let provider = self
            .table_provider
            .resolve_table(table_ref.clone())
            .await
            .context(CatalogSnafu)?;
        let result = LogicalPlanBuilder::scan_with_filters(table_ref, provider, None, filter)
            .context(DataFusionPlanningSnafu)?
            .build()
            .context(DataFusionPlanningSnafu)?;
        Ok(result)
    }

    /// Setup [PromPlannerContext]'s state fields.
    async fn setup_context(&mut self) -> Result<()> {
        let table_name = self
            .ctx
            .table_name
            .clone()
            .context(TableNameNotFoundSnafu)?;
        let table = self
            .table_provider
            .resolve_table(TableReference::bare(&table_name))
            .await
            .context(CatalogSnafu)?
            .as_any()
            .downcast_ref::<DefaultTableSource>()
            .context(UnknownTableSnafu)?
            .table_provider
            .as_any()
            .downcast_ref::<DfTableProviderAdapter>()
            .context(UnknownTableSnafu)?
            .table();

        // set time index column name
        let time_index = table
            .schema()
            .timestamp_column()
            .with_context(|| TimeIndexNotFoundSnafu { table: table_name })?
            .name
            .clone();
        self.ctx.time_index_column = Some(time_index);

        // set values columns
        let values = table
            .table_info()
            .meta
            .field_column_names()
            .cloned()
            .collect();
        self.ctx.field_columns = values;

        // set primary key (tag) columns
        let tags = table
            .table_info()
            .meta
            .row_key_column_names()
            .cloned()
            .collect();
        self.ctx.tag_columns = tags;

        Ok(())
    }

    // TODO(ruihang): insert column expr
    fn create_function_args(&self, args: &[Box<PromExpr>]) -> Result<FunctionArgs> {
        let mut result = FunctionArgs::default();

        for arg in args {
            match *arg.clone() {
                PromExpr::Aggregate(_)
                | PromExpr::Unary(_)
                | PromExpr::Binary(_)
                | PromExpr::Paren(_)
                | PromExpr::Subquery(_)
                | PromExpr::VectorSelector(_)
                | PromExpr::MatrixSelector(_)
                | PromExpr::Extension(_)
                | PromExpr::Call(_) => {
                    if result.input.replace(*arg.clone()).is_some() {
                        MultipleVectorSnafu { expr: *arg.clone() }.fail()?;
                    }
                }

                PromExpr::NumberLiteral(NumberLiteral { val, .. }) => {
                    let scalar_value = ScalarValue::Float64(Some(val));
                    result.literals.push(DfExpr::Literal(scalar_value));
                }
                PromExpr::StringLiteral(StringLiteral { val, .. }) => {
                    let scalar_value = ScalarValue::Utf8(Some(val));
                    result.literals.push(DfExpr::Literal(scalar_value));
                }
            }
        }

        Ok(result)
    }

    /// # Side Effects
    ///
    /// This method will update [PromPlannerContext]'s value fields.
    fn create_function_expr(
        &mut self,
        func: &Function,
        mut other_input_exprs: Vec<DfExpr>,
    ) -> Result<Vec<DfExpr>> {
        // TODO(ruihang): check function args list

        // TODO(ruihang): set this according to in-param list
        let field_column_pos = 0;
        let scalar_func = match func.name {
            "increase" => ScalarFunc::ExtrapolateUdf(Increase::scalar_udf(
                self.ctx.range.context(ExpectRangeSelectorSnafu)?,
            )),
            "rate" => ScalarFunc::ExtrapolateUdf(Rate::scalar_udf(
                self.ctx.range.context(ExpectRangeSelectorSnafu)?,
            )),
            "delta" => ScalarFunc::ExtrapolateUdf(Delta::scalar_udf(
                self.ctx.range.context(ExpectRangeSelectorSnafu)?,
            )),
            "idelta" => ScalarFunc::Udf(IDelta::<false>::scalar_udf()),
            "irate" => ScalarFunc::Udf(IDelta::<true>::scalar_udf()),
            "resets" => ScalarFunc::Udf(Resets::scalar_udf()),
            "changes" => ScalarFunc::Udf(Changes::scalar_udf()),
            "deriv" => ScalarFunc::Udf(Deriv::scalar_udf()),
            "avg_over_time" => ScalarFunc::Udf(AvgOverTime::scalar_udf()),
            "min_over_time" => ScalarFunc::Udf(MinOverTime::scalar_udf()),
            "max_over_time" => ScalarFunc::Udf(MaxOverTime::scalar_udf()),
            "sum_over_time" => ScalarFunc::Udf(SumOverTime::scalar_udf()),
            "count_over_time" => ScalarFunc::Udf(CountOverTime::scalar_udf()),
            "last_over_time" => ScalarFunc::Udf(LastOverTime::scalar_udf()),
            "absent_over_time" => ScalarFunc::Udf(AbsentOverTime::scalar_udf()),
            "present_over_time" => ScalarFunc::Udf(PresentOverTime::scalar_udf()),
            "stddev_over_time" => ScalarFunc::Udf(StddevOverTime::scalar_udf()),
            "stdvar_over_time" => ScalarFunc::Udf(StdvarOverTime::scalar_udf()),
            "quantile_over_time" => {
                let quantile_expr = match other_input_exprs.get(0) {
                    Some(DfExpr::Literal(ScalarValue::Float64(Some(quantile)))) => *quantile,
                    other => UnexpectedPlanExprSnafu {
                        desc: format!("expect f64 literal as quantile, but found {:?}", other),
                    }
                    .fail()?,
                };
                ScalarFunc::Udf(QuantileOverTime::scalar_udf(quantile_expr))
            }
            "predict_linear" => {
                let t_expr = match other_input_exprs.get(0) {
                    Some(DfExpr::Literal(ScalarValue::Time64Microsecond(Some(t)))) => *t,
                    other => UnexpectedPlanExprSnafu {
                        desc: format!("expect i64 literal as t, but found {:?}", other),
                    }
                    .fail()?,
                };
                ScalarFunc::Udf(PredictLinear::scalar_udf(t_expr))
            }
            "holt_winters" => {
                let sf_exp = match other_input_exprs.get(0) {
                    Some(DfExpr::Literal(ScalarValue::Float64(Some(sf)))) => *sf,
                    other => UnexpectedPlanExprSnafu {
                        desc: format!(
                            "expect f64 literal as smoothing factor, but found {:?}",
                            other
                        ),
                    }
                    .fail()?,
                };
                let tf_exp = match other_input_exprs.get(1) {
                    Some(DfExpr::Literal(ScalarValue::Float64(Some(tf)))) => *tf,
                    other => UnexpectedPlanExprSnafu {
                        desc: format!("expect f64 literal as trend factor, but found {:?}", other),
                    }
                    .fail()?,
                };
                ScalarFunc::Udf(HoltWinters::scalar_udf(sf_exp, tf_exp))
            }
            _ => ScalarFunc::DataFusionBuiltin(
                BuiltinScalarFunction::from_str(func.name).map_err(|_| {
                    UnsupportedExprSnafu {
                        name: func.name.to_string(),
                    }
                    .build()
                })?,
            ),
        };

        // TODO(ruihang): handle those functions doesn't require input
        let mut exprs = Vec::with_capacity(self.ctx.field_columns.len());
        for value in &self.ctx.field_columns {
            let col_expr = DfExpr::Column(Column::from_name(value));

            match scalar_func.clone() {
                ScalarFunc::DataFusionBuiltin(fun) => {
                    other_input_exprs.insert(field_column_pos, col_expr);
                    let fn_expr = DfExpr::ScalarFunction {
                        fun,
                        args: other_input_exprs.clone(),
                    };
                    exprs.push(fn_expr);
                    other_input_exprs.remove(field_column_pos);
                }
                ScalarFunc::Udf(fun) => {
                    let ts_range_expr = DfExpr::Column(Column::from_name(
                        RangeManipulate::build_timestamp_range_name(
                            self.ctx.time_index_column.as_ref().unwrap(),
                        ),
                    ));
                    other_input_exprs.insert(field_column_pos, ts_range_expr);
                    other_input_exprs.insert(field_column_pos + 1, col_expr);
                    let fn_expr = DfExpr::ScalarUDF {
                        fun: Arc::new(fun),
                        args: other_input_exprs.clone(),
                    };
                    exprs.push(fn_expr);
                    other_input_exprs.remove(field_column_pos + 1);
                    other_input_exprs.remove(field_column_pos);
                }
                ScalarFunc::ExtrapolateUdf(fun) => {
                    let ts_range_expr = DfExpr::Column(Column::from_name(
                        RangeManipulate::build_timestamp_range_name(
                            self.ctx.time_index_column.as_ref().unwrap(),
                        ),
                    ));
                    other_input_exprs.insert(field_column_pos, ts_range_expr);
                    other_input_exprs.insert(field_column_pos + 1, col_expr);
                    other_input_exprs
                        .insert(field_column_pos + 2, self.create_time_index_column_expr()?);
                    let fn_expr = DfExpr::ScalarUDF {
                        fun: Arc::new(fun),
                        args: other_input_exprs.clone(),
                    };
                    exprs.push(fn_expr);
                    other_input_exprs.remove(field_column_pos + 2);
                    other_input_exprs.remove(field_column_pos + 1);
                    other_input_exprs.remove(field_column_pos);
                }
            }
        }

        // update value columns' name, and alias them to remove qualifiers
        let mut new_field_columns = Vec::with_capacity(exprs.len());
        exprs = exprs
            .into_iter()
            .map(|expr| {
                let display_name = expr.display_name()?;
                new_field_columns.push(display_name.clone());
                Ok(expr.alias(display_name))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .context(DataFusionPlanningSnafu)?;
        self.ctx.field_columns = new_field_columns;

        Ok(exprs)
    }

    fn create_time_index_column_expr(&self) -> Result<DfExpr> {
        Ok(DfExpr::Column(Column::from_name(
            self.ctx
                .time_index_column
                .clone()
                .with_context(|| TimeIndexNotFoundSnafu { table: "unknown" })?,
        )))
    }

    fn create_tag_column_exprs(&self) -> Result<Vec<DfExpr>> {
        let mut result = Vec::with_capacity(self.ctx.tag_columns.len());
        for tag in &self.ctx.tag_columns {
            let expr = DfExpr::Column(Column::from_name(tag));
            result.push(expr);
        }
        Ok(result)
    }

    fn create_tag_and_time_index_column_sort_exprs(&self) -> Result<Vec<DfExpr>> {
        let mut result = self
            .ctx
            .tag_columns
            .iter()
            .map(|col| DfExpr::Column(Column::from_name(col)).sort(false, false))
            .collect::<Vec<_>>();
        result.push(self.create_time_index_column_expr()?.sort(false, false));
        Ok(result)
    }

    fn create_empty_values_filter_expr(&self) -> Result<DfExpr> {
        let mut exprs = Vec::with_capacity(self.ctx.field_columns.len());
        for value in &self.ctx.field_columns {
            let expr = DfExpr::Column(Column::from_name(value)).is_not_null();
            exprs.push(expr);
        }

        utils::conjunction(exprs.into_iter()).context(ValueNotFoundSnafu {
            table: self.ctx.table_name.clone().unwrap(),
        })
    }

    /// Create [DfExpr::AggregateFunction] expr for each value column with given aggregate function.
    ///
    /// # Side effect
    ///
    /// This method will update value columns in context to the new value columns created by
    /// aggregate function.
    fn create_aggregate_exprs(
        &mut self,
        op: TokenType,
        input_plan: &LogicalPlan,
    ) -> Result<Vec<DfExpr>> {
        let aggr = match op.id() {
            token::T_SUM => AggregateFunctionEnum::Sum,
            token::T_AVG => AggregateFunctionEnum::Avg,
            token::T_COUNT => AggregateFunctionEnum::Count,
            token::T_MIN => AggregateFunctionEnum::Min,
            token::T_MAX => AggregateFunctionEnum::Max,
            token::T_GROUP => AggregateFunctionEnum::Grouping,
            token::T_STDDEV => AggregateFunctionEnum::StddevPop,
            token::T_STDVAR => AggregateFunctionEnum::VariancePop,
            token::T_TOPK | token::T_BOTTOMK | token::T_COUNT_VALUES | token::T_QUANTILE => {
                UnsupportedExprSnafu {
                    name: format!("{op:?}"),
                }
                .fail()?
            }
            _ => UnexpectedTokenSnafu { token: op }.fail()?,
        };

        // perform aggregate operation to each value column
        let exprs: Vec<DfExpr> = self
            .ctx
            .field_columns
            .iter()
            .map(|col| {
                DfExpr::AggregateFunction(AggregateFunction {
                    fun: aggr.clone(),
                    args: vec![DfExpr::Column(Column::from_name(col))],
                    distinct: false,
                    filter: None,
                })
            })
            .collect();

        // update value column name according to the aggregators
        let mut new_field_columns = Vec::with_capacity(self.ctx.field_columns.len());
        let normalized_exprs =
            normalize_cols(exprs.iter().cloned(), input_plan).context(DataFusionPlanningSnafu)?;
        for expr in normalized_exprs {
            new_field_columns.push(expr.display_name().context(DataFusionPlanningSnafu)?);
        }
        self.ctx.field_columns = new_field_columns;

        Ok(exprs)
    }

    /// Try to build a DataFusion Literal Expression from PromQL Expr, return
    /// `None` if the input is not a literal expression.
    fn try_build_literal_expr(expr: &PromExpr) -> Option<DfExpr> {
        match expr {
            PromExpr::NumberLiteral(NumberLiteral { val }) => {
                let scalar_value = ScalarValue::Float64(Some(*val));
                Some(DfExpr::Literal(scalar_value))
            }
            PromExpr::StringLiteral(StringLiteral { val }) => {
                let scalar_value = ScalarValue::Utf8(Some(val.to_string()));
                Some(DfExpr::Literal(scalar_value))
            }
            PromExpr::VectorSelector(_)
            | PromExpr::MatrixSelector(_)
            | PromExpr::Call(_)
            | PromExpr::Extension(_)
            | PromExpr::Aggregate(_)
            | PromExpr::Subquery(_) => None,
            PromExpr::Paren(ParenExpr { expr }) => Self::try_build_literal_expr(expr),
            // TODO(ruihang): support Unary operator
            PromExpr::Unary(UnaryExpr { expr, .. }) => Self::try_build_literal_expr(expr),
            PromExpr::Binary(PromBinaryExpr { lhs, rhs, op, .. }) => {
                let lhs = Self::try_build_literal_expr(lhs)?;
                let rhs = Self::try_build_literal_expr(rhs)?;
                let op = Self::prom_token_to_binary_op(*op).ok()?;
                Some(DfExpr::BinaryExpr(BinaryExpr {
                    left: Box::new(lhs),
                    op,
                    right: Box::new(rhs),
                }))
            }
        }
    }

    fn prom_token_to_binary_op(token: TokenType) -> Result<Operator> {
        match token.id() {
            token::T_ADD => Ok(Operator::Plus),
            token::T_SUB => Ok(Operator::Minus),
            token::T_MUL => Ok(Operator::Multiply),
            token::T_DIV => Ok(Operator::Divide),
            token::T_MOD => Ok(Operator::Modulo),
            token::T_EQLC => Ok(Operator::Eq),
            token::T_NEQ => Ok(Operator::NotEq),
            token::T_GTR => Ok(Operator::Gt),
            token::T_LSS => Ok(Operator::Lt),
            token::T_GTE => Ok(Operator::GtEq),
            token::T_LTE => Ok(Operator::LtEq),
            // TODO(ruihang): support these two operators
            // token::T_POW => Ok(Operator::Power),
            // token::T_ATAN2 => Ok(Operator::Atan2),
            _ => UnexpectedTokenSnafu { token }.fail(),
        }
    }

    /// Check if the given op is a [comparison operator](https://prometheus.io/docs/prometheus/latest/querying/operators/#comparison-binary-operators).
    fn is_token_a_comparison_op(token: TokenType) -> bool {
        matches!(
            token.id(),
            token::T_EQLC
                | token::T_NEQ
                | token::T_GTR
                | token::T_LSS
                | token::T_GTE
                | token::T_LTE
        )
    }

    /// Build a inner join on time index column and tag columns to concat two logical plans.
    /// The left plan will be alised as [`LEFT_PLAN_JOIN_ALIAS`].
    fn join_on_non_field_columns(
        &self,
        left: LogicalPlan,
        right: LogicalPlan,
    ) -> Result<LogicalPlan> {
        let mut tag_columns = self
            .ctx
            .tag_columns
            .iter()
            .map(Column::from_name)
            .collect::<Vec<_>>();

        // push time index column if it exist
        if let Some(time_index_column) = &self.ctx.time_index_column {
            tag_columns.push(Column::from_name(time_index_column));
        }

        // Inner Join on time index column to concat two operator
        LogicalPlanBuilder::from(left)
            .alias(LEFT_PLAN_JOIN_ALIAS)
            .context(DataFusionPlanningSnafu)?
            .join(
                right,
                JoinType::Inner,
                // (vec![time_index_column.clone()], vec![time_index_column]),
                (tag_columns.clone(), tag_columns),
                None,
            )
            .context(DataFusionPlanningSnafu)?
            .build()
            .context(DataFusionPlanningSnafu)
    }

    /// Build a projection that project and perform operation expr for every value columns.
    /// Non-value columns (tag and timestamp) will be preserved in the projection.
    ///
    /// # Side effect
    ///
    /// This function will update the value columns in the context. Those new column names
    /// don't contains qualifier.
    fn projection_for_each_field_column<F>(
        &mut self,
        input: LogicalPlan,
        name_to_expr: F,
    ) -> Result<LogicalPlan>
    where
        F: FnMut(&String) -> Result<DfExpr>,
    {
        let non_field_columns_iter = self
            .ctx
            .tag_columns
            .iter()
            .chain(self.ctx.time_index_column.iter())
            .map(|col| {
                Ok(DfExpr::Column(Column::new(
                    self.ctx.table_name.clone(),
                    col,
                )))
            });

        // build computation exprs
        let result_field_columns = self
            .ctx
            .field_columns
            .iter()
            .map(name_to_expr)
            .collect::<Result<Vec<_>>>()?;

        // alias the computation exprs to remove qualifier
        self.ctx.field_columns = result_field_columns
            .iter()
            .map(|expr| expr.display_name())
            .collect::<DfResult<Vec<_>>>()
            .context(DataFusionPlanningSnafu)?;
        let field_columns_iter = result_field_columns
            .into_iter()
            .zip(self.ctx.field_columns.iter())
            .map(|(expr, name)| Ok(DfExpr::Alias(Box::new(expr), name.to_string())));

        // chain non-value columns (unchanged) and value columns (applied computation then alias)
        let project_fields = non_field_columns_iter
            .chain(field_columns_iter)
            .collect::<Result<Vec<_>>>()?;

        LogicalPlanBuilder::from(input)
            .project(project_fields)
            .context(DataFusionPlanningSnafu)?
            .build()
            .context(DataFusionPlanningSnafu)
    }

    /// Build a filter plan that filter on value column. Notice that only one value column
    /// is expected.
    fn filter_on_field_column<F>(
        &self,
        input: LogicalPlan,
        mut name_to_expr: F,
    ) -> Result<LogicalPlan>
    where
        F: FnMut(&String) -> Result<DfExpr>,
    {
        ensure!(
            self.ctx.field_columns.len() == 1,
            UnsupportedExprSnafu {
                name: "filter on multi-value input"
            }
        );

        let field_column_filter = name_to_expr(&self.ctx.field_columns[0])?;

        LogicalPlanBuilder::from(input)
            .filter(field_column_filter)
            .context(DataFusionPlanningSnafu)?
            .build()
            .context(DataFusionPlanningSnafu)
    }
}

#[derive(Default, Debug)]
struct FunctionArgs {
    input: Option<PromExpr>,
    literals: Vec<DfExpr>,
}

#[derive(Debug, Clone)]
enum ScalarFunc {
    DataFusionBuiltin(BuiltinScalarFunction),
    Udf(ScalarUDF),
    // todo(ruihang): maybe merge with Udf later
    /// UDF that require extra information like range length to be evaluated.
    ExtrapolateUdf(ScalarUDF),
}

#[cfg(test)]
mod test {
    use std::time::{Duration, UNIX_EPOCH};

    use catalog::local::MemoryCatalogManager;
    use catalog::{CatalogManager, RegisterTableRequest};
    use common_catalog::consts::{DEFAULT_CATALOG_NAME, DEFAULT_SCHEMA_NAME};
    use datatypes::prelude::ConcreteDataType;
    use datatypes::schema::{ColumnSchema, Schema};
    use promql_parser::parser;
    use session::context::QueryContext;
    use table::metadata::{TableInfoBuilder, TableMetaBuilder};
    use table::test_util::EmptyTable;

    use super::*;

    async fn build_test_table_provider(
        table_name: String,
        num_tag: usize,
        num_field: usize,
    ) -> DfTableSourceProvider {
        let mut columns = vec![];
        for i in 0..num_tag {
            columns.push(ColumnSchema::new(
                format!("tag_{i}"),
                ConcreteDataType::string_datatype(),
                false,
            ));
        }
        columns.push(
            ColumnSchema::new(
                "timestamp".to_string(),
                ConcreteDataType::timestamp_millisecond_datatype(),
                false,
            )
            .with_time_index(true),
        );
        for i in 0..num_field {
            columns.push(ColumnSchema::new(
                format!("field_{i}"),
                ConcreteDataType::float64_datatype(),
                true,
            ));
        }
        let schema = Arc::new(Schema::new(columns));
        let table_meta = TableMetaBuilder::default()
            .schema(schema)
            .primary_key_indices((0..num_tag).collect())
            .value_indices((num_tag + 1..num_tag + 1 + num_field).collect())
            .next_column_id(1024)
            .build()
            .unwrap();
        let table_info = TableInfoBuilder::default()
            .name(&table_name)
            .meta(table_meta)
            .build()
            .unwrap();
        let table = Arc::new(EmptyTable::from_table_info(&table_info));
        let catalog_list = Arc::new(MemoryCatalogManager::default());
        catalog_list
            .register_table(RegisterTableRequest {
                catalog: DEFAULT_CATALOG_NAME.to_string(),
                schema: DEFAULT_SCHEMA_NAME.to_string(),
                table_name,
                table_id: 1024,
                table,
            })
            .await
            .unwrap();
        DfTableSourceProvider::new(catalog_list, false, &QueryContext::new())
    }

    // {
    //     input: `abs(some_metric{foo!="bar"})`,
    //     expected: &Call{
    //         Func: MustGetFunction("abs"),
    //         Args: Expressions{
    //             &VectorSelector{
    //                 Name: "some_metric",
    //                 LabelMatchers: []*labels.Matcher{
    //                     MustLabelMatcher(labels.MatchNotEqual, "foo", "bar"),
    //                     MustLabelMatcher(labels.MatchEqual, model.MetricNameLabel, "some_metric"),
    //                 },
    //             },
    //         },
    //     },
    // },
    async fn do_single_instant_function_call(fn_name: &'static str, plan_name: &str) {
        let prom_expr =
            parser::parse(&format!("{fn_name}(some_metric{{tag_0!=\"bar\"}})")).unwrap();
        let eval_stmt = EvalStmt {
            expr: prom_expr,
            start: UNIX_EPOCH,
            end: UNIX_EPOCH
                .checked_add(Duration::from_secs(100_000))
                .unwrap(),
            interval: Duration::from_secs(5),
            lookback_delta: Duration::from_secs(1),
        };

        let table_provider = build_test_table_provider("some_metric".to_string(), 1, 1).await;
        let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt)
            .await
            .unwrap();

        let expected = String::from(
            "Filter: TEMPLATE(field_0) IS NOT NULL [timestamp:Timestamp(Millisecond, None), TEMPLATE(field_0):Float64;N, tag_0:Utf8]\
            \n  Projection: some_metric.timestamp, TEMPLATE(some_metric.field_0) AS TEMPLATE(field_0), some_metric.tag_0 [timestamp:Timestamp(Millisecond, None), TEMPLATE(field_0):Float64;N, tag_0:Utf8]\
            \n    PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            Filter: some_metric.tag_0 != Utf8(\"bar\") [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n              TableScan: some_metric, unsupported_filters=[tag_0 != Utf8(\"bar\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        ).replace("TEMPLATE", plan_name);

        assert_eq!(plan.display_indent_schema().to_string(), expected);
    }

    #[tokio::test]
    async fn single_abs() {
        do_single_instant_function_call("abs", "abs").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_absent() {
        do_single_instant_function_call("absent", "").await;
    }

    #[tokio::test]
    async fn single_ceil() {
        do_single_instant_function_call("ceil", "ceil").await;
    }

    #[tokio::test]
    async fn single_exp() {
        do_single_instant_function_call("exp", "exp").await;
    }

    #[tokio::test]
    async fn single_ln() {
        do_single_instant_function_call("ln", "ln").await;
    }

    #[tokio::test]
    async fn single_log2() {
        do_single_instant_function_call("log2", "log2").await;
    }

    #[tokio::test]
    async fn single_log10() {
        do_single_instant_function_call("log10", "log10").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_scalar() {
        do_single_instant_function_call("scalar", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_sgn() {
        do_single_instant_function_call("sgn", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_sort() {
        do_single_instant_function_call("sort", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_sort_desc() {
        do_single_instant_function_call("sort_desc", "").await;
    }

    #[tokio::test]
    async fn single_sqrt() {
        do_single_instant_function_call("sqrt", "sqrt").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_timestamp() {
        do_single_instant_function_call("timestamp", "").await;
    }

    #[tokio::test]
    async fn single_acos() {
        do_single_instant_function_call("acos", "acos").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_acosh() {
        do_single_instant_function_call("acosh", "").await;
    }

    #[tokio::test]
    async fn single_asin() {
        do_single_instant_function_call("asin", "asin").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_asinh() {
        do_single_instant_function_call("asinh", "").await;
    }

    #[tokio::test]
    async fn single_atan() {
        do_single_instant_function_call("atan", "atan").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_atanh() {
        do_single_instant_function_call("atanh", "").await;
    }

    #[tokio::test]
    async fn single_cos() {
        do_single_instant_function_call("cos", "cos").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_cosh() {
        do_single_instant_function_call("cosh", "").await;
    }

    #[tokio::test]
    async fn single_sin() {
        do_single_instant_function_call("sin", "sin").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_sinh() {
        do_single_instant_function_call("sinh", "").await;
    }

    #[tokio::test]
    async fn single_tan() {
        do_single_instant_function_call("tan", "tan").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_tanh() {
        do_single_instant_function_call("tanh", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_deg() {
        do_single_instant_function_call("deg", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn single_rad() {
        do_single_instant_function_call("rad", "").await;
    }

    // {
    //     input: "avg by (foo)(some_metric)",
    //     expected: &AggregateExpr{
    //         Op: AVG,
    //         Expr: &VectorSelector{
    //             Name: "some_metric",
    //             LabelMatchers: []*labels.Matcher{
    //                 MustLabelMatcher(labels.MatchEqual, model.MetricNameLabel, "some_metric"),
    //             },
    //             PosRange: PositionRange{
    //                 Start: 13,
    //                 End:   24,
    //             },
    //         },
    //         Grouping: []string{"foo"},
    //         PosRange: PositionRange{
    //             Start: 0,
    //             End:   25,
    //         },
    //     },
    // },
    async fn do_aggregate_expr_plan(fn_name: &str, plan_name: &str) {
        let prom_expr = parser::parse(&format!(
            "{fn_name} by (tag_1)(some_metric{{tag_0!=\"bar\"}})",
        ))
        .unwrap();
        let mut eval_stmt = EvalStmt {
            expr: prom_expr,
            start: UNIX_EPOCH,
            end: UNIX_EPOCH
                .checked_add(Duration::from_secs(100_000))
                .unwrap(),
            interval: Duration::from_secs(5),
            lookback_delta: Duration::from_secs(1),
        };

        // test group by
        let table_provider = build_test_table_provider("some_metric".to_string(), 2, 2).await;
        let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt.clone())
            .await
            .unwrap();
        let  expected_no_without = String::from(
            "Sort: some_metric.tag_1 ASC NULLS LAST, some_metric.timestamp ASC NULLS LAST [tag_1:Utf8, timestamp:Timestamp(Millisecond, None), TEMPLATE(some_metric.field_0):Float64;N, TEMPLATE(some_metric.field_1):Float64;N]\
            \n  Aggregate: groupBy=[[some_metric.tag_1, some_metric.timestamp]], aggr=[[TEMPLATE(some_metric.field_0), TEMPLATE(some_metric.field_1)]] [tag_1:Utf8, timestamp:Timestamp(Millisecond, None), TEMPLATE(some_metric.field_0):Float64;N, TEMPLATE(some_metric.field_1):Float64;N]\
            \n    PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\", \"tag_1\"] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.tag_1 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n            Filter: some_metric.tag_0 != Utf8(\"bar\") [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n              TableScan: some_metric, unsupported_filters=[tag_0 != Utf8(\"bar\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]"
        ).replace("TEMPLATE", plan_name);
        assert_eq!(
            plan.display_indent_schema().to_string(),
            expected_no_without
        );

        // test group without
        if let PromExpr::Aggregate(AggregateExpr { modifier, .. }) = &mut eval_stmt.expr {
            *modifier = Some(LabelModifier::Exclude(
                vec![String::from("tag_1")].into_iter().collect(),
            ));
        }
        let table_provider = build_test_table_provider("some_metric".to_string(), 2, 2).await;
        let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt)
            .await
            .unwrap();
        let  expected_without = String::from(
            "Sort: some_metric.tag_0 ASC NULLS LAST, some_metric.timestamp ASC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), TEMPLATE(some_metric.field_0):Float64;N, TEMPLATE(some_metric.field_1):Float64;N]\
            \n  Aggregate: groupBy=[[some_metric.tag_0, some_metric.timestamp]], aggr=[[TEMPLATE(some_metric.field_0), TEMPLATE(some_metric.field_1)]] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), TEMPLATE(some_metric.field_0):Float64;N, TEMPLATE(some_metric.field_1):Float64;N]\
            \n    PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\", \"tag_1\"] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.tag_1 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n            Filter: some_metric.tag_0 != Utf8(\"bar\") [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]\
            \n              TableScan: some_metric, unsupported_filters=[tag_0 != Utf8(\"bar\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, tag_1:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, field_1:Float64;N]"
        ).replace("TEMPLATE", plan_name);
        assert_eq!(plan.display_indent_schema().to_string(), expected_without);
    }

    #[tokio::test]
    async fn aggregate_sum() {
        do_aggregate_expr_plan("sum", "SUM").await;
    }

    #[tokio::test]
    async fn aggregate_avg() {
        do_aggregate_expr_plan("avg", "AVG").await;
    }

    #[tokio::test]
    #[should_panic] // output type doesn't match
    async fn aggregate_count() {
        do_aggregate_expr_plan("count", "COUNT").await;
    }

    #[tokio::test]
    async fn aggregate_min() {
        do_aggregate_expr_plan("min", "MIN").await;
    }

    #[tokio::test]
    async fn aggregate_max() {
        do_aggregate_expr_plan("max", "MAX").await;
    }

    #[tokio::test]
    #[should_panic] // output type doesn't match
    async fn aggregate_group() {
        do_aggregate_expr_plan("grouping", "GROUPING").await;
    }

    #[tokio::test]
    async fn aggregate_stddev() {
        do_aggregate_expr_plan("stddev", "STDDEVPOP").await;
    }

    #[tokio::test]
    async fn aggregate_stdvar() {
        do_aggregate_expr_plan("stdvar", "VARIANCEPOP").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn aggregate_top_k() {
        do_aggregate_expr_plan("topk", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn aggregate_bottom_k() {
        do_aggregate_expr_plan("bottomk", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn aggregate_count_values() {
        do_aggregate_expr_plan("count_values", "").await;
    }

    #[tokio::test]
    #[should_panic]
    async fn aggregate_quantile() {
        do_aggregate_expr_plan("quantile", "").await;
    }

    // TODO(ruihang): add range fn tests once exprs are ready.

    // {
    //     input: "some_metric{tag_0="foo"} + some_metric{tag_0="bar"}",
    //     expected: &BinaryExpr{
    //         Op: ADD,
    //         LHS: &VectorSelector{
    //             Name: "a",
    //             LabelMatchers: []*labels.Matcher{
    //                     MustLabelMatcher(labels.MatchEqual, "tag_0", "foo"),
    //                     MustLabelMatcher(labels.MatchEqual, model.MetricNameLabel, "some_metric"),
    //             },
    //         },
    //         RHS: &VectorSelector{
    //             Name: "sum",
    //             LabelMatchers: []*labels.Matcher{
    //                     MustLabelMatcher(labels.MatchxEqual, "tag_0", "bar"),
    //                     MustLabelMatcher(labels.MatchEqual, model.MetricNameLabel, "some_metric"),
    //             },
    //         },
    //         VectorMatching: &VectorMatching{},
    //     },
    // },
    #[tokio::test]
    async fn binary_op_column_column() {
        let prom_expr =
            parser::parse(r#"some_metric{tag_0="foo"} + some_metric{tag_0="bar"}"#).unwrap();
        let eval_stmt = EvalStmt {
            expr: prom_expr,
            start: UNIX_EPOCH,
            end: UNIX_EPOCH
                .checked_add(Duration::from_secs(100_000))
                .unwrap(),
            interval: Duration::from_secs(5),
            lookback_delta: Duration::from_secs(1),
        };

        let table_provider = build_test_table_provider("some_metric".to_string(), 1, 1).await;
        let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt)
            .await
            .unwrap();

        let  expected = String::from(
            "Projection: some_metric.tag_0, some_metric.timestamp, some_metric.field_0 + some_metric.field_0 AS some_metric.field_0 + some_metric.field_0 [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), some_metric.field_0 + some_metric.field_0:Float64;N]\
            \n  Inner Join: lhs.tag_0 = some_metric.tag_0, lhs.timestamp = some_metric.timestamp [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N, tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    SubqueryAlias: lhs [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n              Filter: some_metric.tag_0 = Utf8(\"foo\") [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n                TableScan: some_metric, unsupported_filters=[tag_0 = Utf8(\"foo\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            Filter: some_metric.tag_0 = Utf8(\"bar\") [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n              TableScan: some_metric, unsupported_filters=[tag_0 = Utf8(\"bar\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        assert_eq!(plan.display_indent_schema().to_string(), expected);
    }

    async fn indie_query_plan_compare(query: &str, expected: String) {
        let prom_expr = parser::parse(query).unwrap();
        let eval_stmt = EvalStmt {
            expr: prom_expr,
            start: UNIX_EPOCH,
            end: UNIX_EPOCH
                .checked_add(Duration::from_secs(100_000))
                .unwrap(),
            interval: Duration::from_secs(5),
            lookback_delta: Duration::from_secs(1),
        };

        let table_provider = build_test_table_provider("some_metric".to_string(), 1, 1).await;
        let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt)
            .await
            .unwrap();

        assert_eq!(plan.display_indent_schema().to_string(), expected);
    }

    #[tokio::test]
    async fn binary_op_literal_column() {
        let query = r#"1 + some_metric{tag_0="bar"}"#;
        let expected = String::from(
            "Projection: some_metric.tag_0, some_metric.timestamp, Float64(1) + some_metric.field_0 AS Float64(1) + field_0 [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), Float64(1) + field_0:Float64;N]\
            \n  PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          Filter: some_metric.tag_0 = Utf8(\"bar\") [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            TableScan: some_metric, unsupported_filters=[tag_0 = Utf8(\"bar\"), timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    #[ignore = "pure literal arithmetic is not supported yet"]
    async fn binary_op_literal_literal() {
        let query = r#"1 + 1"#;
        let expected = String::from("");

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn simple_bool_grammar() {
        let query = "some_metric != bool 1.2345";
        let expected = String::from(
            "Projection: some_metric.tag_0, some_metric.timestamp, CAST(some_metric.field_0 != Float64(1.2345) AS Float64) AS field_0 != Float64(1.2345) [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0 != Float64(1.2345):Float64;N]\
            \n  PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          TableScan: some_metric, unsupported_filters=[timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    #[ignore = "pure literal arithmetic is not supported yet"]
    async fn bool_with_additional_arithmetic() {
        let query = "some_metric + (1 == bool 2)";
        let expected = String::from("");

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn simple_unary() {
        let query = "-some_metric";
        let expected = String::from(
            "Projection: some_metric.tag_0, some_metric.timestamp, (- some_metric.field_0) AS (- field_0) [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), (- field_0):Float64;N]\
            \n  PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          TableScan: some_metric, unsupported_filters=[timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn increase_aggr() {
        let query = "increase(some_metric[5m])";
        let expected = String::from(
            "Filter: prom_increase(timestamp_range,field_0,timestamp) IS NOT NULL [timestamp:Timestamp(Millisecond, None), prom_increase(timestamp_range,field_0,timestamp):Float64;N, tag_0:Utf8]\
            \n  Projection: some_metric.timestamp, prom_increase(timestamp_range, field_0, some_metric.timestamp) AS prom_increase(timestamp_range,field_0,timestamp), some_metric.tag_0 [timestamp:Timestamp(Millisecond, None), prom_increase(timestamp_range,field_0,timestamp):Float64;N, tag_0:Utf8]\
            \n    PromRangeManipulate: req range=[0..100000000], interval=[5000], eval range=[300000], time index=[timestamp], values=[\"field_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Dictionary(Int64, Float64);N, timestamp_range:Dictionary(Int64, Timestamp(Millisecond, None))]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [true] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            TableScan: some_metric, unsupported_filters=[timestamp >= TimestampMillisecond(-301000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn less_filter_on_value() {
        let query = "some_metric < 1.2345";
        let expected = String::from(
            "Filter: some_metric.field_0 < Float64(1.2345) [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n  PromInstantManipulate: range=[0..100000000], lookback=[1000], interval=[5000], time index=[timestamp] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n    PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [false] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n      PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          TableScan: some_metric, unsupported_filters=[timestamp >= TimestampMillisecond(-1000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn count_over_time() {
        let query = "count_over_time(some_metric[5m])";
        let expected = String::from(
            "Filter: prom_count_over_time(timestamp_range,field_0) IS NOT NULL [timestamp:Timestamp(Millisecond, None), prom_count_over_time(timestamp_range,field_0):Float64;N, tag_0:Utf8]\
            \n  Projection: some_metric.timestamp, prom_count_over_time(timestamp_range, field_0) AS prom_count_over_time(timestamp_range,field_0), some_metric.tag_0 [timestamp:Timestamp(Millisecond, None), prom_count_over_time(timestamp_range,field_0):Float64;N, tag_0:Utf8]\
            \n    PromRangeManipulate: req range=[0..100000000], interval=[5000], eval range=[300000], time index=[timestamp], values=[\"field_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Dictionary(Int64, Float64);N, timestamp_range:Dictionary(Int64, Timestamp(Millisecond, None))]\
            \n      PromSeriesNormalize: offset=[0], time index=[timestamp], filter NaN: [true] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n        PromSeriesDivide: tags=[\"tag_0\"] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n          Sort: some_metric.tag_0 DESC NULLS LAST, some_metric.timestamp DESC NULLS LAST [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]\
            \n            TableScan: some_metric, unsupported_filters=[timestamp >= TimestampMillisecond(-301000, None), timestamp <= TimestampMillisecond(100001000, None)] [tag_0:Utf8, timestamp:Timestamp(Millisecond, None), field_0:Float64;N]"
        );

        indie_query_plan_compare(query, expected).await;
    }

    #[tokio::test]
    async fn value_matcher() {
        // template
        let mut eval_stmt = EvalStmt {
            expr: PromExpr::NumberLiteral(NumberLiteral { val: 1.0 }),
            start: UNIX_EPOCH,
            end: UNIX_EPOCH
                .checked_add(Duration::from_secs(100_000))
                .unwrap(),
            interval: Duration::from_secs(5),
            lookback_delta: Duration::from_secs(1),
        };

        let cases = [
            // single equal matcher
            (
                r#"some_metric{__field__="field_1"}"#,
                vec![
                    "some_metric.field_1",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // two equal matchers
            (
                r#"some_metric{__field__="field_1", __field__="field_0"}"#,
                vec![
                    "some_metric.field_0",
                    "some_metric.field_1",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // single not_eq mathcer
            (
                r#"some_metric{__field__!="field_1"}"#,
                vec![
                    "some_metric.field_0",
                    "some_metric.field_2",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // two not_eq mathcers
            (
                r#"some_metric{__field__!="field_1", __field__!="field_2"}"#,
                vec![
                    "some_metric.field_0",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // equal and not_eq matchers (no conflict)
            (
                r#"some_metric{__field__="field_1", __field__!="field_0"}"#,
                vec![
                    "some_metric.field_1",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // equal and not_eq matchers (conflict)
            (
                r#"some_metric{__field__="field_2", __field__!="field_2"}"#,
                vec![
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // single regex eq matcher
            (
                r#"some_metric{__field__=~"field_1|field_2"}"#,
                vec![
                    "some_metric.field_1",
                    "some_metric.field_2",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
            // single regex not_eq matcher
            (
                r#"some_metric{__field__!~"field_1|field_2"}"#,
                vec![
                    "some_metric.field_0",
                    "some_metric.tag_0",
                    "some_metric.tag_1",
                    "some_metric.tag_2",
                    "some_metric.timestamp",
                ],
            ),
        ];

        for case in cases {
            let prom_expr = parser::parse(case.0).unwrap();
            eval_stmt.expr = prom_expr;
            let table_provider = build_test_table_provider("some_metric".to_string(), 3, 3).await;
            let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt.clone())
                .await
                .unwrap();
            let mut fields = plan.schema().field_names();
            let mut expected = case.1.into_iter().map(String::from).collect::<Vec<_>>();
            fields.sort();
            expected.sort();
            assert_eq!(fields, expected, "case: {:?}", case.0);
        }

        let bad_cases = [
            r#"some_metric{__field__="nonexistent"}"#,
            r#"some_metric{__field__!="nonexistent"}"#,
        ];

        for case in bad_cases {
            let prom_expr = parser::parse(case).unwrap();
            eval_stmt.expr = prom_expr;
            let table_provider = build_test_table_provider("some_metric".to_string(), 3, 3).await;
            let plan = PromPlanner::stmt_to_plan(table_provider, eval_stmt.clone()).await;
            assert!(plan.is_err(), "case: {:?}", case);
        }
    }
}
