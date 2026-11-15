use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Query(String),
    Select(String),
    AddAlias { nickname: String, path: String },
    RecordSelection { query: String, path: String },
    ListAliases,
    RemoveAlias(String),
    ClearAliases,
    ClearSelectionMemory,
    Reindex,
    Compact,
    Stats,
    Doctor,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    QueryResults(Vec<(String, f32)>),
    AliasList(Vec<(String, String)>),
    Status(String),
    Error(String),
}
