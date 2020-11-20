use anyhow::*;
use log::LevelFilter;
use northstar::{Server, Request, Response, GEMINI_PORT, Document};
use northstar::document::HeadingLevel::*;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::builder()
        .filter_module("northstar", LevelFilter::Debug)
        .init();

    Server::bind(("localhost", GEMINI_PORT))
        .add_route("/",handle_request)
        .serve()
        .await
}

async fn handle_request(_request: Request) -> Result<Response> {
    let mut document = Document::new();

    document
        .add_preformatted(include_str!("northstar_logo.txt"))
        .add_blank_line()
        .add_link("https://docs.rs/northstar", "Documentation")
        .add_link("https://github.com/panicbit/northstar", "GitHub")
        .add_blank_line()
        .add_heading(H1, "Usage")
        .add_blank_line()
        .add_text("Add the latest version of northstar to your `Cargo.toml`.")
        .add_blank_line()
        .add_heading(H2, "Manually")
        .add_blank_line()
        .add_preformatted_with_alt("toml", r#"northstar = "0.3.0" # check crates.io for the latest version"#)
        .add_blank_line()
        .add_heading(H2, "Automatically")
        .add_blank_line()
        .add_preformatted_with_alt("sh", "cargo add northstar")
        .add_blank_line()
        .add_heading(H1, "Generating a key & certificate")
        .add_blank_line()
        .add_preformatted_with_alt("sh", concat!(
            "mkdir cert && cd cert\n",
            "openssl req -x509 -nodes -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365",
        ));

    Ok(Response::document(document))
}
