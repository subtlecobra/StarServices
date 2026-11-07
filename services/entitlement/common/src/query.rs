//! sc.external.common.api.v1.Query as SQL. Properties are looked up in a list per service, so request
//! text only ever reaches the database as a parameter.

use sc_proto::common::api::v1::{
    composite_filter::LogicalOperator, filter::FilterType, property_filter::ComparisonOperator,
    property_filter::ValueField, CompositeFilter, Filter, PaginationInfo, PaginationOrder, PropertyFilter, Query,
};
use tonic::Status;

const DEFAULT_PAGE: u32 = 100;
const MAX_PAGE: u32 = 1_000;

pub enum Kind {
    Text,
    Number,
    /// A number, which may be given as the name of an enum value.
    Enum(fn(&str) -> Option<i32>),
}

pub struct Field {
    pub property: &'static str,
    pub expression: &'static str,
    pub kind: Kind,
}

#[derive(Debug)]
pub struct Compiled {
    /// A boolean expression, and the text bound to its parameters in order.
    pub condition: String,
    pub parameters: Vec<String>,
    /// ORDER BY, LIMIT and OFFSET. LIMIT is one more than the page, to learn if another follows.
    pub tail: String,
    pub offset: u64,
    pub page: u32,
}

/// Clients use both `entity_class_guid` and `entityClassGuid`.
fn normalized(property: &str) -> String {
    property.chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect()
}

fn field<'a>(fields: &'a [Field], property: &str) -> Result<&'a Field, Status> {
    let wanted = normalized(property);
    fields
        .iter()
        .find(|field| normalized(field.property) == wanted)
        .ok_or_else(|| Status::invalid_argument(format!("unknown property {property}")))
}

struct Compiler<'a> {
    fields: &'a [Field],
    first_parameter: usize,
    parameters: Vec<String>,
}

impl Compiler<'_> {
    fn bind(&mut self, field: &Field, value: &str) -> Result<String, Status> {
        let invalid = || Status::invalid_argument(format!("invalid value {value} for {}", field.property));
        let (text, cast) = match field.kind {
            Kind::Text => (value.to_string(), ""),
            Kind::Number => {
                value.parse::<f64>().map_err(|_| invalid())?;
                (value.to_string(), "::numeric")
            }
            Kind::Enum(from_name) => {
                let number = value.parse().ok().or_else(|| from_name(value)).ok_or_else(invalid)?;
                (i32::to_string(&number), "::numeric")
            }
        };
        self.parameters.push(text);
        Ok(format!("${}::text{cast}", self.first_parameter + self.parameters.len() - 1))
    }

    fn property(&mut self, filter: &PropertyFilter) -> Result<String, Status> {
        let field = field(self.fields, &filter.property)?;
        let left = match field.kind {
            Kind::Text => format!("({})", field.expression),
            _ => format!("({})::numeric", field.expression),
        };
        let values: Vec<&str> = match &filter.value_field {
            Some(ValueField::Value(value)) => vec![value],
            Some(ValueField::Values(values)) => values.data.iter().map(String::as_str).collect(),
            None => Vec::new(),
        };
        let operator = ComparisonOperator::try_from(filter.operator).unwrap_or(ComparisonOperator::Unspecified);
        if operator == ComparisonOperator::In || values.len() != 1 {
            if values.is_empty() {
                return Ok("FALSE".to_string());
            }
            let bound = values.iter().map(|value| self.bind(field, value)).collect::<Result<Vec<_>, _>>()?;
            return Ok(format!("{left} IN ({})", bound.join(", ")));
        }
        let symbol = match operator {
            ComparisonOperator::LessThan => "<",
            ComparisonOperator::GreaterThan => ">",
            ComparisonOperator::Like if matches!(field.kind, Kind::Text) => "LIKE",
            ComparisonOperator::Like => return Err(Status::invalid_argument("LIKE needs a text property")),
            _ => "=",
        };
        Ok(format!("{left} {symbol} {}", self.bind(field, values[0])?))
    }

    fn composite(&mut self, filter: &CompositeFilter) -> Result<String, Status> {
        let parts = filter.filters.iter().map(|part| self.filter(part)).collect::<Result<Vec<_>, _>>()?;
        if parts.is_empty() {
            return Ok("TRUE".to_string());
        }
        let operator = LogicalOperator::try_from(filter.operator).unwrap_or(LogicalOperator::Unspecified);
        Ok(match operator {
            LogicalOperator::Or => format!("({})", parts.join(" OR ")),
            LogicalOperator::Not => format!("NOT ({})", parts.join(" AND ")),
            _ => format!("({})", parts.join(" AND ")),
        })
    }

    fn filter(&mut self, filter: &Filter) -> Result<String, Status> {
        match &filter.filter_type {
            Some(FilterType::CompositeFilter(composite)) => self.composite(composite),
            Some(FilterType::PropertyFilter(property)) => self.property(property),
            None => Ok("TRUE".to_string()),
        }
    }
}

