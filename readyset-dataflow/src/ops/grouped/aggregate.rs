use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

pub use nom_sql::{BinaryOperator, Literal, SqlType};
use readyset_data::{Collation, DfType};
use readyset_errors::{invariant, ReadySetResult};
use serde::{Deserialize, Serialize};

use crate::ops::grouped::{GroupedOperation, GroupedOperator};
use crate::prelude::*;

/// Supported aggregation operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Aggregation {
    /// Count the number of non-null values.
    Count,
    /// Sum the value of the `over` column for all records of each group.
    Sum,
    /// Average the value of the `over` column. Maintains count and sum in HashMap
    Avg,
    /// Concatenates using the given separator between values.
    GroupConcat { separator: String },
}

impl Aggregation {
    /// Construct a new `Aggregator` that performs this operation.
    ///
    /// The aggregation will aggregate the value in column number `over` from its inputs (i.e.,
    /// from the `src` node in the graph), and use the columns in the `group_by` array as a group
    /// identifier.
    pub fn over(
        self,
        src: NodeIndex,
        over: usize,
        group_by: &[usize],
        over_col_ty: &DfType,
    ) -> ReadySetResult<GroupedOperator<Aggregator>> {
        let out_ty = match &self {
            Aggregation::Count { .. } => DfType::BigInt,
            // The SUM() and AVG() functions return a DECIMAL value for exact-value arguments
            // (integer or DECIMAL), and a DOUBLE value for approximate-value arguments (FLOAT or
            // DOUBLE).
            Aggregation::Sum | Aggregation::Avg => {
                if over_col_ty.is_any_float() {
                    DfType::Double
                } else {
                    DfType::DEFAULT_NUMERIC
                }
            }
            Aggregation::GroupConcat { .. } => DfType::Text(/* TODO */ Collation::default()),
        };

        Ok(GroupedOperator::new(
            src,
            Aggregator {
                op: self,
                over,
                group: group_by.into(),
                count_sum_map: RefCell::new(Default::default()),
                over_else: None,
                out_ty,
            },
        ))
    }
}

/// Aggregator implements a Soup node that performs common aggregation operations such as counts
/// and sums
///
/// `Aggregator` nodes are constructed through `Aggregation` variants using `Aggregation::new`.
///
/// When a new record arrives, the aggregator will first query the currently aggregated value for
/// the new record's group by doing a query into its own output. The aggregated column
/// (`self.over`) of the incoming record is then added to the current aggregation value according
/// to the operator in use. The output record is constructed by concatenating the columns
/// identifying the group, and appending the aggregated value. For example, for a sum with
/// `self.over == 1`, a previous sum of `3`, and an incoming record with `[a, 1, x]`, the output
/// would be `[a, x, 4]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aggregator {
    op: Aggregation,
    over: usize,
    group: Vec<usize>,
    // only needed for AVG. Stores both sum and count to avoid rounding errors.
    // We skip serde since we don't want the state, just the configuration.
    #[serde(skip)]
    count_sum_map: RefCell<HashMap<GroupHash, AverageDataPair>>,
    over_else: Option<Literal>,
    // Output type of this column
    out_ty: DfType,
}

/// Diff type for numerical aggregations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumericalDiff {
    /// Numerical value of the diff of the `over` column
    value: DfValue,
    /// True if positive record, false if negative
    positive: bool,
    /// Hash of the values of the group by columns, needed for AVG
    group_hash: GroupHash,
}

type GroupHash = u64;

/// For storing (Count, Sum) in additional state for Average.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AverageDataPair {
    count: DfValue,
    sum: DfValue,
}

impl AverageDataPair {
    fn apply_diff(&mut self, d: NumericalDiff) -> ReadySetResult<DfValue> {
        if d.positive {
            self.sum = (&self.sum + &d.value)?;
            self.count = (&self.count + &DfValue::Int(1))?;
        } else {
            self.sum = (&self.sum - &d.value)?;
            self.count = (&self.count - &DfValue::Int(1))?;
        }

        if self.count > DfValue::Int(0) {
            &self.sum / &self.count
        } else {
            Ok(DfValue::Double(0.0))
        }
    }
}

