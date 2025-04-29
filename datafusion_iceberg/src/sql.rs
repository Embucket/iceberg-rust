use std::ops::ControlFlow;

use datafusion::sql::sqlparser::{
    ast::{visit_relations_mut, Ident, ObjectNamePart},
    dialect::GenericDialect,
    parser::Parser,
};
use itertools::Itertools;

use crate::error::Error;

pub(crate) fn transform_name(input: &str) -> String {
    input.replace('.', "__")
}

pub(crate) fn transform_relations(sql: &str) -> Result<Vec<String>, Error> {
    let mut statements = Parser::parse_sql(&GenericDialect, sql)?;

    visit_relations_mut(&mut statements, |relation| {
        let combined_name = relation.0.iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => ident.value.as_str()
            })
            .intersperse(".")
            .collect::<String>();
        
        let transformed_name = transform_name(&combined_name);
        relation.0 = vec![ObjectNamePart::Identifier(Ident::new(transformed_name))];
        
        ControlFlow::<()>::Continue(())
    });

    Ok(statements
        .into_iter()
        .map(|statement| statement.to_string())
        .collect())
}
