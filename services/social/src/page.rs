//! Forward pagination and `Query` filters. A cursor is the offset of the next row.

use std::cmp::Ordering;

use sc_proto::common::api::v1::{
    composite_filter::LogicalOperator,
    filter::FilterType,
    property_filter::{ComparisonOperator, ValueField},
    Filter, PaginationArguments, PaginationInfo, PropertyFilter,
};
use tonic::Status;

const DEFAULT_PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub offset: i64,
    pub limit: i64,
}

impl Page {
    pub fn new(arguments: Option<&PaginationArguments>) -> Result<Self, Status> {
        let (first, after) = arguments.map_or((0, ""), |a| (a.first, a.after.as_str()));
        let first = if first == 0 { DEFAULT_PAGE } else { first.min(MAX_PAGE) };
        let offset = match after {
            "" => 0,
            cursor => cursor.parse::<u32>().map_err(|_| Status::invalid_argument("bad pagination cursor"))?,
        };
        Ok(Self { offset: i64::from(offset), limit: i64::from(first) })
    }

    /// For SQL: one row more than the page, which tells whether another page follows.
    pub fn fetch(&self) -> i64 {
        self.limit + 1
    }

    /// Cuts the rows fetched with `LIMIT fetch() OFFSET offset` down to the page.
    pub fn finish<T>(&self, mut rows: Vec<T>) -> (Vec<T>, PaginationInfo) {
        let has_next_page = rows.len() as i64 > self.limit;
        rows.truncate(self.limit as usize);
        let info = PaginationInfo {
            has_next_page,
            has_previous_page: self.offset > 0,
            start_cursor: self.offset.to_string(),
            end_cursor: (self.offset + rows.len() as i64).to_string(),
        };
        (rows, info)
    }

    /// The page out of a complete list.
    pub fn slice<T>(&self, items: Vec<T>) -> (Vec<T>, PaginationInfo) {
        self.finish(items.into_iter().skip(self.offset as usize).collect())
    }
}

fn compare(left: &str, right: &str) -> Ordering {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        _ => left.cmp(right),
    }
}

fn property_matches(filter: &PropertyFilter, field: &str) -> bool {
    let expected = match &filter.value_field {
        Some(ValueField::Value(value)) => value.as_str(),
        _ => "",
    };
    match filter.operator() {
        ComparisonOperator::Equals => field.eq_ignore_ascii_case(expected),
        ComparisonOperator::LessThan => compare(field, expected).is_lt(),
        ComparisonOperator::GreaterThan => compare(field, expected).is_gt(),
        ComparisonOperator::Like => {
            field.to_lowercase().contains(&expected.trim_matches('%').to_lowercase())
        }
        ComparisonOperator::In => match &filter.value_field {
            Some(ValueField::Values(values)) => values.data.iter().any(|v| v.eq_ignore_ascii_case(field)),
            _ => field.eq_ignore_ascii_case(expected),
        },
        ComparisonOperator::Unspecified => true,
    }
}

/// Whether a row passes a `Query` filter. `field` gives a property of the row, and a property the
/// row does not have fails the comparison. No filter passes everything.
pub fn matches(filter: Option<&Filter>, field: &dyn Fn(&str) -> Option<String>) -> bool {
    match filter.and_then(|f| f.filter_type.as_ref()) {
        None => true,
        Some(FilterType::PropertyFilter(property)) => {
            field(&property.property).is_some_and(|value| property_matches(property, &value))
        }
        Some(FilterType::CompositeFilter(composite)) => {
            let mut filters = composite.filters.iter().map(|f| matches(Some(f), field));
            match composite.operator() {
                LogicalOperator::Or => filters.any(|passed| passed),
                LogicalOperator::Not => !filters.any(|passed| passed),
                LogicalOperator::And | LogicalOperator::Unspecified => filters.all(|passed| passed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::common::api::v1::CompositeFilter;

    fn arguments(first: u32, after: &str) -> PaginationArguments {
        PaginationArguments { first, after: after.to_string() }
    }

    fn property(name: &str, operator: ComparisonOperator, value: &str) -> Filter {
        let filter = PropertyFilter {
            property: name.to_string(),
            operator: operator as i32,
            value_field: Some(ValueField::Value(value.to_string())),
        };
        Filter { filter_type: Some(FilterType::PropertyFilter(filter)) }
    }

    #[test]
    fn the_clients_first_250_is_kept() {
        assert_eq!(Page::new(Some(&arguments(250, ""))).unwrap(), Page { offset: 0, limit: 250 });
        assert_eq!(Page::new(None).unwrap().limit, i64::from(DEFAULT_PAGE));
        assert_eq!(Page::new(Some(&arguments(100_000, ""))).unwrap().limit, i64::from(MAX_PAGE));
        assert!(Page::new(Some(&arguments(1, "x"))).is_err());
        assert!(Page::new(Some(&arguments(1, "-1"))).is_err());
    }

    #[test]
    fn pages_walk_a_list() {
        let page = Page::new(Some(&arguments(2, ""))).unwrap();
        let (first, info) = page.slice(vec![1, 2, 3]);
        assert_eq!(first, [1, 2]);
        assert!(info.has_next_page && !info.has_previous_page);

        let page = Page::new(Some(&arguments(2, &info.end_cursor))).unwrap();
        let (second, info) = page.slice(vec![1, 2, 3]);
        assert_eq!(second, [3]);
        assert!(!info.has_next_page && info.has_previous_page);
    }

    #[test]
    fn filters_combine() {
        let field = |name: &str| match name {
            "nickname" => Some("Pilot".to_string()),
            "age" => Some("10".to_string()),
            _ => None,
        };
        assert!(matches(None, &field));
        assert!(matches(Some(&property("nickname", ComparisonOperator::Equals, "pilot")), &field));
        assert!(matches(Some(&property("nickname", ComparisonOperator::Like, "%IL%")), &field));
        assert!(matches(Some(&property("age", ComparisonOperator::GreaterThan, "9")), &field));
        assert!(!matches(Some(&property("missing", ComparisonOperator::Equals, "x")), &field));

        let both = CompositeFilter {
            operator: LogicalOperator::And as i32,
            filters: vec![
                property("age", ComparisonOperator::LessThan, "11"),
                property("nickname", ComparisonOperator::Equals, "other"),
            ],
        };
        let filter = Filter { filter_type: Some(FilterType::CompositeFilter(both)) };
        assert!(!matches(Some(&filter), &field));
    }
}
