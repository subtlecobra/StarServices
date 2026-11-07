//! The VQL subset this service understands. CIG's grammar is not in the client, so this one is invented:
//!
//!   [SELECT * | SELECT field, ...] FROM domain [WHERE field op value [AND field op value ...]]
//!
//! `op` is `=`, `!=` or `LIKE`, and a value is a bare word or 'quoted'. Domains and fields come from a fixed
//! list and every value becomes a parameter, so text from a query never reaches the SQL.

pub struct Domain {
    pub name: &'static str,
    pub table: &'static str,
    pub description: &'static str,
    pub fields: &'static [&'static str],
}

pub const DOMAINS: &[Domain] = &[
    Domain {
        name: "tags",
        table: "vql_tags",
        description: "The tag hierarchy",
        fields: &["guid", "name", "parent_guid", "full_path"],
    },
    Domain {
        name: "records",
        table: "vql_records",
        description: "Data records",
        fields: &["guid", "record_type", "file_path"],
    },
    Domain {
        name: "locations",
        table: "vql_locations",
        description: "Location definitions",
        fields: &["location_def_guid", "oc_asset_key", "display_name"],
    },
    Domain {
        name: "violations",
        table: "vql_violations",
        description: "Validation findings",
        fields: &["domain", "source_file", "element_id", "rule", "severity", "description"],
    },
];

#[derive(Debug, PartialEq)]
pub struct VqlError {
    pub offset: usize,
    pub message: String,
}

#[derive(Debug, PartialEq)]
pub struct Token {
    pub text: String,
    pub offset: usize,
    pub quoted: bool,
}

pub struct Parsed {
    pub domain: &'static Domain,
    pub fields: Vec<&'static str>,
    /// `WHERE` body, with `$n` for each entry of `values`.
    pub condition: String,
    pub values: Vec<String>,
}

impl Parsed {
    /// The statement, leaving its LIMIT and OFFSET to the two parameters after the values.
    pub fn sql(&self) -> String {
        let next = self.values.len() + 1;
        format!("{} ORDER BY 1 LIMIT ${next} OFFSET ${}", self.select(&self.columns()), next + 1)
    }

    pub fn count_sql(&self) -> String {
        self.select("count(*) AS total")
    }

    fn columns(&self) -> String {
        self.fields.iter().map(|field| format!("{field}::text AS {field}")).collect::<Vec<_>>().join(", ")
    }

    fn select(&self, columns: &str) -> String {
        let table = self.domain.table;
        if self.condition.is_empty() {
            format!("SELECT {columns} FROM {table}")
        } else {
            format!("SELECT {columns} FROM {table} WHERE {}", self.condition)
        }
    }
}

pub fn tokenize(text: &str) -> Result<Vec<Token>, VqlError> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(offset, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '\'' {
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next() {
                    Some((_, '\'')) => break,
                    Some((_, c)) => value.push(c),
                    None => return Err(VqlError { offset, message: "unterminated string".into() }),
                }
            }
            tokens.push(Token { text: value, offset, quoted: true });
        } else if c == ',' || c == '*' || c == '=' {
            chars.next();
            tokens.push(Token { text: c.to_string(), offset, quoted: false });
        } else if c == '!' {
            chars.next();
            if chars.next_if(|(_, c)| *c == '=').is_none() {
                return Err(VqlError { offset, message: "expected !=".into() });
            }
            tokens.push(Token { text: "!=".into(), offset, quoted: false });
        } else {
            let mut word = String::new();
            while let Some((_, c)) = chars.next_if(|(_, c)| !c.is_whitespace() && !",*=!'".contains(*c)) {
                word.push(c);
            }
            tokens.push(Token { text: word, offset, quoted: false });
        }
    }
    Ok(tokens)
}

fn is_word(token: Option<&Token>, word: &str) -> bool {
    token.is_some_and(|token| !token.quoted && token.text.eq_ignore_ascii_case(word))
}

fn field_of(domain: &Domain, token: &Token) -> Result<&'static str, VqlError> {
    domain.fields.iter().find(|field| field.eq_ignore_ascii_case(&token.text)).copied().ok_or_else(|| VqlError {
        offset: token.offset,
        message: format!("unknown field {} in {}", token.text, domain.name),
    })
}

fn end_of(tokens: &[Token], text_len: usize) -> usize {
    tokens.last().map_or(text_len, |token| token.offset + token.text.len())
}

