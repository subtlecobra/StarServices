//! sc.external.common.api.v1.Query, applied in memory to rows a service has already narrowed down. A row
//! is seen through a lookup from property name to text.

use std::cmp::Ordering;

use sc_proto::common::api::v1::{
    composite_filter::LogicalOperator, filter::FilterType, property_filter::ComparisonOperator,
    property_filter::ValueField, Filter, PaginationArguments, PaginationInfo, PaginationOrder, PaginationSort,
    PropertyFilter,
};
use tonic::Status;

const DEFAULT_PAGE: usize = 500;
const MAX_PAGE: usize = 1000;

fn compare(left: &str, right: &str) -> Ordering {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        _ => left.cmp(right),
    }
}

/// SQL LIKE: `%` is any run of characters and `_` is exactly one.
fn like(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('%', rest)) => (0..=text.len()).any(|skip| like(rest, &text[skip..])),
        Some(('_', rest)) => !text.is_empty() && like(rest, &text[1..]),
        Some((first, rest)) => text.first() == Some(first) && like(rest, &text[1..]),
    }
}

fn property_matches(filter: &PropertyFilter, value: &str) -> bool {
    let expected: Vec<&str> = match &filter.value_field {
        Some(ValueField::Value(value)) => vec![value.as_str()],
        Some(ValueField::Values(values)) => values.data.iter().map(String::as_str).collect(),
        None => Vec::new(),
    };
    let text: Vec<char> = value.chars().collect();
    expected.iter().any(|candidate| match filter.operator() {
        ComparisonOperator::Unspecified | ComparisonOperator::Equals | ComparisonOperator::In => {
            compare(value, candidate) == Ordering::Equal
        }
        ComparisonOperator::LessThan => compare(value, candidate) == Ordering::Less,
        ComparisonOperator::GreaterThan => compare(value, candidate) == Ordering::Greater,
        ComparisonOperator::Like => like(&candidate.chars().collect::<Vec<_>>(), &text),
    })
}

/// Whether a row passes `filter`. No filter passes everything, and a property the row lacks fails.
pub fn matches(filter: Option<&Filter>, row: &dyn Fn(&str) -> Option<String>) -> bool {
    match filter.and_then(|filter| filter.filter_type.as_ref()) {
        None => true,
        Some(FilterType::PropertyFilter(property)) => {
            row(&property.property).is_some_and(|value| property_matches(property, &value))
        }
        Some(FilterType::CompositeFilter(composite)) => {
            let mut parts = composite.filters.iter().map(|part| matches(Some(part), row));
            match composite.operator() {
                LogicalOperator::Or => parts.any(|passed| passed),
                LogicalOperator::Not => !parts.any(|passed| passed),
                LogicalOperator::And | LogicalOperator::Unspecified => parts.all(|passed| passed),
            }
        }
    }
}

