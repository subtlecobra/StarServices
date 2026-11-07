//! Compiles the query filters of the proto into AQL expressions. Names are looked up in fixed tables
//! and values travel as bind parameters, so nothing from a request reaches the query text.

use sc_proto::services::entitygraph::v1::{
    container_filter, entity_class_filter, entity_filter, inventory_filter, prune_edge_filter, scalar_value,
    BitFilter, BitOperator, ComparisonOperator, ContainerFilter, EdgeFilter, EntityClassFilter, EntityEdgeType,
    EntityFilter, EntityPreCondition, EntityPreconditionOperator, InventoryFilter, LabelOperator, LogicalOperator,
    PruneEdgeFilter, PruneEdgePropertyFilter, ScalarValue,
};
use serde_json::{json, Map, Value};
use tonic::Status;

use crate::doc::hex;

pub const ALWAYS: &str = "true";

#[derive(Default)]
pub struct Binds(Map<String, Value>);

impl Binds {
    /// Stores a value and returns its placeholder.
    pub fn add(&mut self, value: impl Into<Value>) -> String {
        let name = format!("p{}", self.0.len());
        self.0.insert(name.clone(), value.into());
        format!("@{name}")
    }

    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }
}

/// How a column is stored: a 64-bit id is a decimal string, anything else is its JSON value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Big,
    Plain,
}

