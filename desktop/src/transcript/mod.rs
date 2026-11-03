//! The session transcript: the items of a session as the daemon replicates them, drawn as a list
//! that follows the text as it streams in. Markdown for the assistant, folded tool calls, answerable
//! questions, the user's attachments as thumbnails.

pub mod attach;
pub mod body;
pub mod items;
pub mod live;
pub mod markdown;
pub mod md_view;
pub mod question;
pub mod splice;
pub mod thumb;
pub mod turns;

mod ask;
mod faces;
mod rows;

pub use live::Transcript;