impl Aggregator {
    fn group_hash(&self, rec: &[DfValue]) -> GroupHash {
        let mut hasher = DefaultHasher::new();
        for &col in self.group.iter() {
            #[allow(clippy::indexing_slicing)]
            // When the Aggregator is constructed, it is constructed with a group by that is
            // derived from existing columns. If we lack a column in the record, then something has
            // gone horrible wrong and we should panic.
            rec[col].hash(&mut hasher)
        }
        hasher.finish()
    }

    fn new_data(&self) -> ReadySetResult<DfValue> {
        match &self.out_ty {
            DfType::BigInt => Ok(DfValue::Int(Default::default())),
            DfType::Double => Ok(DfValue::Double(Default::default())),
            DfType::Numeric { .. } => Ok(DfValue::Numeric(Default::default())),
            DfType::Text { .. } => Ok(DfValue::from("" /* TODO(grfn): Use collation here */)),
            _ => internal!(),
        }
    }
}

impl GroupedOperation for Aggregator {
    type Diff = NumericalDiff;

    fn setup(&mut self, parent: &Node) -> ReadySetResult<()> {
        invariant!(
            self.over < parent.columns().len(),
            "cannot aggregate over non-existing column"
        );

        Ok(())
    }

    fn group_by(&self) -> &[usize] {
        &self.group[..]
    }

    fn to_diff(&self, r: &[DfValue], pos: bool) -> ReadySetResult<Self::Diff> {
        let group_hash = self.group_hash(r);
        Ok(NumericalDiff {
            #[allow(clippy::indexing_slicing)]
            // When the Aggregator is constructed, it is constructed with an over that is
            // derived from an existing column, or it is defaulted to 0. We should always have at
            // least one column in this node, so this index is safe.
            value: r[self.over].clone(),
            positive: pos,
            group_hash,
        })
    }

    fn apply(
        &self,
        current: Option<&DfValue>,
        diffs: &mut dyn Iterator<Item = Self::Diff>,
    ) -> ReadySetResult<Option<DfValue>> {
        let apply_count = |curr: DfValue, diff: Self::Diff| -> ReadySetResult<DfValue> {
            if diff.positive {
                &curr + &DfValue::Int(1)
            } else {
                &curr - &DfValue::Int(1)
            }
        };

        let apply_sum = |curr: DfValue, diff: Self::Diff| -> ReadySetResult<DfValue> {
            if diff.positive {
                &curr + &diff.value
            } else {
                &curr - &diff.value
            }
        };

        let apply_avg = |_curr, diff: Self::Diff| -> ReadySetResult<DfValue> {
            self.count_sum_map
                .borrow_mut()
                .entry(diff.group_hash)
                .or_insert(AverageDataPair {
                    sum: DfValue::Double(0.0),
                    count: DfValue::Int(0),
                })
                .apply_diff(diff)
        };

        let apply_diff =
            |curr: ReadySetResult<DfValue>, diff: Self::Diff| -> ReadySetResult<DfValue> {
                if diff.value.is_none() {
                    return curr;
                }

                match self.op {
                    Aggregation::Count { .. } => apply_count(curr?, diff),
                    Aggregation::Sum => apply_sum(curr?, diff),
                    Aggregation::Avg => apply_avg(curr?, diff),
                    Aggregation::GroupConcat { separator: _ } => internal!(
                        "GroupConcats are separate from the other aggregations in the dataflow."
                    ),
                }
            };

        diffs
            .fold(Ok(current.cloned().unwrap_or(self.new_data()?)), apply_diff)
            .map(Some)
    }

