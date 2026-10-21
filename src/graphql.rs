use serde::Deserialize;

use crate::envelope::Forge;
use crate::forge::Failure;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection<T> {
    pub nodes: Vec<T>,
    pub page_info: PageInfo,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub end_cursor: Option<String>,
    pub has_next_page: bool,
}

/// One connection paged to its end across the queries of a poll, its `$<name>After` variable
/// asking for the next page and its `$<name>` one skipping it once done.
pub struct Paged<T> {
    forge: Forge,
    name: &'static str,
    pub nodes: Vec<T>,
    page: Page,
}

enum Page {
    First,
    After(String),
    Done,
}

impl<T> Paged<T> {
    pub fn new(forge: Forge, name: &'static str) -> Self {
        Self {
            forge,
            name,
            nodes: Vec::new(),
            page: Page::First,
        }
    }

    /// Marks the connection finished without fetching it.
    pub fn finish(&mut self) {
        self.page = Page::Done;
    }

    pub fn done(&self) -> bool {
        matches!(self.page, Page::Done)
    }

    /// The query variable that asks for the next page, or that skips a finished connection.
    pub fn variable(&self) -> Option<(&'static str, String)> {
        match &self.page {
            Page::First => None,
            Page::After(cursor) => Some(("-f", format!("{}After={cursor}", self.name))),
            Page::Done => Some(("-F", format!("{}=false", self.name))),
        }
    }

    pub fn take(&mut self, connection: Option<Connection<T>>) -> Result<(), Failure> {
        if self.done() {
            return Ok(());
        }
        let Some(connection) = connection else {
            let message = format!("the {} connection came back null", self.name);
            return Err(Failure::transient(self.forge, &message));
        };
        self.nodes.extend(connection.nodes);
        self.page = match connection.page_info {
            PageInfo {
                has_next_page: false,
                ..
            } => Page::Done,
            PageInfo {
                end_cursor: Some(cursor),
                ..
            } => Page::After(cursor),
            PageInfo {
                end_cursor: None, ..
            } => {
                let message = format!("the {} connection has a next page but no cursor", self.name);
                return Err(Failure::transient(self.forge, &message));
            }
        };
        Ok(())
    }
}

/// Fails a poll whose head moved between two of its pages.
pub fn same_head(forge: Forge, first: &str, page: &str) -> Result<(), Failure> {
    if first == page {
        return Ok(());
    }
    let message = format!("the head moved from {first} to {page} between pages");
    Err(Failure::transient(forge, &message))
}