/// Compiles `query`. Its parameters are numbered from `first_parameter`, and `tie_break` keeps the
/// order stable between pages.
pub fn compile(
    query: Option<&Query>,
    fields: &[Field],
    first_parameter: usize,
    tie_break: &str,
) -> Result<Compiled, Status> {
    let default_query = Query::default();
    let query = query.unwrap_or(&default_query);
    let mut compiler = Compiler { fields, first_parameter, parameters: Vec::new() };
    let condition = query.filter.as_ref().map_or(Ok("TRUE".to_string()), |filter| compiler.filter(filter))?;

    let pagination = query.pagination.clone().unwrap_or_default();
    let offset = match pagination.after.as_str() {
        "" => 0,
        cursor => cursor.parse().map_err(|_| Status::invalid_argument("invalid cursor"))?,
    };
    let page = match pagination.first {
        0 => DEFAULT_PAGE,
        first => first.min(MAX_PAGE),
    };
    let mut order = tie_break.to_string();
    if let Some(sort) = query.sort.as_ref().filter(|sort| !sort.by.is_empty()) {
        let direction = match PaginationOrder::try_from(sort.order) {
            Ok(PaginationOrder::Descending) => "DESC",
            _ => "ASC",
        };
        order = format!("({}) {direction}, {tie_break}", field(fields, &sort.by)?.expression);
    }
    let tail = format!("ORDER BY {order} LIMIT {} OFFSET {offset}", u64::from(page) + 1);
    Ok(Compiled { condition, parameters: compiler.parameters, tail, offset, page })
}

