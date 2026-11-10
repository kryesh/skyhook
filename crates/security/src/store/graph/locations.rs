//! Where in the target a record sits: a code region, an address range of a binary image, or an
//! operation of a named interface. Each is stored once per kind and key, in its kind's detail
//! table sharing the `location` row's id, as a node's kinds share its id.

use std::{fmt, str::FromStr};

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use skyhook::db::{Db, Sql as _, corrupt, params};
use skyhook_base::enums::Kinds;

use crate::store::*;

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, Kinds,
)]
#[serde(rename_all = "snake_case")]
#[kinds(name = LocationKind, derive(PartialOrd, Ord, Hash, JsonSchema))]
pub(crate) enum Location {
    Code(CodeSite),
    Binary(BinarySite),
    Interface(InterfaceSite),
}

/// Inclusive one-based lines; name the `function` they lie in
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "CodeInput")]
#[schemars(inline)]
pub(crate) struct CodeSite {
    pub(crate) file: Text,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) function: Option<Text>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CodeInput {
    file: Text,
    #[schemars(range(min = 1))]
    start_line: u32,
    #[schemars(range(min = 1))]
    end_line: u32,
    function: Option<Text>,
}
impl TryFrom<CodeInput> for CodeSite {
    type Error = Error;
    fn try_from(value: CodeInput) -> Result<Self> {
        if value.start_line == 0 || value.start_line > value.end_line {
            return Err(Error::Span);
        }
        Ok(Self {
            file: value.file,
            start_line: value.start_line,
            end_line: value.end_line,
            function: value.function,
        })
    }
}