    fn description(&self, detailed: bool) -> String {
        if !detailed {
            return match self.op {
                Aggregation::Count { .. } => "+".to_owned(),
                Aggregation::Sum => "𝛴".to_owned(),
                Aggregation::Avg => "Avg".to_owned(),
                Aggregation::GroupConcat { separator: ref s } => {
                    format!("||({})", s)
                }
            };
        }

        let op_string = match self.op {
            Aggregation::Count { .. } => "|*|".to_owned(),
            Aggregation::Sum => format!("𝛴({})", self.over),
            Aggregation::Avg => format!("Avg({})", self.over),
            Aggregation::GroupConcat { separator: ref s } => format!("||({}, {})", s, self.over),
        };
        let group_cols = self
            .group
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} γ[{}]", op_string, group_cols)
    }

    fn over_column(&self) -> usize {
        self.over
    }

    fn output_col_type(&self) -> DfType {
        self.out_ty.clone()
    }

    fn empty_value(&self) -> Option<DfValue> {
        match self.op {
            Aggregation::Count { .. } => Some(0.into()),
            _ => None,
        }
    }

    fn emit_empty(&self) -> bool {
        match self.op {
            Aggregation::Count { .. } | Aggregation::GroupConcat { .. } => {
                self.group_by().is_empty()
            }
            _ => false,
        }
    }
}

