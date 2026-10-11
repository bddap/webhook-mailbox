mod endpoints;
mod service;

#[rocket::launch]
fn rocket() -> rocket::Rocket<rocket::Build> {
    endpoints::rocket_build(service::Db::default())
}