pub type Column = (&'static str, Kind);

fn normalized(name: &str) -> String {
    name.chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect()
}

pub fn entity_column(name: &str) -> Option<Column> {
    Some(match normalized(name).as_str() {
        "geid" => ("geid", Kind::Big),
        "ownerid" => ("ownerId", Kind::Big),
        "ownershiptype" => ("ownershipType", Kind::Big),
        "entityflags" => ("entityFlags", Kind::Big),
        "gameflags" => ("gameFlags", Kind::Big),
        "classguidcrc" => ("classGuidCrc", Kind::Plain),
        "scale" => ("scale", Kind::Plain),
        "radius" => ("radius", Kind::Plain),
        "stacksize" => ("stackSize", Kind::Plain),
        "parenturn" => ("parentUrn", Kind::Plain),
        "generation" => ("generation", Kind::Plain),
        "locationid" => ("locationId", Kind::Plain),
        "version" => ("version", Kind::Plain),
        "createdat" => ("createdAt", Kind::Plain),
        "updatedat" => ("updatedAt", Kind::Plain),
        "itemtypeenum" => ("itemTypeEnum", Kind::Plain),
        "itemsubtypeenum" => ("itemSubTypeEnum", Kind::Plain),
        "ownershipstatus" => ("ownershipStatus", Kind::Plain),
        "unstowedfrominventoryid" => ("unstowedFromInventoryId", Kind::Plain),
        "position.x" => ("position.x", Kind::Plain),
        "position.y" => ("position.y", Kind::Plain),
        "position.z" => ("position.z", Kind::Plain),
        "orientation.x" => ("orientation.x", Kind::Plain),
        "orientation.y" => ("orientation.y", Kind::Plain),
        "orientation.z" => ("orientation.z", Kind::Plain),
        "orientation.w" => ("orientation.w", Kind::Plain),
        _ => return None,
    })
}

pub fn inventory_column(name: &str) -> Option<Column> {
    Some(match normalized(name).as_str() {
        "id" => ("id", Kind::Plain),
        "ownerid" => ("ownerId", Kind::Big),
        "subjectid" => ("subjectId", Kind::Big),
        "createdat" => ("createdAt", Kind::Plain),
        "updatedat" => ("updatedAt", Kind::Plain),
        "unstowedin" => ("unstowedIn", Kind::Plain),
        "generation" => ("generation", Kind::Plain),
        "shared" => ("shared", Kind::Plain),
        "type" | "inventorytype" => ("inventoryType", Kind::Plain),
        "context" => ("context", Kind::Plain),
        "name" => ("name", Kind::Plain),
        "configuration.physical.capacity" => ("configuration.physical.capacity", Kind::Plain),
        "configuration.physical.occupancy" => ("configuration.physical.occupancy", Kind::Plain),
        _ => return None,
    })
}

pub fn container_column(name: &str) -> Option<Column> {
    Some(match normalized(name).as_str() {
        "guid" => ("guid", Kind::Plain),
        "geid" => ("geid", Kind::Big),
        "socpakcontenthash" => ("socpakContentHash", Kind::Plain),
        "socpakfilepathhash" => ("socpakFilePathHash", Kind::Plain),
        "generation" => ("generation", Kind::Plain),
        "createdat" => ("createdAt", Kind::Plain),
        "updatedat" => ("updatedAt", Kind::Plain),
        _ => return None,
    })
}

fn edge_column(name: &str) -> Option<Column> {
    Some(match normalized(name).as_str() {
        "type" => ("type", Kind::Plain),
        "attachmenttype" => ("attachmentType", Kind::Plain),
        "attachmentportcrc" => ("attachmentPortCrc", Kind::Plain),
        "rank" => ("rank", Kind::Plain),
        "context" => ("context", Kind::Plain),
        "subjectid" => ("subjectId", Kind::Big),
        "physical.occupancy" => ("physical.occupancy", Kind::Plain),
        _ => return None,
    })
}

fn class_column(name: &str) -> Option<Column> {
    Some(match normalized(name).as_str() {
        "guid" => ("guid", Kind::Plain),
        "guidhashcrc" => ("guidHashCrc", Kind::Plain),
        "classname" => ("className", Kind::Plain),
        "itemtype" => ("itemType", Kind::Plain),
        "itemsubtype" => ("itemSubType", Kind::Plain),
        "itemtypeenum" => ("itemTypeEnum", Kind::Plain),
        "itemsubtypeenum" => ("itemSubTypeEnum", Kind::Plain),
        "generation" => ("generation", Kind::Plain),
        _ => return None,
    })
}

fn class_name_column(language: &str) -> Option<Column> {
    Some(match language {
        "token" => ("name.token", Kind::Plain),
        "en" => ("name.en", Kind::Plain),
        "fr" => ("name.fr", Kind::Plain),
        "it" => ("name.it", Kind::Plain),
        "de" => ("name.de", Kind::Plain),
        "es" => ("name.es", Kind::Plain),
        _ => return None,
    })
}

fn known(column: Option<Column>, name: &str) -> Result<Column, Status> {
    column.ok_or_else(|| Status::invalid_argument(format!("unknown property {name}")))
}

fn edge_type(name: &str) -> Result<i32, Status> {
    let name = normalized(name);
    match name.strip_prefix("entityedgetype").unwrap_or(&name) {
        "attachedto" => Ok(EntityEdgeType::AttachedTo as i32),
        "stowedin" => Ok(EntityEdgeType::StowedIn as i32),
        "references" => Ok(EntityEdgeType::References as i32),
        _ => Err(Status::invalid_argument("unknown edge type")),
    }
}

fn scalar(value: &ScalarValue) -> Value {
    use scalar_value::ScalarType;
    match &value.scalar_type {
        None | Some(ScalarType::NullValue(_)) => Value::Null,
        Some(ScalarType::StringValue(text)) => json!(text),
        Some(ScalarType::BooleanValue(flag)) => json!(flag),
        Some(ScalarType::IntegerValue(number)) => json!(number),
        Some(ScalarType::DoubleValue(number)) => json!(number),
        Some(ScalarType::BigintValue(number)) => json!(number),
        Some(ScalarType::UnsignedIntegerValue(number)) => json!(number),
        Some(ScalarType::UnsignedBigintValue(number)) => json!(number),
        Some(ScalarType::BlobValue(bytes)) => json!(hex(bytes)),
        Some(ScalarType::TimestampValue(time)) => json!(time.seconds * 1000 + i64::from(time.nanos) / 1_000_000),
    }
}

/// A value as a 64-bit column stores it. Integers are printed whole, so no precision is lost.
fn big_text(value: &ScalarValue) -> Value {
    match scalar(value) {
        Value::Number(number) => match number.as_f64().filter(|_| !number.is_i64() && !number.is_u64()) {
            Some(float) => json!((float as u64).to_string()),
            None => json!(number.to_string()),
        },
        other => other,
    }
}

fn stored(kind: Kind, value: &ScalarValue) -> Value {
    if kind == Kind::Big {
        big_text(value)
    } else {
        scalar(value)
    }
}

/// The right side of an ordering comparison. A 64-bit column compares as a double there, which is
/// exact below 2^53 and good enough for a range above it.
fn ordered(kind: Kind, value: &ScalarValue) -> Value {
    match (kind, scalar(value)) {
        (Kind::Big, Value::String(text)) => json!(text.parse::<f64>().unwrap_or(0.0)),
        (_, other) => other,
    }
}

fn property(
    var: &str,
    (path, kind): Column,
    operator: ComparisonOperator,
    values: &[ScalarValue],
    binds: &mut Binds,
) -> String {
    use ComparisonOperator as Op;
    let Some(first) = values.first() else {
        return ALWAYS.to_string();
    };
    let column = format!("{var}.{path}");
    let numeric = if kind == Kind::Big { format!("TO_NUMBER({column})") } else { column.clone() };
    let many = || Value::Array(values.iter().map(|value| stored(kind, value)).collect());
    match operator {
        Op::LessThan => format!("{numeric} < {}", binds.add(ordered(kind, first))),
        Op::LessThanOrEqual => format!("{numeric} <= {}", binds.add(ordered(kind, first))),
        Op::GreaterThan => format!("{numeric} > {}", binds.add(ordered(kind, first))),
        Op::GreaterThanOrEqual => format!("{numeric} >= {}", binds.add(ordered(kind, first))),
        Op::Match => format!("LIKE({column}, {}, true)", binds.add(scalar(first))),
        Op::NotMatch => format!("!LIKE({column}, {}, true)", binds.add(scalar(first))),
        Op::NotEqual | Op::NotIn => format!("{column} NOT IN {}", binds.add(many())),
        Op::Unspecified | Op::Equal | Op::In => format!("{column} IN {}", binds.add(many())),
    }
}

fn composite(operator: LogicalOperator, parts: Vec<String>) -> Result<String, Status> {
    let joiner = match operator {
        LogicalOperator::And => " AND ",
        LogicalOperator::Or => " OR ",
        LogicalOperator::Unspecified => return Err(Status::invalid_argument("invalid logical operator")),
    };
    if parts.is_empty() {
        return Ok(ALWAYS.to_string());
    }
    let wrapped: Vec<String> = parts.iter().map(|part| format!("({part})")).collect();
    Ok(wrapped.join(joiner))
}

fn labels(var: &str, operator: LabelOperator, wanted: &[i32], binds: &mut Binds) -> String {
    let list = binds.add(json!(wanted));
    match operator {
        LabelOperator::HasAll => format!("{list} ALL IN {var}.labels"),
        LabelOperator::HasNone => format!("{list} NONE IN {var}.labels"),
        LabelOperator::HasAny | LabelOperator::Unspecified => format!("{list} ANY IN {var}.labels"),
    }
}

fn texts(values: &[ScalarValue]) -> Value {
    Value::Array(values.iter().map(big_text).collect())
}

fn edge(var: &str, filter: &EdgeFilter, binds: &mut Binds) -> Result<String, Status> {
    if filter.values.is_empty() {
        return Ok(ALWAYS.to_string());
    }
    let kind = binds.add(edge_type(&filter.edge_type)?);
    let ends = binds.add(texts(&filter.values));
    Ok(format!(
        "LENGTH(FOR x IN entity_edges FILTER x._from == {var}._id AND x.type == {kind} AND x.endId IN {ends} LIMIT 1 RETURN 1) > 0"
    ))
}

fn bits(var: &str, filter: &BitFilter, binds: &mut Binds) -> Result<String, Status> {
    let column = match normalized(&filter.property).as_str() {
        "entityflags" => "entityFlagsLow",
        "gameflags" => "gameFlagsLow",
        _ => return Err(Status::invalid_argument("unknown bit property")),
    };
    let mask = binds.add(filter.value as u32);
    let masked = format!("BIT_AND({var}.{column}, {mask})");
    Ok(match filter.operator() {
        BitOperator::Not => format!("{masked} == 0"),
        BitOperator::Or => format!("{masked} != 0"),
        BitOperator::And | BitOperator::Unspecified => format!("{masked} == {mask}"),
    })
}

pub fn entity_class(filter: &EntityClassFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    use entity_class_filter::FilterType;
    match &filter.filter_type {
        None => Ok(ALWAYS.to_string()),
        Some(FilterType::CompositeFilter(group)) => {
            let parts = group.filters.iter().map(|part| entity_class(part, var, binds));
            composite(group.operator(), parts.collect::<Result<_, _>>()?)
        }
        Some(FilterType::PropertyFilter(p)) => {
            let column = known(class_column(&p.property), &p.property)?;
            Ok(property(var, column, p.operator(), &p.values, binds))
        }
        Some(FilterType::NameFilter(p)) => {
            let column = known(class_name_column(&p.language), &p.language)?;
            Ok(property(var, column, p.operator(), &p.values, binds))
        }
    }
}

pub fn class_membership(filter: &EntityClassFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    let classes = entity_class(filter, "c", binds)?;
    Ok(format!("{var}.classGuidCrc IN (FOR c IN entity_classes FILTER {classes} RETURN c.guidHashCrc)"))
}

pub fn entity(filter: &EntityFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    use entity_filter::FilterType;
    match &filter.filter_type {
        None => Ok(ALWAYS.to_string()),
        Some(FilterType::CompositeFilter(group)) => {
            let parts = group.filters.iter().map(|part| entity(part, var, binds));
            composite(group.operator(), parts.collect::<Result<_, _>>()?)
        }
        Some(FilterType::LabelFilter(f)) => Ok(labels(var, f.operator(), &f.labels, binds)),
        Some(FilterType::PropertyFilter(p)) => {
            let column = known(entity_column(&p.property), &p.property)?;
            Ok(property(var, column, p.operator(), &p.values, binds))
        }
        Some(FilterType::EdgeFilter(f)) => edge(var, f, binds),
        Some(FilterType::BitFilter(f)) => bits(var, f, binds),
        Some(FilterType::EntityClassesFilter(f)) => class_membership(f, var, binds),
    }
}

pub fn inventory(filter: &InventoryFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    use inventory_filter::FilterType;
    match &filter.filter_type {
        None => Ok(ALWAYS.to_string()),
        Some(FilterType::CompositeFilter(group)) => {
            let parts = group.filters.iter().map(|part| inventory(part, var, binds));
            composite(group.operator(), parts.collect::<Result<_, _>>()?)
        }
        Some(FilterType::PropertyFilter(p)) => {
            let column = known(inventory_column(&p.property), &p.property)?;
            Ok(property(var, column, p.operator(), &p.values, binds))
        }
        Some(FilterType::EdgeFilter(f)) => edge(var, f, binds),
    }
}

pub fn container(filter: &ContainerFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    use container_filter::FilterType;
    match &filter.filter_type {
        None => Ok(ALWAYS.to_string()),
        Some(FilterType::CompositeFilter(group)) => {
            let parts = group.filters.iter().map(|part| container(part, var, binds));
            composite(group.operator(), parts.collect::<Result<_, _>>()?)
        }
        Some(FilterType::LabelFilter(f)) => Ok(labels(var, f.operator(), &f.labels, binds)),
        Some(FilterType::PropertyFilter(p)) => {
            let column = known(container_column(&p.property), &p.property)?;
            Ok(property(var, column, p.operator(), &p.values, binds))
        }
        Some(FilterType::EdgeFilter(f)) => {
            if f.values.is_empty() {
                return Ok(ALWAYS.to_string());
            }
            let ends = binds.add(texts(&f.values));
            Ok(format!(
                "LENGTH(FOR x IN container_edges FILTER x._from == {var}._id AND x.endId IN {ends} LIMIT 1 RETURN 1) > 0"
            ))
        }
    }
}

fn prune_edge_property(filter: &PruneEdgePropertyFilter, var: &str, binds: &mut Binds) -> Result<String, Status> {
    let column = known(edge_column(&filter.property), &filter.property)?;
    let matches = property(var, column, filter.operator(), &filter.values, binds);
    if filter.edge_type.is_empty() {
        return Ok(matches);
    }
    let kind = binds.add(edge_type(&filter.edge_type)?);
    Ok(format!("{var}.type == {kind} AND ({matches})"))
}

/// None when the filter is empty, so that it cuts nothing.
pub fn prune_edge(filter: &PruneEdgeFilter, var: &str, binds: &mut Binds) -> Result<Option<String>, Status> {
    use prune_edge_filter::FilterType;
    Ok(match &filter.filter_type {
        None => None,
        Some(FilterType::PropertyFilter(p)) => Some(prune_edge_property(p, var, binds)?),
        Some(FilterType::CompositeFilter(group)) => {
            let parts = group.filters.iter().map(|part| prune_edge_property(part, var, binds));
            Some(composite(group.operator(), parts.collect::<Result<_, _>>()?)?)
        }
    })
}

pub fn precondition(condition: &EntityPreCondition, var: &str, binds: &mut Binds) -> Result<String, Status> {
    let column = known(entity_column(&condition.field), &condition.field)?;
    let operator = match condition.operator() {
        EntityPreconditionOperator::GreaterThan => ComparisonOperator::GreaterThan,
        EntityPreconditionOperator::LessThan => ComparisonOperator::LessThan,
        EntityPreconditionOperator::Equals | EntityPreconditionOperator::Unspecified => ComparisonOperator::Equal,
    };
    let value = condition.value.clone().unwrap_or_default();
    Ok(property(var, column, operator, std::slice::from_ref(&value), binds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::entitygraph::v1::{EntityCompositeFilter, NodeLabelFilter, PropertyFilter};

    fn geid_filter(property: &str, geid: u64) -> EntityFilter {
        let value = ScalarValue { scalar_type: Some(scalar_value::ScalarType::UnsignedBigintValue(geid)) };
        EntityFilter {
            filter_type: Some(entity_filter::FilterType::PropertyFilter(PropertyFilter {
                property: property.into(),
                operator: ComparisonOperator::Equal as i32,
                values: vec![value],
            })),
        }
    }

    #[test]
    fn the_clients_geid_filter_compares_exact_text() {
        let mut binds = Binds::default();
        let expression = entity(&geid_filter("geid", u64::MAX), "e", &mut binds).unwrap();
        assert_eq!(expression, "e.geid IN @p0");
        assert_eq!(binds.into_value()["p0"], json!(["18446744073709551615"]));
    }

    #[test]
    fn a_property_outside_the_table_is_refused_and_never_reaches_the_query() {
        let mut binds = Binds::default();
        let attempt = entity(&geid_filter("geid == 1 OR true", 1), "e", &mut binds);
        assert_eq!(attempt.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn names_are_accepted_in_snake_and_camel_case() {
        assert_eq!(entity_column("owner_id"), entity_column("ownerId"));
        assert!(entity_column("position.x").is_some());
    }

    #[test]
    fn composites_nest_and_need_an_operator() {
        let label = EntityFilter {
            filter_type: Some(entity_filter::FilterType::LabelFilter(NodeLabelFilter {
                operator: LabelOperator::HasNone as i32,
                labels: vec![7],
            })),
        };
        let group = |operator: LogicalOperator| EntityFilter {
            filter_type: Some(entity_filter::FilterType::CompositeFilter(EntityCompositeFilter {
                operator: operator as i32,
                filters: vec![geid_filter("ownerId", 5), label.clone()],
            })),
        };
        let mut binds = Binds::default();
        let expression = entity(&group(LogicalOperator::Or), "v", &mut binds).unwrap();
        assert_eq!(expression, "(v.ownerId IN @p0) OR (@p1 NONE IN v.labels)");
        assert!(entity(&group(LogicalOperator::Unspecified), "v", &mut binds).is_err());
    }

    #[test]
    fn edge_types_match_any_spelling() {
        assert_eq!(edge_type("ATTACHED_TO").unwrap(), EntityEdgeType::AttachedTo as i32);
        assert_eq!(edge_type("stowedIn").unwrap(), EntityEdgeType::StowedIn as i32);
        assert_eq!(edge_type("ENTITY_EDGE_TYPE_REFERENCES").unwrap(), EntityEdgeType::References as i32);
    }

    #[test]
    fn an_empty_value_list_matches_everything_like_the_client() {
        let filter = PropertyFilter { property: "geid".into(), ..Default::default() };
        let wrapped = EntityFilter { filter_type: Some(entity_filter::FilterType::PropertyFilter(filter)) };
        assert_eq!(entity(&wrapped, "e", &mut Binds::default()).unwrap(), ALWAYS);
    }
}
