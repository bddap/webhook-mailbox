//! ## Issues:
//!
//! The current implementation can be abused as generic data storage. Possible fixes:
//! - Data coming in over hook could just be streamed directly to the watcher
//!   that way we wouldn't need to store it.
//! - Set lifetime for data, delete after certain period.
//! - Delete the oldest data when more space is needed.
//!
//! Post bodies that are too large will be truncated. How do we communicate to the user
//! that the body was too large.
//!
//! Get `/hook/<address>` is ignored. Only post works. Maybe this is what we want?
//!
//! Does the user want to be able to get the http headers that were sent to `/hook/<address>`

mod endpoints;
mod service;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    endpoints::rocket_build(service::Db::default())
        .launch()
        .await
}
