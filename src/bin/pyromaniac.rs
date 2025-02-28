use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use serde_aux::prelude::*;
use sqlx::mysql::MySqlPoolOptions;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(long)]
    directory: Option<PathBuf>,
    #[clap(long)]
    mysql: Option<String>,
}

#[derive(Deserialize, Debug)]
struct SyntheticPrice {
    numerator: String,
    denominator: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Validity {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    lower_bound: u64,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    upper_bound: u64,
}

#[derive(Deserialize, Debug)]
struct OraclePayload {
    synthetic: String,
    synthetic_price: SyntheticPrice,
    validity: Validity,
    payload: String,
}

#[derive(Debug, Serialize)]
struct Payload {
    id: Option<u64>,
    timestamp: DateTime<Utc>,
    source: Option<String>,
    synthetic: Option<String>,
    synthetic_numerator: Option<String>,
    synthetic_denominator: Option<String>,
    validity_lower: Option<u64>,
    validity_upper: Option<u64>,
    payload: Option<Vec<u8>>,
    error_text: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = reqwest::Client::new();

    if args.directory.is_none() && args.mysql.is_none() {
        println!("No directory or MySQL specified");
        return Ok(());
    }
    if let Some(directory) = &args.directory {
        fs::create_dir_all(directory)?;
    }

    loop {
        thread::sleep(std::time::Duration::from_millis(5000));
        println!("Fetching prices from https://api.butane.dev/prices");
        let response = client
            .get("https://api.butane.dev/prices")
            .header(USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36")
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                eprintln!("Failed to fetch prices: {}", err);
                continue;
            }
        };
        // let payload = response.text().await?;
        let payloads: Result<Vec<OraclePayload>> = response
            .json()
            .await
            .context("failed to parse json response");
        let payloads = match payloads {
            Ok(payloads) => payloads,
            Err(err) => {
                eprintln!("Failed to parse prices: {}", err);
                continue;
            }
        };
        if let Some(directory) = &args.directory {
            let payloads: Result<Vec<_>> = payloads
                .iter()
                .map(|p| to_payload("butane".to_string(), p).context("failed to convert payload"))
                .collect();
            let payloads = match payloads {
                Ok(payloads) => payloads,
                Err(err) => {
                    eprintln!("Failed to convert payloads: {}", err);
                    continue;
                }
            };
            let path = Path::join(
                directory,
                format!("{}.json", Utc::now().format("%Y-%m-%d_%H-%M-%S.%f")),
            );
            println!("Saving to {}", path.display());
            match save_file(payloads, &path).await {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("Failed to save file: {}", err);
                }
            }
        }
    }
    /*
    sqlx::query!(
        "
        INSERT INTO payloads
        (source, synthetic, synthetic_numerator, synthetic_denominator, validity_lower, validity_upper, payload, error_text)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ",
        "example_source",
        "example_synthetic",
        vec![1, 2, 3],
        vec![4, 5, 6],
        100,
        200,
        vec![7, 8, 9],
        "example_error_text"
    )
        .execute(&pool)
        .await
        .context("failed to insert payload")?;

    let rows = sqlx::query_as!(Payload, "SELECT * FROM payloads")
        .fetch_all(&pool)
        .await
        .context("failed to fetch payloads")?;
        */
    // println!("{:?}", rows);
}

fn to_payload(source: String, payload: &OraclePayload) -> Result<Payload> {
    let payload = Payload {
        id: None,
        timestamp: Utc::now(),
        source: Some(source),
        synthetic: Some(payload.synthetic.clone()),
        synthetic_numerator: Some(payload.synthetic_price.numerator.clone()),
        synthetic_denominator: Some(payload.synthetic_price.denominator.clone()),
        validity_lower: Some(payload.validity.lower_bound.clone()),
        validity_upper: Some(payload.validity.upper_bound.clone()),
        payload: Some(hex::decode(payload.payload.clone())?),
        error_text: None,
    };
    Ok(payload)
}

async fn save_file(payload: Vec<Payload>, path: &impl AsRef<Path>) -> Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer(writer, &payload).context("failed to save file")
}
