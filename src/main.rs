mod endpoints;
mod service;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    endpoints::rocket_build(service::Db::default())
        .launch()
        .await
}