impl Compiled {
    /// Cuts the extra row off and describes the page. Cursors are row offsets.
    pub fn page_of<T>(&self, rows: &mut Vec<T>) -> PaginationInfo {
        let has_next_page = rows.len() > self.page as usize;
        rows.truncate(self.page as usize);
        PaginationInfo {
            has_next_page,
            has_previous_page: self.offset > 0,
            start_cursor: self.offset.to_string(),
            end_cursor: (self.offset + rows.len() as u64).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_proto::common::api::v1::{PaginationArguments, PaginationSort, PropertyFilterValues};

    fn status(name: &str) -> Option<i32> {
        (name == "ACTIVE").then_some(2)
    }

    const FIELDS: &[Field] = &[
        Field { property: "entity_class_guid", expression: "guid", kind: Kind::Text },
        Field { property: "tier", expression: "tier", kind: Kind::Number },
        Field { property: "status", expression: "status", kind: Kind::Enum(status) },
    ];

    fn property(property: &str, operator: ComparisonOperator, value: &str) -> Filter {
        let filter = PropertyFilter {
            property: property.into(),
            operator: operator as i32,
            value_field: Some(ValueField::Value(value.into())),
        };
        Filter { filter_type: Some(FilterType::PropertyFilter(filter)) }
    }

    fn query(filter: Filter) -> Query {
        Query { filter: Some(filter), ..Default::default() }
    }

    #[test]
    fn nothing_matches_everything() {
        let compiled = compile(None, FIELDS, 2, "id").unwrap();
        assert_eq!(compiled.condition, "TRUE");
        assert_eq!(compiled.tail, "ORDER BY id LIMIT 101 OFFSET 0");
    }

    #[test]
    fn values_only_travel_as_parameters() {
        let injection = "x' OR '1'='1";
        let query = query(property("entityClassGuid", ComparisonOperator::Equals, injection));
        let compiled = compile(Some(&query), FIELDS, 2, "id").unwrap();
        assert_eq!(compiled.condition, "(guid) = $2::text");
        assert_eq!(compiled.parameters, [injection]);
    }

    #[test]
    fn enums_accept_names_and_numbers() {
        for value in ["ACTIVE", "2"] {
            let query = query(property("status", ComparisonOperator::Equals, value));
            let compiled = compile(Some(&query), FIELDS, 1, "id").unwrap();
            assert_eq!(compiled.condition, "(status)::numeric = $1::text::numeric");
            assert_eq!(compiled.parameters, ["2"]);
        }
    }

    #[test]
    fn composites_nest_and_number_their_parameters() {
        let or = CompositeFilter {
            operator: LogicalOperator::Or as i32,
            filters: vec![
                property("tier", ComparisonOperator::GreaterThan, "1"),
                property("entity_class_guid", ComparisonOperator::Like, "a%"),
            ],
        };
        let not = CompositeFilter {
            operator: LogicalOperator::Not as i32,
            filters: vec![Filter { filter_type: Some(FilterType::CompositeFilter(or)) }],
        };
        let query = query(Filter { filter_type: Some(FilterType::CompositeFilter(not)) });
        let compiled = compile(Some(&query), FIELDS, 3, "id").unwrap();
        assert_eq!(compiled.condition, "NOT (((tier)::numeric > $3::text::numeric OR (guid) LIKE $4::text))");
        assert_eq!(compiled.parameters, ["1", "a%"]);
    }

    #[test]
    fn in_binds_every_value_and_an_empty_list_matches_nothing() {
        let filter = |data: Vec<String>| PropertyFilter {
            property: "tier".into(),
            operator: ComparisonOperator::In as i32,
            value_field: Some(ValueField::Values(PropertyFilterValues { data })),
        };
        let wrap = |filter| query(Filter { filter_type: Some(FilterType::PropertyFilter(filter)) });
        let compiled = compile(Some(&wrap(filter(vec!["1".into(), "2".into()]))), FIELDS, 1, "id").unwrap();
        assert_eq!(compiled.condition, "(tier)::numeric IN ($1::text::numeric, $2::text::numeric)");
        let compiled = compile(Some(&wrap(filter(Vec::new()))), FIELDS, 1, "id").unwrap();
        assert_eq!(compiled.condition, "FALSE");
    }

    #[test]
    fn unknown_properties_and_bad_values_are_refused() {
        let unknown = query(property("password", ComparisonOperator::Equals, "x"));
        assert_eq!(compile(Some(&unknown), FIELDS, 1, "id").unwrap_err().code(), tonic::Code::InvalidArgument);
        let number = query(property("tier", ComparisonOperator::Equals, "1; DROP TABLE x"));
        assert_eq!(compile(Some(&number), FIELDS, 1, "id").unwrap_err().code(), tonic::Code::InvalidArgument);
        let like = query(property("tier", ComparisonOperator::Like, "1"));
        assert_eq!(compile(Some(&like), FIELDS, 1, "id").unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sorting_and_pages() {
        let query = Query {
            pagination: Some(PaginationArguments { first: 2, after: "4".into() }),
            sort: Some(PaginationSort { order: PaginationOrder::Descending as i32, by: "tier".into() }),
            ..Default::default()
        };
        let compiled = compile(Some(&query), FIELDS, 1, "id").unwrap();
        assert_eq!(compiled.tail, "ORDER BY (tier) DESC, id LIMIT 3 OFFSET 4");
        let mut rows = vec![1, 2, 3];
        let info = compiled.page_of(&mut rows);
        assert_eq!(rows, [1, 2]);
        assert!(info.has_next_page && info.has_previous_page);
        assert_eq!((info.start_cursor.as_str(), info.end_cursor.as_str()), ("4", "6"));
    }

    #[test]
    fn a_huge_page_is_capped_and_a_bad_cursor_refused() {
        let page = |first, after: &str| Query {
            pagination: Some(PaginationArguments { first, after: after.into() }),
            ..Default::default()
        };
        assert_eq!(compile(Some(&page(u32::MAX, "")), FIELDS, 1, "id").unwrap().page, MAX_PAGE);
        assert!(compile(Some(&page(1, "abc")), FIELDS, 1, "id").is_err());
    }
}
