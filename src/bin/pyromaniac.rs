use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use serde_aux::prelude::*;
use sqlx::{mysql::MySqlPoolOptions, MySql, Pool};
use time::{format_description, OffsetDateTime};

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
    timestamp: OffsetDateTime,
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
    let mut connection = None;
    if let Some(directory) = &args.directory {
        fs::create_dir_all(directory)?;
    } else if let Some(mysql) = &args.mysql {
        connection = Some(MySqlPoolOptions::new().connect(mysql).await?);
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
            match save_file(payloads, directory).await {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("Failed to save file: {}", err);
                }
            }
        } else if let Some(connection) = &connection {
            println!("Saving to {:?}", args.mysql.clone().unwrap());
            match save_db(payloads, connection).await {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("Failed to save database: {}", err);
                }
            }
        }
    }
}

fn to_payload(source: String, payload: &OraclePayload) -> Result<Payload> {
    let payload = Payload {
        id: None,
        timestamp: OffsetDateTime::now_utc(),
        source: Some(source),
        synthetic: Some(payload.synthetic.clone()),
        synthetic_numerator: Some(payload.synthetic_price.numerator.clone()),
        synthetic_denominator: Some(payload.synthetic_price.denominator.clone()),
        validity_lower: Some(payload.validity.lower_bound),
        validity_upper: Some(payload.validity.upper_bound),
        payload: Some(hex::decode(payload.payload.clone())?),
        error_text: None,
    };
    Ok(payload)
}

async fn save_file(payloads: Vec<OraclePayload>, directory: impl AsRef<Path>) -> Result<()> {
    let payloads: Result<Vec<_>> = payloads
        .iter()
        .map(|p| to_payload("butane".to_string(), p).context("failed to convert payload"))
        .collect();
    let payloads = match payloads {
        Ok(payloads) => payloads,
        Err(err) => {
            eprintln!("Failed to convert payloads: {}", err);
            return Ok(());
        }
    };
    let format = format_description::parse(
        "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour \
             sign:mandatory]:[offset_minute]:[offset_second]",
    )?;
    let path = Path::join(
        directory.as_ref(),
        format!("{}.json", OffsetDateTime::now_utc().format(&format)?),
    );
    println!("Saving to {}", path.display());
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer(writer, &payloads).context("failed to save file")
}

async fn save_db(payloads: Vec<OraclePayload>, connection: &Pool<MySql>) -> Result<()> {
    for payload in payloads.iter().map(|p| to_payload("butane".to_string(), p)) {
        let payload = payload?;
        sqlx::query!(
            "
            INSERT INTO payloads
            (source, synthetic, synthetic_numerator, synthetic_denominator, validity_lower, validity_upper, payload, error_text)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ",
            payload.source,
            payload.synthetic,
            payload.synthetic_numerator,
            payload.synthetic_denominator,
            payload.validity_lower,
            payload.validity_upper,
            payload.payload,
            payload.error_text,
        )
        .execute(connection)
        .await
        .context("failed to save to database")?;
    }
    Ok(())
}