/// Inclusive addresses at the link base; name the `symbol` when known
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "BinaryInput")]
#[schemars(inline)]
pub(crate) struct BinarySite {
    pub(crate) image: Text,
    pub(crate) start: Address,
    pub(crate) end: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symbol: Option<Text>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BinaryInput {
    image: Text,
    start: Address,
    end: Address,
    symbol: Option<Text>,
}
impl TryFrom<BinaryInput> for BinarySite {
    type Error = Error;
    fn try_from(value: BinaryInput) -> Result<Self> {
        if value.start > value.end {
            return Err(Error::Span);
        }
        Ok(Self {
            image: value.image,
            start: value.start,
            end: value.end,
            symbol: value.symbol,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an address is 0x followed by 1 to 16 hex digits")]
pub(crate) struct InvalidAddress;

/// A virtual address, written as hex so that no JSON number rounds it. It is stored by its bit
/// pattern, so SQL never orders addresses.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, SerializeDisplay, DeserializeFromStr,
)]
pub(crate) struct Address(pub(crate) u64);

impl FromStr for Address {
    type Err = InvalidAddress;
    fn from_str(value: &str) -> std::result::Result<Self, InvalidAddress> {
        let digits = (value.strip_prefix("0x"))
            .filter(|digits| {
                (1..=16).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or(InvalidAddress)?;
        u64::from_str_radix(digits, 16)
            .map(Self)
            .map_err(|_| InvalidAddress)
    }
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}
impl JsonSchema for Address {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Address".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^0x[0-9A-Fa-f]{1,16}$"})
    }
}

/// `operation` in the interface's terms (a route template, a method, a message type); `field`
/// when narrower
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub(crate) struct InterfaceSite {
    pub(crate) interface: InterfaceName,
    pub(crate) operation: Text,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) field: Option<Text>,
}

impl Location {
    /// The statement finding its id by its kind's key, the one inserting its detail row with
    /// the id last, and the key.
    fn keyed(&self) -> (&'static str, &'static str, Vec<libsql::Value>) {
        match self {
            Self::Code(site) => (
                "SELECT location FROM code_location WHERE file=?1 AND start_line=?2 AND \
                 end_line=?3 AND function IS ?4",
                "INSERT INTO code_location(file,start_line,end_line,function,location) VALUES \
                 (?1,?2,?3,?4,?5)",
                params![
                    &site.file,
                    site.start_line,
                    site.end_line,
                    site.function.as_ref()
                ],
            ),
            Self::Binary(site) => (
                "SELECT location FROM binary_location WHERE image=?1 AND start_address=?2 AND \
                 end_address=?3 AND symbol IS ?4",
                "INSERT INTO binary_location(image,start_address,end_address,symbol,location) \
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    &site.image,
                    site.start.0 as i64,
                    site.end.0 as i64,
                    site.symbol.as_ref()
                ],
            ),
            Self::Interface(site) => (
                "SELECT location FROM interface_location WHERE interface=?1 AND operation=?2 AND \
                 field IS ?3",
                "INSERT INTO interface_location(interface,operation,field,location) VALUES \
                 (?1,?2,?3,?4)",
                params![
                    site.interface.as_str(),
                    &site.operation,
                    site.field.as_ref()
                ],
            ),
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let within = match self {
            Self::Code(site) => {
                write!(f, "{}:{}-{}", site.file, site.start_line, site.end_line)?;
                site.function.as_ref().map(|function| (" in ", function))
            }
            Self::Binary(site) => {
                write!(f, "{}@{}-{}", site.image, site.start, site.end)?;
                site.symbol.as_ref().map(|symbol| (" in ", symbol))
            }
            Self::Interface(site) => {
                write!(f, "{} {}", site.interface, site.operation)?;
                if let Some(field) = &site.field {
                    write!(f, " [{field}]")?;
                }
                None
            }
        };
        match within {
            Some((lead, name)) => write!(f, "{lead}{name}"),
            None => Ok(()),
        }
    }
}

/// A component owns interface `name`.
pub(in crate::store) fn known_interface(db: &Db, name: &InterfaceName) -> Result<()> {
    let known = "SELECT 1 FROM component_interface WHERE interface=?1";
    match exists(db, known, params![name.as_str()])? {
        true => Ok(()),
        false => Err(Error::UnknownInterface(name.clone())),
    }
}

/// The id of `location`, recorded on first use. An interface site names a recorded interface.
pub(in crate::store) fn location_id(db: &Db, location: &Location) -> Result<LocationId> {
    let (find, insert, key) = location.keyed();
    if let Some(id) = optional(db, find, key.clone(), |row| Ok(LocationId(row.get(0)?)))? {
        return Ok(id);
    }
    if let Location::Interface(site) = location {
        known_interface(db, &site.interface)?;
    }
    let id = LocationId(db.insert(
        "INSERT INTO location(kind) VALUES (?1)",
        params![location.kind()],
    )?);
    let mut values = key;
    values.push(id.sql());
    db.execute(insert, values)?;
    Ok(id)
}

/// The detail columns of the location a statement joins with [`location_joins`], decoded by
/// [`location_column`].
macro_rules! location_columns {
    () => {
        "lc.file,lc.start_line,lc.end_line,lc.function,lb.image,lb.start_address,lb.end_address,\
         lb.symbol,li.interface,li.operation,li.field"
    };
}
pub(in crate::store) use location_columns;

/// The detail rows of the location whose id is `$at`, joined for [`location_columns`].
macro_rules! location_joins {
    ($at:literal) => {
        concat!(
            " LEFT JOIN code_location lc ON lc.location=",
            $at,
            " LEFT JOIN binary_location lb ON lb.location=",
            $at,
            " LEFT JOIN interface_location li ON li.location=",
            $at
        )
    };
}
pub(in crate::store) use location_joins;

/// The location in the [`location_columns`] from `at`, if the joins found one.
pub(in crate::store) fn location_column(row: &Row, at: i32) -> Result<Option<Location>> {
    let text = |offset: i32| -> Result<Option<Text>> {
        Ok((row.get::<Option<String>>(at + offset)?)
            .map(|value| value.parse())
            .transpose()?)
    };
    let address =
        |offset: i32| -> Result<Address> { Ok(Address(row.get::<i64>(at + offset)? as u64)) };
    Ok(if let Some(file) = text(0)? {
        Some(Location::Code(CodeSite {
            file,
            start_line: row.get(at + 1)?,
            end_line: row.get(at + 2)?,
            function: text(3)?,
        }))
    } else if let Some(image) = text(4)? {
        Some(Location::Binary(BinarySite {
            image,
            start: address(5)?,
            end: address(6)?,
            symbol: text(7)?,
        }))
    } else if let Some(interface) = row.get::<Option<String>>(at + 8)? {
        Some(Location::Interface(InterfaceSite {
            interface: interface.parse()?,
            operation: row.get::<String>(at + 9)?.parse()?,
            field: text(10)?,
        }))
    } else {
        None
    })
}

/// The location a holder requires, in the [`location_columns`] from `at`.
pub(in crate::store) fn located(row: &Row, at: i32) -> Result<Location> {
    location_column(row, at)?.ok_or_else(|| corrupt("a required location is missing").into())
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use serde_json::json;

    use super::*;
    use crate::store::tests::{component, fixture, text};

    /// Each kind parses from its tagged shape, is stored once per key and reads back as given;
    /// a reversed span, a malformed address and an interface no component owns are refused.
    #[test]
    fn locations_round_trip_by_kind() {
        let (_session, store, actor) = fixture();
        let component = component(&store, &actor, "public");
        let parsed = |value| serde_json::from_value::<Location>(value);
        let sites = [
            json!({"code": {"file": "public/lib.c", "start_line": 3, "end_line": 9, "function": "serve"}}),
            json!({"binary": {"image": "bin/d", "start": "0xffffffffffff1000", "end": "0xFFFFFFFFFFFF10f8"}}),
            json!({"interface": {"interface": "public-api", "operation": "POST /v1/files/{id}", "field": "body.name"}}),
        ];
        let shown = [
            "public/lib.c:3-9 in serve",
            "bin/d@0xffffffffffff1000-0xffffffffffff10f8",
            "public-api POST /v1/files/{id} [body.name]",
        ];
        for (site, shown) in sites.into_iter().zip(shown) {
            let location = parsed(site.clone()).unwrap();
            assert_eq!(location.to_string(), shown);
            let flaw = |location: &Location| {
                let new = NewFlaw {
                    title: text("flaw"),
                    body: text("here"),
                    component,
                    location: location.clone(),
                    cwe: None,
                };
                let id = FlawId::of(
                    store
                        .upsert(&actor, &Upsert::Create(NewNode::Flaw(new)))
                        .unwrap(),
                );
                let NodeView::Flaw(view) = store.node(id.into()).unwrap() else {
                    panic!("a flaw")
                };
                view.location
            };
            assert_eq!(flaw(&location), location);
            assert_eq!(flaw(&location), location);
            let lowered = serde_json::to_value(&location)
                .unwrap()
                .to_string()
                .to_lowercase();
            assert_eq!(lowered, site.to_string().to_lowercase());
        }
        let stored = store
            .read(|db| count(db, "SELECT count(*) FROM location", Vec::new()))
            .unwrap();
        assert_eq!(stored, 3, "a location is recorded once per key");

        for refused in [
            json!({"code": {"file": "a.c", "start_line": 9, "end_line": 3}}),
            json!({"binary": {"image": "bin/d", "start": "0x20", "end": "0x10"}}),
            json!({"binary": {"image": "bin/d", "start": "4096", "end": "0x10"}}),
            json!({"binary": {"image": "bin/d", "start": "0x+1", "end": "0x10"}}),
            json!({"binary": {"image": "bin/d", "start": "0x10000000000000000", "end": "0x1"}}),
            json!({"interface": {"interface": "public api", "operation": "GET /"}}),
        ] {
            assert!(parsed(refused.clone()).is_err(), "{refused}");
        }
        let unknown = Location::Interface(InterfaceSite {
            interface: "admin-api".parse().unwrap(),
            operation: text("GET /"),
            field: None,
        });
        assert_matches!(
            store.write(|db| location_id(db, &unknown)),
            Err(Error::UnknownInterface(name)) if name.as_str() == "admin-api"
        );
    }
}
