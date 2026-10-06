mod endpoints;
mod service;

#[rocket::launch]
fn rocket() -> _ {
    endpoints::rocket_build(service::Db::default())
}