/// The values `filter` requires of `property` in any row it passes, when that can be told from its
/// top-level AND. Lets a service narrow its SQL before filtering.
pub fn required(filter: Option<&Filter>, property: &str) -> Vec<String> {
    match filter.and_then(|filter| filter.filter_type.as_ref()) {
        Some(FilterType::PropertyFilter(found))
            if found.property == property
                && matches!(
                    found.operator(),
                    ComparisonOperator::Unspecified | ComparisonOperator::Equals | ComparisonOperator::In
                ) =>
        {
            match &found.value_field {
                Some(ValueField::Value(value)) => vec![value.clone()],
                Some(ValueField::Values(values)) => values.data.clone(),
                None => Vec::new(),
            }
        }
        Some(FilterType::CompositeFilter(composite))
            if matches!(composite.operator(), LogicalOperator::And | LogicalOperator::Unspecified) =>
        {
            composite.filters.iter().map(|part| required(Some(part), property)).find(|v| !v.is_empty()).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

pub fn sort<T>(rows: &mut [T], sort: Option<&PaginationSort>, row: impl Fn(&T, &str) -> Option<String>) {
    let Some(sort) = sort.filter(|sort| !sort.by.is_empty()) else {
        return;
    };
    rows.sort_by(|left, right| {
        let order = compare(
            row(left, &sort.by).unwrap_or_default().as_str(),
            row(right, &sort.by).unwrap_or_default().as_str(),
        );
        if sort.order() == PaginationOrder::Descending {
            order.reverse()
        } else {
            order
        }
    });
}

/// One page of `rows`. Cursors are offsets into the filtered and sorted rows.
pub fn page<T>(rows: Vec<T>, pagination: Option<&PaginationArguments>) -> Result<(Vec<T>, PaginationInfo), Status> {
    let (first, after) = pagination.map_or((0, ""), |p| (p.first as usize, p.after.as_str()));
    let size = if first == 0 { DEFAULT_PAGE } else { first.min(MAX_PAGE) };
    let start = match after {
        "" => 0,
        cursor => cursor.parse::<usize>().map_err(|_| Status::invalid_argument("bad pagination cursor"))?,
    };
    let total = rows.len();
    let page: Vec<T> = rows.into_iter().skip(start).take(size).collect();
    let end = start.saturating_add(page.len());
    let info = PaginationInfo {
        has_next_page: end < total,
        has_previous_page: start > 0,
        start_cursor: start.to_string(),
        end_cursor: end.to_string(),
    };
    Ok((page, info))
}

#[cfg(test)]
mod tests {
    use sc_proto::common::api::v1::{CompositeFilter, PropertyFilterValues};

    use super::*;

    fn property(name: &str, operator: ComparisonOperator, value: &str) -> Filter {
        Filter {
            filter_type: Some(FilterType::PropertyFilter(PropertyFilter {
                property: name.into(),
                operator: operator as i32,
                value_field: Some(ValueField::Value(value.into())),
            })),
        }
    }

    fn composite(operator: LogicalOperator, filters: Vec<Filter>) -> Filter {
        Filter { filter_type: Some(FilterType::CompositeFilter(CompositeFilter { operator: operator as i32, filters })) }
    }

    fn row(name: &str) -> Option<String> {
        match name {
            "entity" => Some("urn:sc:orgs:org:string:crusader".into()),
            "score" => Some("150".into()),
            _ => None,
        }
    }

    #[test]
    fn comparisons_are_numeric_when_both_sides_are() {
        assert!(matches(Some(&property("score", ComparisonOperator::GreaterThan, "20")), &row));
        assert!(!matches(Some(&property("score", ComparisonOperator::LessThan, "20")), &row));
    }

    #[test]
    fn like_and_in() {
        assert!(matches(Some(&property("entity", ComparisonOperator::Like, "%crus_der")), &row));
        assert!(!matches(Some(&property("entity", ComparisonOperator::Like, "crus%")), &row));
        let within = Filter {
            filter_type: Some(FilterType::PropertyFilter(PropertyFilter {
                property: "score".into(),
                operator: ComparisonOperator::In as i32,
                value_field: Some(ValueField::Values(PropertyFilterValues { data: vec!["1".into(), "150".into()] })),
            })),
        };
        assert!(matches(Some(&within), &row));
    }

    #[test]
    fn composites_and_missing_properties() {
        let missing = property("nothing", ComparisonOperator::Equals, "x");
        let score = property("score", ComparisonOperator::Equals, "150");
        assert!(!matches(Some(&missing), &row));
        assert!(matches(Some(&composite(LogicalOperator::Or, vec![missing.clone(), score.clone()])), &row));
        assert!(!matches(Some(&composite(LogicalOperator::And, vec![missing.clone(), score.clone()])), &row));
        assert!(matches(Some(&composite(LogicalOperator::Not, vec![missing])), &row));
        assert!(matches(None, &row));
    }

    #[test]
    fn required_values_only_come_from_an_and() {
        let subject = property("subject", ComparisonOperator::Equals, "a");
        let other = property("score", ComparisonOperator::Equals, "1");
        let and = composite(LogicalOperator::And, vec![other.clone(), subject.clone()]);
        let or = composite(LogicalOperator::Or, vec![other, subject]);
        assert_eq!(required(Some(&and), "subject"), ["a"]);
        assert!(required(Some(&or), "subject").is_empty());
    }

    #[test]
    fn pages_walk_the_rows_by_cursor() {
        let first = PaginationArguments { first: 2, after: String::new() };
        let (rows, info) = page(vec![1, 2, 3], Some(&first)).unwrap();
        assert_eq!(rows, [1, 2]);
        assert!(info.has_next_page);
        let next = PaginationArguments { first: 2, after: info.end_cursor };
        let (rows, info) = page(vec![1, 2, 3], Some(&next)).unwrap();
        assert_eq!(rows, [3]);
        assert!(!info.has_next_page && info.has_previous_page);
        assert!(page(vec![1], Some(&PaginationArguments { first: 1, after: "x".into() })).is_err());
    }

    #[test]
    fn sorts_descending_by_number() {
        let mut rows = vec!["9", "100", "20"];
        let by = PaginationSort { order: PaginationOrder::Descending as i32, by: "n".into() };
        sort(&mut rows, Some(&by), |row, _| Some(row.to_string()));
        assert_eq!(rows, ["100", "20", "9"]);
    }
}
