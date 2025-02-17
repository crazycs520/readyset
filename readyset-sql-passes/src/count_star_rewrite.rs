use std::collections::HashMap;

use nom_sql::analysis::visit_mut::{walk_select_statement, VisitorMut};
use nom_sql::{
    Column, Expr, FunctionExpr, Relation, SelectStatement, SqlIdentifier, SqlQuery, TableExpr,
};
use readyset_errors::{internal_err, ReadySetError, ReadySetResult};

#[derive(Debug)]
pub struct CountStarRewriteVisitor<'schema> {
    schemas: &'schema HashMap<Relation, Vec<SqlIdentifier>>,
    tables: Option<Vec<TableExpr>>,
}

impl<'ast, 'schema> VisitorMut<'ast> for CountStarRewriteVisitor<'schema> {
    type Error = ReadySetError;

    fn visit_select_statement(
        &mut self,
        select_statement: &'ast mut SelectStatement,
    ) -> Result<(), Self::Error> {
        self.tables = Some(select_statement.tables.clone());
        walk_select_statement(self, select_statement)?;
        Ok(())
    }

    fn visit_function_expr(
        &mut self,
        function_expression: &'ast mut FunctionExpr,
    ) -> Result<(), Self::Error> {
        if *function_expression == FunctionExpr::CountStar {
            let bogo_table = self
                .tables
                .as_ref()
                .and_then(|ts| ts.first())
                .cloned()
                .ok_or_else(|| internal_err!("Tables should be set first"))?;

            let mut schema_iter = self
                .schemas
                .get(&bogo_table.table)
                .ok_or_else(|| ReadySetError::TableNotFound {
                    name: bogo_table.table.name.clone().into(),
                    schema: bogo_table.table.schema.clone().map(Into::into),
                })?
                .iter();
            // The columns in the table_columns map are actually columns as seen from the
            // current mir node. In this case, we've already passed star expansion, which
            // means the list of columns in the passed in table_columns map contains all
            // columns for the table in question. This means that we are garaunteed to have
            // at least one result in this columns list, and can simply choose the first
            // column.
            let bogo_column = schema_iter.next().unwrap();

            *function_expression = FunctionExpr::Count {
                expr: Box::new(Expr::Call(FunctionExpr::Call {
                    name: "coalesce".into(),
                    arguments: vec![
                        Expr::Column(Column {
                            name: bogo_column.clone(),
                            table: Some(bogo_table.table.clone()),
                        }),
                        Expr::Literal(0.into()),
                    ],
                })),
                distinct: false,
            };
        }

        Ok(())
    }
}

pub trait CountStarRewrite: Sized {
    fn rewrite_count_star(
        self,
        schemas: &HashMap<Relation, Vec<SqlIdentifier>>,
    ) -> ReadySetResult<Self>;
}

impl CountStarRewrite for SelectStatement {
    fn rewrite_count_star(
        mut self,
        schemas: &HashMap<Relation, Vec<SqlIdentifier>>,
    ) -> ReadySetResult<Self> {
        let mut visitor = CountStarRewriteVisitor {
            schemas,
            tables: None,
        };

        visitor.visit_select_statement(&mut self)?;
        Ok(self)
    }
}

impl CountStarRewrite for SqlQuery {
    fn rewrite_count_star(
        self,
        schemas: &HashMap<Relation, Vec<SqlIdentifier>>,
    ) -> ReadySetResult<SqlQuery> {
        match self {
            SqlQuery::Select(sq) => Ok(SqlQuery::Select(sq.rewrite_count_star(schemas)?)),
            _ => Ok(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use nom_sql::parser::parse_query;
    use nom_sql::{
        BinaryOperator, Column, Dialect, FieldDefinitionExpr, FunctionExpr, Literal, SqlQuery,
    };

    use super::*;

    #[test]
    fn it_expands_count_star() {
        // SELECT COUNT(*) FROM users;
        // -->
        // SELECT COUNT(coalesce(users.id, 0)) FROM users;
        let q = parse_query(Dialect::MySQL, "SELECT COUNT(*) FROM users;").unwrap();
        let mut schema = HashMap::new();
        schema.insert(
            "users".into(),
            vec!["id".into(), "name".into(), "age".into()],
        );

        let res = q.rewrite_count_star(&schema).unwrap();
        match res {
            SqlQuery::Select(tq) => {
                assert_eq!(
                    tq.fields,
                    vec![FieldDefinitionExpr::from(Expr::Call(FunctionExpr::Count {
                        expr: Box::new(Expr::Call(FunctionExpr::Call {
                            name: "coalesce".into(),
                            arguments: vec![
                                Expr::Column(Column::from("users.id")),
                                Expr::Literal(0.into())
                            ]
                        })),
                        distinct: false,
                    }))]
                );
            }
            // if we get anything other than a selection query back, something really weird is up
            _ => panic!(),
        }
    }

    #[test]
    fn it_expands_count_star_with_group_by() {
        // SELECT COUNT(*) FROM users GROUP BY id;
        // -->
        // SELECT COUNT(coalesce(users.id, 0)) FROM users GROUP BY id;
        let q = parse_query(Dialect::MySQL, "SELECT COUNT(*) FROM users GROUP BY id;").unwrap();
        let mut schema = HashMap::new();
        schema.insert(
            "users".into(),
            vec!["id".into(), "name".into(), "age".into()],
        );

        let res = q.rewrite_count_star(&schema).unwrap();
        match res {
            SqlQuery::Select(tq) => {
                assert_eq!(
                    tq.fields,
                    vec![FieldDefinitionExpr::from(Expr::Call(FunctionExpr::Count {
                        expr: Box::new(Expr::Call(FunctionExpr::Call {
                            name: "coalesce".into(),
                            arguments: vec![
                                Expr::Column(Column::from("users.id")),
                                Expr::Literal(0.into())
                            ]
                        })),
                        distinct: false,
                    }))]
                );
            }
            // if we get anything other than a selection query back, something really weird is up
            _ => panic!(),
        }
    }

    #[test]
    fn nested_in_expression() {
        let q = parse_query(Dialect::MySQL, "SELECT COUNT(*) + 1 FROM users;").unwrap();
        let schema = HashMap::from([(
            "users".into(),
            vec!["id".into(), "name".into(), "age".into()],
        )]);

        let res = q.rewrite_count_star(&schema).unwrap();
        match res {
            SqlQuery::Select(stmt) => {
                assert_eq!(
                    stmt.fields,
                    vec![FieldDefinitionExpr::from(Expr::BinaryOp {
                        lhs: Box::new(Expr::Call(FunctionExpr::Count {
                            expr: Box::new(Expr::Call(FunctionExpr::Call {
                                name: "coalesce".into(),
                                arguments: vec![
                                    Expr::Column(Column::from("users.id")),
                                    Expr::Literal(0.into())
                                ]
                            })),
                            distinct: false,
                        })),
                        op: BinaryOperator::Add,
                        rhs: Box::new(Expr::Literal(Literal::UnsignedInteger(1)))
                    })]
                );
            }
            _ => panic!(),
        }
    }
}
