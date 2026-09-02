//! The `usage_component` table repository (`aub-lqe.8`).
//!
//! Stores token components as child rows rather than permanently fixed columns,
//! allowing unknown token classes to survive normalization without schema changes
//! (PLAN.md 12.9).

use rusqlite::params;

use crate::error::Error;
use crate::store::usage_event::EventId;

/// A component row's identity, by SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId(i64);

impl ComponentId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A new usage component to be inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageComponent<'a> {
    pub event_id: EventId,
    pub token_class: &'a str,
    pub count: u64,
}

/// A retrieved usage component row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageComponentRow {
    pub id: ComponentId,
    pub event_id: EventId,
    pub token_class: String,
    pub count: u64,
}

/// Inserts one usage component child row for an event.
pub fn insert_component(
    conn: &rusqlite::Connection,
    component: &NewUsageComponent<'_>,
) -> Result<ComponentId, Error> {
    conn.query_row(
        "INSERT INTO usage_component (
            event_id,
            token_class,
            count
        ) VALUES (?1, ?2, ?3)
        RETURNING id",
        params![
            component.event_id.value(),
            component.token_class,
            component.count as i64,
        ],
        |row| row.get(0),
    )
    .map(ComponentId::new)
    .map_err(|e| Error::Store(format!("cannot insert usage component: {e}")))
}

/// Inserts multiple usage components for an event in order.
pub fn insert_components(
    conn: &rusqlite::Connection,
    event_id: EventId,
    components: &[(&str, u64)],
) -> Result<(), Error> {
    for &(token_class, count) in components {
        insert_component(
            conn,
            &NewUsageComponent {
                event_id,
                token_class,
                count,
            },
        )?;
    }
    Ok(())
}

/// Retrieves all components for a given canonical event ID.
pub fn get_components_for_event(
    conn: &rusqlite::Connection,
    event_id: EventId,
) -> Result<Vec<UsageComponentRow>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, event_id, token_class, count
            FROM usage_component
            WHERE event_id = ?1
            ORDER BY id ASC",
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot prepare get_components_for_event query: {e}"
            ))
        })?;

    let rows = stmt
        .query_map(params![event_id.value()], |row| {
            let id_val: i64 = row.get(0)?;
            let event_id_val: i64 = row.get(1)?;
            let token_class: String = row.get(2)?;
            let count_val: i64 = row.get(3)?;
            Ok(UsageComponentRow {
                id: ComponentId::new(id_val),
                event_id: EventId::new(event_id_val),
                token_class,
                count: count_val as u64,
            })
        })
        .map_err(|e| {
            Error::Store(format!(
                "cannot execute get_components_for_event query: {e}"
            ))
        })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| Error::Store(format!("cannot read component row: {e}")))?)
    }
    Ok(result)
}
