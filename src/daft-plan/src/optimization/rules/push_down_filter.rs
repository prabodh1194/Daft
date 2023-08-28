use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use common_error::DaftResult;
use daft_dsl::{
    col,
    optimization::{get_required_columns, replace_columns_with_expressions},
    Expr,
};

use crate::{
    ops::{Concat, Filter, Project},
    LogicalPlan,
};

use super::{
    utils::{conjuct, split_conjuction},
    ApplyOrder, OptimizerRule, Transformed,
};

/// Optimization rules for pushing Filters further into the logical plan.
#[derive(Default)]
pub struct PushDownFilter {}

impl PushDownFilter {
    pub fn new() -> Self {
        Self {}
    }
}

impl OptimizerRule for PushDownFilter {
    fn apply_order(&self) -> ApplyOrder {
        ApplyOrder::TopDown
    }

    fn try_optimize(&self, plan: Arc<LogicalPlan>) -> DaftResult<Transformed<Arc<LogicalPlan>>> {
        let filter = match plan.as_ref() {
            LogicalPlan::Filter(filter) => filter,
            _ => return Ok(Transformed::No(plan)),
        };
        let child_plan = filter.input.as_ref();
        let new_plan = match child_plan {
            LogicalPlan::Filter(child_filter) => {
                // Combine filters.
                //
                // Filter-Filter --> Filter

                // Split predicate expression on conjunctions (ANDs).
                let parent_predicates = split_conjuction(&filter.predicate);
                let predicate_set: HashSet<&&Expr> = parent_predicates.iter().collect();
                // Add child predicate expressions to parent predicate expressions, eliminating duplicates.
                let new_predicates = parent_predicates
                    .iter()
                    .chain(
                        split_conjuction(&child_filter.predicate)
                            .iter()
                            .filter(|e| !predicate_set.contains(e)),
                    )
                    .map(|e| (*e).clone())
                    .collect::<Vec<_>>();
                // Reconjunct predicate expressions.
                let new_predicate = conjuct(new_predicates).unwrap();
                let new_filter: Arc<LogicalPlan> =
                    LogicalPlan::from(Filter::new(new_predicate, child_filter.input.clone()))
                        .into();
                self.try_optimize(new_filter.clone())?
                    .or(Transformed::Yes(new_filter))
                    .unwrap()
                    .clone()
            }
            LogicalPlan::Project(child_project) => {
                // Commute filter with projection if predicate only depends on projection columns that
                // don't involve compute.
                //
                // Filter-Projection --> {Filter-}Projection-Filter
                let predicates = split_conjuction(&filter.predicate);
                let projection_input_mapping = child_project
                    .projection
                    .iter()
                    .filter_map(|e| {
                        e.input_mapping()
                            .map(|s| (e.name().unwrap().to_string(), col(s)))
                    })
                    .collect::<HashMap<String, Expr>>();
                // Split predicate expressions into those that don't depend on projection compute (can_push) and those
                // that do (can_not_push).
                // TODO(Clark): Push Filters depending on Projection columns involving compute if those expressions are
                // (1) determinstic && (pure || idempotent),
                // (2) inexpensive to recompute.
                // This can be done by rewriting the Filter predicate expression to contain the relevant Projection expression.
                let mut can_push = vec![];
                let mut can_not_push = vec![];
                for predicate in predicates {
                    let predicate_cols = get_required_columns(predicate);
                    if predicate_cols
                        .iter()
                        .all(|col| projection_input_mapping.contains_key(col))
                    {
                        // Can push predicate through expression.
                        let new_predicate =
                            replace_columns_with_expressions(predicate, &projection_input_mapping);
                        can_push.push(new_predicate);
                    } else {
                        // Can't push predicate expression through projection.
                        can_not_push.push(predicate.clone());
                    }
                }
                if can_push.is_empty() {
                    // No predicate expressions can be pushed through projection.
                    return Ok(Transformed::No(plan));
                }
                // Create new Filter with predicates that can be pushed past Projection.
                let predicates_to_push = conjuct(can_push).unwrap();
                let push_down_filter: LogicalPlan =
                    Filter::new(predicates_to_push, child_project.input.clone()).into();
                // Create new Projection.
                let new_projection: LogicalPlan = Project::try_new(
                    push_down_filter.into(),
                    child_project.projection.clone(),
                    child_project.resource_request.clone(),
                )?
                .into();
                if can_not_push.is_empty() {
                    // If all Filter predicate expressions were pushable past Projection, return new
                    // Projection-Filter subplan.
                    new_projection.into()
                } else {
                    // Otherwise, add a Filter after Projection that filters with predicate expressions
                    // that couldn't be pushed past the Projection, returning a Filter-Projection-Filter subplan.
                    let post_projection_predicate = conjuct(can_not_push).unwrap();
                    let post_projection_filter: LogicalPlan =
                        Filter::new(post_projection_predicate, new_projection.into()).into();
                    post_projection_filter.into()
                }
            }
            LogicalPlan::Sort(_) | LogicalPlan::Repartition(_) | LogicalPlan::Coalesce(_) => {
                // Naive commuting with unary ops.
                let new_filter = plan.with_new_children(&[child_plan.children()[0].clone()]);
                child_plan.with_new_children(&[new_filter])
            }
            LogicalPlan::Concat(Concat { input, other }) => {
                // Push filter into each side of the concat.
                let new_input: LogicalPlan =
                    Filter::new(filter.predicate.clone(), input.clone()).into();
                let new_other: LogicalPlan =
                    Filter::new(filter.predicate.clone(), other.clone()).into();
                let new_concat: LogicalPlan =
                    Concat::new(new_other.into(), new_input.into()).into();
                new_concat.into()
            }
            LogicalPlan::Join(child_join) => {
                // Push filter into each side of the join.
                // TODO(Clark): Merge filter predicate with on predicate, if present.
                // TODO(Clark): Duplicate filters for joined columns so filters can be pushed down to both sides.

                // Get all input columns for predicate.
                let predicate_cols: HashSet<_> = get_required_columns(&filter.predicate)
                    .iter()
                    .cloned()
                    .collect();
                // Only push the filter into the left side of the join if the left side of the join has all columns
                // required by the predicate.
                let left_cols: HashSet<_> =
                    child_join.input.schema().names().iter().cloned().collect();
                let can_push_left = left_cols
                    .intersection(&predicate_cols)
                    .collect::<HashSet<_>>()
                    .len()
                    == predicate_cols.len();
                // Only push the filter into the right side of the join if the right side of the join has all columns
                // required by the predicate.
                let right_cols: HashSet<_> =
                    child_join.right.schema().names().iter().cloned().collect();
                let can_push_right = right_cols
                    .intersection(&predicate_cols)
                    .collect::<HashSet<_>>()
                    .len()
                    == predicate_cols.len();
                if !can_push_left && !can_push_right {
                    return Ok(Transformed::No(plan));
                }
                let new_left: Arc<LogicalPlan> = if can_push_left {
                    LogicalPlan::from(Filter::new(
                        filter.predicate.clone(),
                        child_join.input.clone(),
                    ))
                    .into()
                } else {
                    child_join.input.clone()
                };
                let new_right: Arc<LogicalPlan> = if can_push_right {
                    LogicalPlan::from(Filter::new(
                        filter.predicate.clone(),
                        child_join.right.clone(),
                    ))
                    .into()
                } else {
                    child_join.right.clone()
                };
                child_plan.with_new_children(&[new_left, new_right])
            }
            _ => return Ok(Transformed::No(plan)),
        };
        Ok(Transformed::Yes(new_plan))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common_error::DaftResult;
    use daft_core::{datatypes::Field, DataType};
    use daft_dsl::{col, lit};

    use crate::{
        ops::{Coalesce, Concat, Filter, Join, Project, Repartition, Sort},
        optimization::{
            optimizer::{RuleBatch, RuleExecutionStrategy},
            rules::PushDownFilter,
            Optimizer,
        },
        test::dummy_scan_node,
        JoinType, LogicalPlan, PartitionScheme,
    };

    /// Helper that creates an optimizer with the PushDownFilter rule registered, optimizes
    /// the provided plan with said optimizer, and compares the optimized plan's repr with
    /// the provided expected repr.
    fn assert_optimized_plan_eq(plan: Arc<LogicalPlan>, expected: &str) -> DaftResult<()> {
        let optimizer = Optimizer::with_rule_batches(
            vec![RuleBatch::new(
                vec![Box::new(PushDownFilter::new())],
                RuleExecutionStrategy::Once,
            )],
            Default::default(),
        );
        let optimized_plan = optimizer
            .optimize_with_rules(
                optimizer.rule_batches[0].rules.as_slice(),
                plan.clone(),
                &optimizer.rule_batches[0].order,
            )?
            .unwrap()
            .clone();
        assert_eq!(optimized_plan.repr_indent(), expected);

        Ok(())
    }

    /// Tests combining of two Filters by merging their predicates.
    #[test]
    fn filter_combine_with_filter() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let first_filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), source.into()).into();
        let second_filter: LogicalPlan =
            Filter::new(col("b").eq(&lit("foo")), first_filter.into()).into();
        let expected = "\
        Filter: [col(b) == lit(\"foo\")] & [col(a) < lit(2)]\
        \n  Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(second_filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Projections.
    #[test]
    fn filter_commutes_with_projection() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let projection: LogicalPlan =
            Project::try_new(source.into(), vec![col("a")], Default::default())?.into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), projection.into()).into();
        let expected = "\
        Project: col(a)\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that a Filter with multiple columns in its predicate commutes with a Projection on both of those columns.
    #[test]
    fn filter_commutes_with_projection_multi() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let projection: LogicalPlan =
            Project::try_new(source.into(), vec![col("a"), col("b")], Default::default())?.into();
        let filter: LogicalPlan = Filter::new(
            col("a").lt(&lit(2)).and(&col("b").eq(&lit("foo"))),
            projection.into(),
        )
        .into();
        let expected = "\
        Project: col(a), col(b)\
        \n  Filter: [col(a) < lit(2)] & [col(b) == lit(\"foo\")]\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter does not commute with a Projection if the projection expression involves compute.
    #[test]
    fn filter_does_not_commute_with_projection_if_compute() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        // Projection involves compute on filtered column "a".
        let projection: LogicalPlan =
            Project::try_new(source.into(), vec![col("a") + lit(1)], Default::default())?.into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), projection.into()).into();
        // Filter should NOT commute with Project, since this would involve redundant computation.
        let expected = "\
        Filter: col(a) < lit(2)\
        \n  Project: col(a) + lit(1)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Projection if projection expression involves deterministic compute.
    // REASON - No expression attribute indicating whether deterministic && (pure || idempotent).
    #[ignore]
    #[test]
    fn filter_commutes_with_projection_deterministic_compute() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let projection: LogicalPlan =
            Project::try_new(source.into(), vec![col("a") + lit(1)], Default::default())?.into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), projection.into()).into();
        let expected = "\
        Project: col(a) + lit(1)\
        \n  Filter: [col(a) + lit(1)] < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Sort.
    #[test]
    fn filter_commutes_with_sort() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let sort: LogicalPlan = Sort::new(vec![col("a")], vec![true], source.into()).into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), sort.into()).into();
        let expected = "\
        Sort: Sort by = (col(a), descending)\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        // TODO(Clark): For tests in which we only care about reordering of operators, maybe switch to a form that leverages the single-node display?
        // let expected = format!("{sort}\n  {filter}\n    {source}");
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Repartition.
    #[test]
    fn filter_commutes_with_repartition() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let repartition: LogicalPlan =
            Repartition::new(1, vec![col("a")], PartitionScheme::Hash, source.into()).into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), repartition.into()).into();
        let expected = "\
        Repartition: Scheme = Hash, Number of partitions = 1, Partition by = col(a)\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Coalesce.
    #[test]
    fn filter_commutes_with_coalesce() -> DaftResult<()> {
        let source: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let coalesce: LogicalPlan = Coalesce::new(1, source.into()).into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), coalesce.into()).into();
        let expected = "\
        Coalesce: To = 1\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter commutes with Concat.
    #[test]
    fn filter_commutes_with_concat() -> DaftResult<()> {
        let fields = vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ];
        let source1: LogicalPlan = dummy_scan_node(fields.clone()).into();
        let source2: LogicalPlan = dummy_scan_node(fields).into();
        let concat: LogicalPlan = Concat::new(source2.into(), source1.into()).into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), concat.into()).into();
        let expected = "\
        Concat\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter can be pushed into the left side of a Join.
    #[test]
    fn filter_commutes_with_join_left_side() -> DaftResult<()> {
        let source1: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let source2: LogicalPlan = dummy_scan_node(vec![
            Field::new("b", DataType::Utf8),
            Field::new("c", DataType::Float64),
        ])
        .into();
        let join: LogicalPlan = Join::try_new(
            source1.into(),
            source2.into(),
            vec![col("b")],
            vec![col("b")],
            JoinType::Inner,
        )?
        .into();
        let filter: LogicalPlan = Filter::new(col("a").lt(&lit(2)), join.into()).into();
        let expected = "\
        Join: Type = Inner, On = col(b), Output schema = a (Int64), b (Utf8), c (Float64)\
        \n  Filter: col(a) < lit(2)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)\
        \n  Source: \"Json\", File paths = /foo, File schema = b (Utf8), c (Float64), Format-specific config = Json(JsonSourceConfig), Output schema = b (Utf8), c (Float64)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter can be pushed into the right side of a Join.
    #[test]
    fn filter_commutes_with_join_right_side() -> DaftResult<()> {
        let source1: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Utf8),
        ])
        .into();
        let source2: LogicalPlan = dummy_scan_node(vec![
            Field::new("b", DataType::Utf8),
            Field::new("c", DataType::Float64),
        ])
        .into();
        let join: LogicalPlan = Join::try_new(
            source1.into(),
            source2.into(),
            vec![col("b")],
            vec![col("b")],
            JoinType::Inner,
        )?
        .into();
        let filter: LogicalPlan = Filter::new(col("c").lt(&lit(2.0)), join.into()).into();
        let expected = "\
        Join: Type = Inner, On = col(b), Output schema = a (Int64), b (Utf8), c (Float64)\
        \n  Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Utf8), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Utf8)\
        \n  Filter: col(c) < lit(2.0)\
        \n    Source: \"Json\", File paths = /foo, File schema = b (Utf8), c (Float64), Format-specific config = Json(JsonSourceConfig), Output schema = b (Utf8), c (Float64)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }

    /// Tests that Filter can be pushed into both sides of a Join.
    #[test]
    fn filter_commutes_with_join_both_sides() -> DaftResult<()> {
        let source1: LogicalPlan = dummy_scan_node(vec![
            Field::new("a", DataType::Int64),
            Field::new("b", DataType::Int64),
            Field::new("c", DataType::Float64),
        ])
        .into();
        let source2: LogicalPlan = dummy_scan_node(vec![Field::new("b", DataType::Int64)]).into();
        let join: LogicalPlan = Join::try_new(
            source1.into(),
            source2.into(),
            vec![col("b")],
            vec![col("b")],
            JoinType::Inner,
        )?
        .into();
        let filter: LogicalPlan = Filter::new(col("b").lt(&lit(2.0)), join.into()).into();
        let expected = "\
        Join: Type = Inner, On = col(b), Output schema = a (Int64), b (Int64), c (Float64)\
        \n  Filter: col(b) < lit(2.0)\
        \n    Source: \"Json\", File paths = /foo, File schema = a (Int64), b (Int64), c (Float64), Format-specific config = Json(JsonSourceConfig), Output schema = a (Int64), b (Int64), c (Float64)\
        \n  Filter: col(b) < lit(2.0)\
        \n    Source: \"Json\", File paths = /foo, File schema = b (Int64), Format-specific config = Json(JsonSourceConfig), Output schema = b (Int64)";
        assert_optimized_plan_eq(filter.into(), expected)?;
        Ok(())
    }
}