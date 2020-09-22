use super::{ContentType, ProjectionsError};
use crate::catalog::extraction::KeyError;
use crate::doc;
use estuary_json::schema;
use itertools::Itertools;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("at {loc}:\n{detail}")]
    At { loc: String, detail: Box<Error> },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("joining '{relative}' with base URL '{base}': {detail}")]
    URLJoinErr {
        base: url::Url,
        relative: String,
        detail: url::ParseError,
    },
    #[error("parsing URL: {0}")]
    URLParseErr(#[from] url::ParseError),
    #[error("HTTP error (reqwest): {0}")]
    ReqwestErr(#[from] reqwest::Error),
    #[error("failed to parse YAML: {0}")]
    YAMLErr(#[from] serde_yaml::Error),
    #[error("failed to parse JSON: {0}")]
    JSONErr(#[from] serde_json::Error),
    #[error("catalog database error: {0}")]
    SQLiteErr(#[from] rusqlite::Error),
    #[error("cannot fetch resource URI: {0}")]
    FetchNotSupported(url::Url),
    #[error(
        "{source_uri:?} references {import_uri:?} without directly or indirectly importing it"
    )]
    MissingImport {
        source_uri: String,
        import_uri: String,
    },
    #[error("{source_uri:?} imports {import_uri:?}, but {import_uri:?} already transitively imports {source_uri:?}")]
    CyclicImport {
        source_uri: String,
        import_uri: String,
    },
    #[error("resource has content-type {next}, but is already registered with type {prev}")]
    ContentTypeMismatch {
        next: ContentType,
        prev: ContentType,
    },
    #[error("failed to build schema: {0}")]
    SchemaBuildErr(#[from] schema::build::Error),
    #[error("schema index: {0}")]
    SchemaIndexErr(#[from] schema::index::Error),
    #[error("subprocess {process:?} failed with status {status}")]
    SubprocessFailed {
        process: std::path::PathBuf,
        status: std::process::ExitStatus,
    },
    #[error("Invalid collection keys: \n{}", .0.iter().join("\n"))]
    InvalidCollectionKeys(Vec<KeyError>),
    #[error("schema validation error: {}", serde_json::to_string_pretty(.0).unwrap())]
    FailedValidation(doc::FailedValidation),
    #[error("Invalid projections: {0}")]
    MaterializationError(#[from] ProjectionsError),

    #[error(transparent)]
    InvalidProjection(#[from] crate::catalog::projections::NoSuchLocationError),

    #[error(
        "Materialization references the collection: '{collection_name}', which does not exist"
    )]
    MaterializationCollectionMissing { collection_name: String },
}
