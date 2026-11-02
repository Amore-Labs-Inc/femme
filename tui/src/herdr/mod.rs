//! herdr's socket API (herdr.dev/tui/docs/socket-api): one JSON object per line over a Unix
//! socket, `{"id","method","params"}` in, `{"id","result"}` or `{"id","error":{"code","message"}}`
//! out. Method and field names come from herdr 0.9.5's `src/api/schema`.
pub mod client;
pub mod events;
pub mod machines;
pub mod types;