// TODO: These unit tests are lengthy, repetitive, and hard to read.
// Could look into refactoring / creating a more robust testing infrastructure to consolidate
// logic and create test cases more easily.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ops, LookupIndex};

    fn setup(aggregation: Aggregation, mat: bool) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y"]);
        g.set_op(
            "identity",
            &["x", "ys"],
            aggregation
                .over(s.as_global(), 1, &[0], &DfType::Double)
                .unwrap(),
            mat,
        );
        g
    }

    fn setup_multicolumn(aggregation: Aggregation, mat: bool) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y", "z"]);
        g.set_op(
            "identity",
            &["x", "z", "ys"],
            aggregation
                .over(s.as_global(), 1, &[0, 2], &DfType::Double)
                .unwrap(),
            mat,
        );
        g
    }

    #[test]
    fn it_describes() {
        let src = 0.into();

        let c = Aggregation::Count
            .over(src, 1, &[0, 2], &DfType::Unknown)
            .unwrap();
        assert_eq!(c.description(true), "|*| γ[0, 2]");

        let s = Aggregation::Sum
            .over(src, 1, &[2, 0], &DfType::Unknown)
            .unwrap();
        assert_eq!(s.description(true), "𝛴(1) γ[2, 0]");

        let a = Aggregation::Avg
            .over(src, 1, &[2, 0], &DfType::Unknown)
            .unwrap();
        assert_eq!(a.description(true), "Avg(1) γ[2, 0]");
    }

    /// Testing count emits correct records with single column group and single over column
    /// Records are in the form of (GroupCol, OverCol).
    /// Includes adding and removing records from different groups independently and in batch.
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn count_forwards() {
        let mut c = setup(Aggregation::Count, true);

        // Add Group=1, Value=1
        let u: Record = vec![1.into(), 1.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(1,1)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }

        // Add Group=2, Value=2
        let u: Record = vec![2.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(1,1)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }

        // Add Group=1, Value=2
        let u: Record = vec![1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        //Expect Negative(1,1), Positive(1,2)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
            }
            _ => unreachable!(),
        }

        // Remove Group=1, Value=1
        let u = (vec![1.into(), 1.into()], false);
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1,2), Positive(1,1)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }

        // Test multiple records at once
        let u = vec![
            (vec![1.into(), 1.into()], false),
            (vec![1.into(), 1.into()], true),
            (vec![1.into(), 2.into()], true),
            (vec![2.into(), 2.into()], false),
            (vec![2.into(), 2.into()], true),
            (vec![2.into(), 3.into()], true),
            (vec![2.into(), 1.into()], true),
            (vec![3.into(), 3.into()], true),
        ];

        // Group 1 expect Negative(1,1), Positive(1,2)
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 5);
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 1.into() && r[1] == 1.into()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 1.into() && r[1] == 2.into()
        } else {
            false
        }));

        // Group 2 expect Negative(2,1), Positive(2,3)
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 2.into() && r[1] == 1.into()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 2.into() && r[1] == 3.into()
        } else {
            false
        }));

        //Group 3 expect Positive(3,1)
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 3.into() && r[1] == 1.into()
        } else {
            false
        }));
    }

    #[test]
    fn count_empty_group() {
        let mut c = setup(Aggregation::Count, true);

        let u = Record::from(vec![1.into(), 1.into()]);
        let rs = c.narrow_one(u, true);
        assert_eq!(
            rs,
            vec![Record::Positive(vec![1.into(), 1.into(), 1.into()])].into()
        );

        let del = Record::Negative(vec![1.into(), 1.into()]);
        let del_res = c.narrow_one(del, true);
        assert_eq!(
            del_res,
            vec![Record::Negative(vec![1.into(), 1.into(), 1.into()])].into()
        );
    }

    /// Testing SUM emits correct records with single column group and single over column
    /// Records are in the form of (GroupCol, OverCol)
    /// Includes adding and removing records from different groups independently and in batch.
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn sum_forwards() {
        let mut c = setup(Aggregation::Sum, true);

        // Add Group=1, Value=2
        let u: Record = vec![1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(1,2)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], (2.).try_into().unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=2, Value=5
        let u: Record = vec![2.into(), 5.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(2,5)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], (5.).try_into().unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=1, Value=3
        let u: Record = vec![1.into(), 3.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1,2), Positive(1,5)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], (2.).try_into().unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], (5.).try_into().unwrap());
            }
            _ => unreachable!(),
        }

        // Remove Group=1, Value=2
        let u = (vec![1.into(), 2.into()], false);
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1,5), Positive(1,3)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], (5.).try_into().unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], (3.).try_into().unwrap());
            }
            _ => unreachable!(),
        }

        // Test multiple records at once
        let u = vec![
            (vec![1.into(), 2.into()], true),
            (vec![1.into(), 3.into()], true),
            (vec![1.into(), 2.into()], false),
            (vec![1.into(), 5.into()], true),
            (vec![1.into(), 3.into()], false), // Group 1 gains +5
            (vec![2.into(), 5.into()], true),
            (vec![2.into(), 5.into()], false),
            (vec![2.into(), 2.into()], true),
            (vec![2.into(), 2.into()], false),
            (vec![2.into(), 5.into()], false), // Group 2 loses last row and dissapears
            (vec![3.into(), 3.into()], true),  // Group 3 is new, +3
        ];

        // Group 1: 3 -> 8
        // Group 2: 5 -> 4
        // Group 3: new 3

        // multiple positives and negatives should update aggregation value by appropriate amount
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 4);
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 1.into() && r[1] == (3.).try_into().unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 1.into() && r[1] == (8.).try_into().unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 2.into() && r[1] == (5.).try_into().unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 3.into() && r[1] == (3.).try_into().unwrap()
        } else {
            false
        }));
    }

    /// Testing AVG emits correct records with single column group and single integer over column
    /// Records are in the form of (GroupCol, OverCol)
    /// Includes adding and removing records from different groups independently and in batch.
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn avg_of_integers_forwards() {
        use std::convert::TryFrom;
        let mut c = setup(Aggregation::Avg, true);

        // Add Group=1, Value=2
        let u: Record = vec![1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(1, 2.0)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(2.0).unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=2, Value=5
        let u: Record = vec![2.into(), 5.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        //Expect Positive(2, 5.0)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], DfValue::try_from(5.0).unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=1, Value=3. Expect -2.0, +2.5
        let u: Record = vec![1.into(), 3.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1, 2.0), Positive(1, 2.5)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(2.0).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(2.5).unwrap());
            }
            _ => unreachable!(),
        }

        // Remove Group=1, Value=2
        let u = (vec![1.into(), 2.into()], false);
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1, 2.5), Positive(1, 3.0)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(2.5).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(3.0).unwrap());
            }
            _ => unreachable!(),
        }

        // Test multiple records at once
        // Group 1: (3/1) -> (24/2)
        // Group 2: (5/1) -> (25/4)
        // Group 3: new 3
        let u = vec![
            (vec![1.into(), 14.into()], true),
            (vec![1.into(), 8.into()], true),
            (vec![1.into(), 3.into()], true),
            (vec![1.into(), 2.into()], false),
            (vec![1.into(), 2.into()], false),
            (vec![2.into(), 10.into()], true),
            (vec![2.into(), 7.into()], true),
            (vec![2.into(), 3.into()], true),
            (vec![2.into(), 2.into()], true),
            (vec![2.into(), 2.into()], false),
            (vec![3.into(), 3.into()], true),
        ];

        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 5);
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 1.into() && r[1] == DfValue::try_from(3.0).unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 1.into() && r[1] == DfValue::try_from(12.0).unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 2.into() && r[1] == DfValue::try_from(5.0).unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 2.into() && r[1] == DfValue::try_from(6.25).unwrap()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 3.into() && r[1] == DfValue::try_from(3.0).unwrap()
        } else {
            false
        }));
    }

    /// Testing AVG emits correct records with single column group and single decimal over column
    /// Records are in the form of (GroupCol, OverCol)
    /// Includes adding and removing records from different groups independently and in batch.
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn avg_of_decimals_forwards() {
        use std::convert::TryFrom;
        let mut c = setup(Aggregation::Avg, true);

        // Add [1, 1.25]
        let u: Record = vec![1.into(), DfValue::try_from(1.25).unwrap()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(1, 1.25)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(
                    r[1],
                    DfValue::try_from(DfValue::try_from(1.25).unwrap()).unwrap()
                );
            }
            _ => unreachable!(),
        }

        // Add [2, 5.5]
        let u: Record = vec![2.into(), DfValue::try_from(5.5).unwrap()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        // Expect Positive(2, 5.5)
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], DfValue::try_from(5.5).unwrap());
            }
            _ => unreachable!(),
        }

        // Add [1,2.25]
        // Now: [1, 2.25], [1, 1.25]
        let u: Record = vec![1.into(), DfValue::try_from(2.25).unwrap()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1,1.25), Positive(1, 1.75)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(1.25).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(1.75).unwrap());
            }
            _ => unreachable!(),
        }

        // Remove [1, 1.25]
        let u = (vec![1.into(), DfValue::try_from(1.25).unwrap()], false);
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        // Expect Negative(1, 1.75), Positive(1, 2.25)
        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(1.75).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], DfValue::try_from(2.25).unwrap());
            }
            _ => unreachable!(),
        }

        // Test multiple records at once
        // Group 1: (2.25/1) -> (15.75/3) = 5.25
        // Group 2: (5.5/1) -> (10.5/2) = 5.25
        // Group 3: new 3
        let u = vec![
            (vec![1.into(), DfValue::try_from(12.4).unwrap()], true),
            (vec![1.into(), DfValue::try_from(1.15).unwrap()], true),
            (vec![1.into(), DfValue::try_from(1.05).unwrap()], true),
            (vec![1.into(), DfValue::try_from(1.1).unwrap()], true),
            (vec![1.into(), DfValue::try_from(1.15).unwrap()], false),
            (vec![1.into(), DfValue::try_from(1.05).unwrap()], false),
            (vec![2.into(), DfValue::try_from(5.25).unwrap()], true),
            (vec![2.into(), DfValue::try_from(0.75).unwrap()], true),
            (vec![2.into(), DfValue::try_from(1.0).unwrap()], false),
            (vec![3.into(), 3.into()], true),
        ];

        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 5);
        let precision = Some(10.0_f64.powf(2.0_f64));
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 1.into()
                && r[1].equal_under_error_margin(&DfValue::try_from(2.25).unwrap(), precision)
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 1.into()
                && r[1].equal_under_error_margin(&DfValue::try_from(5.25).unwrap(), precision)
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 2.into()
                && r[1].equal_under_error_margin(&DfValue::try_from(5.5).unwrap(), precision)
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 2.into()
                && r[1].equal_under_error_margin(&DfValue::try_from(5.25).unwrap(), precision)
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 3.into()
                && r[1].equal_under_error_margin(&DfValue::try_from(3.0).unwrap(), precision)
        } else {
            false
        }));
    }

    /// Testing AVG emits correct records with multple group by columns and single decimal
    /// over column. Similar to `avg_of_decimals_forwards` with additional group column.
    /// Records are in the form of (GroupCol1, OverCol, GroupCol2).
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn avg_groups_by_multiple_columns() {
        use std::convert::TryFrom;
        let mut c = setup_multicolumn(Aggregation::Avg, true);

        // Add Group=(1,1), Value=1.25
        let u: Record = vec![1.into(), DfValue::try_from(1.25).unwrap(), 1.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[2], DfValue::try_from(1.25).unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=(2,1), Value=5.5
        let u: Record = vec![2.into(), DfValue::try_from(5.5).unwrap(), 1.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[2], DfValue::try_from(5.5).unwrap());
            }
            _ => unreachable!(),
        }

        // Add Group=(1,1), Value=2.25
        let u: Record = vec![1.into(), DfValue::try_from(2.25).unwrap(), 1.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[2], DfValue::try_from(1.25).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[2], DfValue::try_from(1.75).unwrap());
            }
            _ => unreachable!(),
        }

        // Remove Group=(1,1), Value=1.25
        let u = (
            vec![1.into(), DfValue::try_from(1.25).unwrap(), 1.into()],
            false,
        );
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[2], DfValue::try_from(1.75).unwrap());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[2], DfValue::try_from(2.25).unwrap());
            }
            _ => unreachable!(),
        }
    }

    /// Testing COUNT emits correct records with multple group by columns and single
    /// over column. Similar to `count_forwards` with additional group column.
    /// Records are in the form of (GroupCol1, OverCol, GroupCol2).
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn count_groups_by_multiple_columns() {
        let mut c = setup_multicolumn(Aggregation::Count, true);

        // Add Group=(1,2), Value=1
        let u: Record = vec![1.into(), 1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 1.into());
            }
            _ => unreachable!(),
        }

        // Add Group=(2,2), Value=1
        let u: Record = vec![2.into(), 1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 1.into());
            }
            _ => unreachable!(),
        }

        // Add Group=(1,2), Value=1
        let u: Record = vec![1.into(), 1.into(), 2.into()].into();
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 1.into());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 2.into());
            }
            _ => unreachable!(),
        }

        // Remove Group=(1,2), Value=1
        let u = (vec![1.into(), 1.into(), 2.into()], false);
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 2.into());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
                assert_eq!(r[2], 1.into());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn it_suggests_indices() {
        let me = 1.into();
        let c = setup(Aggregation::Avg, false);
        let idx = c.node().suggest_indexes(me);

        // should only add index on own columns
        assert_eq!(idx.len(), 1);
        assert!(idx.contains_key(&me));

        // should only index on the group-by column
        assert_eq!(idx[&me], LookupIndex::Strict(Index::hash_map(vec![0])));
    }

    #[test]
    fn it_resolves() {
        let c = setup(Aggregation::Avg, false);
        assert_eq!(
            c.node().resolve(0),
            Some(vec![(c.narrow_base_id().as_global(), 0)])
        );
        assert_eq!(c.node().resolve(1), None);
    }

    #[test]
    fn sum_add_zero() {
        let mut c = setup(Aggregation::Sum, true);
        let out = c.narrow_one_row(vec!["grp".into(), 1.into()], true);
        assert_eq!(
            out,
            vec![vec![
                DfValue::from("grp"),
                DfValue::try_from(1.0f64).unwrap(),
                DfValue::from(1)
            ]]
            .into()
        );

        // Adding 0 to the group should still add 1 to the row count (the last column)
        let out = c.narrow_one_row(vec!["grp".into(), 0.into()], true);
        assert_eq!(
            out,
            vec![
                (
                    vec![
                        DfValue::from("grp"),
                        DfValue::try_from(1.0f64).unwrap(),
                        DfValue::from(1)
                    ],
                    false
                ),
                (
                    vec![
                        DfValue::from("grp"),
                        DfValue::try_from(1.0f64).unwrap(),
                        DfValue::from(2)
                    ],
                    true
                )
            ]
            .into()
        );
    }
}