pub fn parse(text: &str) -> Result<Parsed, VqlError> {
    let tokens = tokenize(text)?;
    let end = end_of(&tokens, text.len());
    let missing = |what: &str| VqlError { offset: end, message: format!("expected {what}") };
    let mut at = 0;
    let mut selected: Vec<&Token> = Vec::new();
    if is_word(tokens.first(), "select") {
        at = 1;
        while at < tokens.len() && !is_word(tokens.get(at), "from") {
            if tokens[at].text != "," && tokens[at].text != "*" {
                selected.push(&tokens[at]);
            }
            at += 1;
        }
    }
    if !is_word(tokens.get(at), "from") {
        return Err(missing("FROM"));
    }
    let name = tokens.get(at + 1).ok_or_else(|| missing("a domain"))?;
    let domain = DOMAINS
        .iter()
        .find(|domain| domain.name.eq_ignore_ascii_case(&name.text))
        .ok_or_else(|| VqlError { offset: name.offset, message: format!("unknown domain {}", name.text) })?;
    let fields = if selected.is_empty() {
        domain.fields.to_vec()
    } else {
        selected.iter().map(|token| field_of(domain, token)).collect::<Result<_, _>>()?
    };
    at += 2;
    let mut conditions = Vec::new();
    let mut values = Vec::new();
    if at < tokens.len() {
        if !is_word(tokens.get(at), "where") {
            return Err(VqlError { offset: tokens[at].offset, message: "expected WHERE".into() });
        }
        at += 1;
        loop {
            let field = field_of(domain, tokens.get(at).ok_or_else(|| missing("a field"))?)?;
            let op = tokens.get(at + 1).ok_or_else(|| missing("an operator"))?;
            let sql_op = match op.text.to_ascii_uppercase().as_str() {
                "=" => "=",
                "!=" => "<>",
                "LIKE" => "LIKE",
                _ => return Err(VqlError { offset: op.offset, message: format!("unknown operator {}", op.text) }),
            };
            let value = tokens.get(at + 2).ok_or_else(|| missing("a value"))?;
            values.push(value.text.clone());
            conditions.push(format!("{field}::text {sql_op} ${}", values.len()));
            at += 3;
            if at == tokens.len() {
                break;
            }
            if !is_word(tokens.get(at), "and") {
                return Err(VqlError { offset: tokens[at].offset, message: "expected AND".into() });
            }
            at += 1;
        }
    }
    Ok(Parsed { domain, fields, condition: conditions.join(" AND "), values })
}

/// What can come next at the end of `prefix`, as a context name, the domain so far and the expected words.
pub fn complete(prefix: &str) -> (&'static str, String, Vec<String>) {
    let tokens = tokenize(prefix).unwrap_or_default();
    let partial = !prefix.is_empty() && !prefix.ends_with(char::is_whitespace);
    let done = &tokens[..tokens.len() - usize::from(partial && !tokens.is_empty())];
    let from = done.iter().position(|token| is_word(Some(token), "from"));
    let Some(from) = from else {
        return ("statement", String::new(), vec!["SELECT".into(), "FROM".into()]);
    };
    let Some(name) = done.get(from + 1) else {
        return ("domain", String::new(), DOMAINS.iter().map(|domain| domain.name.to_string()).collect());
    };
    let domain = DOMAINS.iter().find(|domain| domain.name.eq_ignore_ascii_case(&name.text));
    let current = domain.map_or_else(String::new, |domain| domain.name.to_string());
    let fields = || domain.map_or_else(Vec::new, |domain| domain.fields.iter().map(|f| f.to_string()).collect());
    let Some(clause) = done.iter().position(|token| is_word(Some(token), "where")) else {
        return ("clause", current, vec!["WHERE".into()]);
    };
    match (done.len() - clause - 1) % 4 {
        0 => ("field", current, fields()),
        1 => ("operator", current, vec!["=".into(), "!=".into(), "LIKE".into()]),
        2 => ("value", current, Vec::new()),
        _ => ("clause", current, vec!["AND".into()]),
    }
}

/// 1-based line and column of a byte offset.
pub fn position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() as u32 + 1;
    let column = before.rsplit('\n').next().map_or(0, |last| last.chars().count()) as u32 + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_query_selects_every_field() {
        let parsed = parse("from tags").unwrap();
        assert_eq!(parsed.domain.table, "vql_tags");
        assert_eq!(parsed.fields.len(), 4);
        assert_eq!(parsed.count_sql(), "SELECT count(*) AS total FROM vql_tags");
    }

    #[test]
    fn conditions_become_parameters() {
        let parsed = parse("SELECT name, guid FROM tags WHERE name LIKE 'Ship%' AND guid != x").unwrap();
        assert_eq!(parsed.fields, ["name", "guid"]);
        assert_eq!(parsed.values, ["Ship%", "x"]);
        assert_eq!(
            parsed.sql(),
            "SELECT name::text AS name, guid::text AS guid FROM vql_tags \
             WHERE name::text LIKE $1 AND guid::text <> $2 ORDER BY 1 LIMIT $3 OFFSET $4"
        );
    }

    #[test]
    fn injection_stays_a_value() {
        let parsed = parse("FROM tags WHERE name = 'x; DROP TABLE vql_tags'").unwrap();
        assert!(!parsed.sql().contains("DROP"));
        assert_eq!(parsed.values, ["x; DROP TABLE vql_tags"]);
    }

    #[test]
    fn errors_point_at_the_problem() {
        assert_eq!(parse("FROM nothing").err().unwrap().offset, 5);
        assert_eq!(parse("FROM tags WHERE bad = 1").err().unwrap().offset, 16);
        assert!(parse("FROM tags WHERE name = 'open").is_err());
        assert!(parse("tags").is_err());
        assert!(parse("SELECT evil FROM tags").is_err());
    }

    #[test]
    fn completion_follows_the_grammar() {
        assert_eq!(complete("").0, "statement");
        assert_eq!(complete("FROM ").0, "domain");
        assert_eq!(complete("FROM ta").0, "domain");
        let (context, domain, expected) = complete("FROM tags ");
        assert_eq!((context, domain.as_str(), expected), ("clause", "tags", vec!["WHERE".to_string()]));
        assert_eq!(complete("FROM tags WHERE ").0, "field");
        assert_eq!(complete("FROM tags WHERE name ").0, "operator");
        assert_eq!(complete("FROM tags WHERE name = ").0, "value");
        assert_eq!(complete("FROM tags WHERE name = x ").0, "clause");
        assert_eq!(complete("FROM tags WHERE name = x AND ").0, "field");
    }

    #[test]
    fn positions_are_one_based() {
        assert_eq!(position("ab\ncd", 4), (2, 2));
        assert_eq!(position("ab", 0), (1, 1));
    }
}
