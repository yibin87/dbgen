use crate::{
    error::{Error, ErrorKind},
    value::Value,
};
use failure::ResultExt;
use pest::{
    iterators::{Pair, Pairs},
    Parser,
};
use pest_derive::Parser;
use std::fmt;

#[derive(Parser)]
#[grammar = "parser.pest"]
struct TemplateParser;

/// A schema-qualified name with quotation marks still intact.
#[derive(Debug, Clone)]
pub struct QName {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: String,
}

impl QName {
    fn from_pairs(mut pairs: Pairs<'_, Rule>) -> Self {
        let mut qname = QName {
            database: None,
            schema: None,
            table: pairs.next().expect("at least one name").as_str().to_owned(),
        };
        if let Some(pair) = pairs.next() {
            qname.schema = Some(qname.table);
            qname.table = pair.as_str().to_owned();
            if let Some(pair) = pairs.next() {
                qname.database = qname.schema;
                qname.schema = Some(qname.table);
                qname.table = pair.as_str().to_owned();
            }
        }
        qname
    }

    fn estimated_joined_len(&self) -> usize {
        self.database.as_ref().map_or(0, |s| s.len() + 1)
            + self.schema.as_ref().map_or(0, |s| s.len() + 1)
            + self.table.len()
    }

    /// Parses a qualified name
    pub fn parse(input: &str) -> Result<Self, Error> {
        let mut pairs = TemplateParser::parse(Rule::qname, input).context(ErrorKind::ParseTemplate)?;
        Ok(Self::from_pairs(pairs.next().unwrap().into_inner()))
    }

    /// Obtains the qualified name connected with dots (`"db"."schema"."table"`)
    pub fn qualified_name(&self) -> String {
        let mut res = String::with_capacity(self.estimated_joined_len());
        if let Some(db) = &self.database {
            res.push_str(db);
            res.push('.');
        }
        if let Some(schema) = &self.schema {
            res.push_str(schema);
            res.push('.');
        }
        res.push_str(&self.table);
        res
    }

    /// Obtains the unique name.
    ///
    /// This name is transformed from the qualified name with these changes:
    ///  - Unquoted names are all converted to lower case in the default
    ///     collation (`XyzÄbc` → `xyzäbc`). If the lowercasing results in
    ///     multiple characters (e.g. `İ` → `i̇`), only the first will be
    ///     included.
    ///  - Quotation marks are removed (`"Hello ""world"""` → `Hello "world"`)
    ///  - Special characters including `.`, `-` and `/` are percent-encoded,
    ///     so the resulting string can be safely used as a filename.
    pub fn unique_name(&self) -> String {
        let mut res = String::with_capacity(self.estimated_joined_len());

        if let Some(db) = &self.database {
            unescape_into(&mut res, db, true);
            res.push('.');
        }
        if let Some(schema) = &self.schema {
            unescape_into(&mut res, schema, true);
            res.push('.');
        }
        unescape_into(&mut res, &self.table, true);
        res
    }
}

fn unescape_into(res: &mut String, ident: &str, do_percent_escape: bool) {
    use std::fmt::Write;

    let mut chars = ident.chars();
    let escape_char = match chars.next() {
        c @ Some('`') | c @ Some('\'') | c @ Some('"') => c,
        Some('[') => Some(']'),
        _ => {
            chars = ident.chars();
            None
        }
    };
    let mut pass_through_escape_char = false;
    for mut c in chars {
        if pass_through_escape_char {
            pass_through_escape_char = false;
        } else if Some(c) == escape_char {
            pass_through_escape_char = true;
            continue;
        } else if escape_char.is_none() {
            c = c.to_lowercase().next().unwrap();
        }
        match c {
            '.' | '-' | '/' => {
                if do_percent_escape {
                    write!(res, "%{:02X}", c as u32).unwrap();
                    continue;
                }
            }
            _ => {}
        }
        res.push(c);
    }
}

#[derive(Debug, Clone)]
pub struct Template {
    /// The default table name.
    pub name: QName,

    /// The content of the CREATE TABLE statement.
    pub content: String,

    /// The expressions to populate the table.
    pub exprs: Vec<Expr>,
}

#[derive(Debug, Clone)]
pub enum Expr {
    RowNum,
    Value(Value),
    Function { name: Function, args: Vec<Expr> },
}

impl Template {
    pub fn parse(input: &str) -> Result<Self, Error> {
        let mut pairs = TemplateParser::parse(Rule::create_table, input).context(ErrorKind::ParseTemplate)?;

        let name = QName::from_pairs(pairs.next().unwrap().into_inner());
        let content = pairs.next().unwrap().as_str().to_owned();
        let exprs = Expr::from_pairs(pairs)?;

        Ok(Self { name, content, exprs })
    }
}

impl Expr {
    fn from_pairs(pairs: Pairs<'_, Rule>) -> Result<Vec<Self>, Error> {
        pairs.map(Expr::from_pair).collect()
    }

    fn from_pair(pair: Pair<'_, Rule>) -> Result<Self, Error> {
        match pair.as_rule() {
            Rule::expr_rownum => Ok(Expr::RowNum),
            Rule::expr_function => {
                let mut pairs = pair.into_inner();
                let qname = QName::from_pairs(pairs.next().unwrap().into_inner());
                let name = Function::from_name(qname.unique_name())?;
                let args = Expr::from_pairs(pairs)?;
                Ok(Expr::Function { name, args })
            }
            Rule::expr_string => {
                let mut string = String::with_capacity(pair.as_str().len());
                unescape_into(&mut string, pair.as_str(), false);
                Ok(Expr::Value(string.into()))
            }
            Rule::expr_number => parse_number(pair.as_str()).map(Expr::Value),
            Rule::expr_neg => {
                let mut pairs = pair.into_inner();
                let inner = Expr::from_pair(pairs.next().unwrap())?;
                Ok(Expr::Function {
                    name: Function::Neg,
                    args: vec![inner],
                })
            }
            r => panic!("unexpected rule <{:?}> while parsing an expression", r),
        }
    }
}

fn parse_number(input: &str) -> Result<Value, Error> {
    match input.get(..2) {
        Some("0x") | Some("0X") => {
            let number =
                u64::from_str_radix(&input[2..], 16).with_context(|_| ErrorKind::IntegerOverflow(input.to_owned()))?;
            return Ok(number.into());
        }
        _ => {}
    }

    Ok(match input.parse::<u64>() {
        Ok(number) => number.into(),
        Err(_) => input.parse::<f64>().unwrap().into(),
    })
}

macro_rules! define_function {
    (
        pub enum $F:ident {
            $($ident:ident = $s:tt,)+
        }
    ) => {
        #[derive(Debug, Copy, Clone, PartialEq, Eq)]
        pub enum $F {
            $($ident,)+
        }

        impl fmt::Display for $F {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(match self {
                    $($F::$ident => $s,)+
                })
            }
        }

        impl $F {
            fn from_name(name: String) -> Result<Self, Error> {
                Ok(match &*name {
                    $($s => $F::$ident,)+
                    _ => return Err(ErrorKind::UnknownFunction(name).into()),
                })
            }
        }
    }
}

define_function! {
    pub enum Function {
        RandRegex = "rand.regex",
        RandRange = "rand.range",
        RandRangeInclusive = "rand.range_inclusive",
        RandUniform = "rand.uniform",
        RandUniformInclusive = "rand.uniform_inclusive",

        Neg = "-",
    }
}
