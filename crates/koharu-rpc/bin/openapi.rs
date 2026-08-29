//! Exports the headless API's OpenAPI document as pretty JSON on stdout.
//!
//! Run with `cargo run -p koharu-rpc --bin openapi`.

use utoipa::OpenApi as _;

fn main() {
    let spec = koharu_rpc::ApiDoc::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document is serializable");
    println!("{spec}");
}
