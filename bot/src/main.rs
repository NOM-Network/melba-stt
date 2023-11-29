use nn::model::{FilesSpec, ModelBuilder};

#[tokio::main]
async fn main() {
    use tracing_subscriber::prelude::*;

    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug,tokenizers::tokenizer::serialization=error");
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let mb = ModelBuilder::default()
        .cuda_or_cpu(0)
        .finish()
        .await
        .unwrap();
    let mut speakers = vec![];

    loop {
        let speaker = mb.get_new_speaker();
        println!("added");
        speakers.push(speaker);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
