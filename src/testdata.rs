use std::time::SystemTime;
use std::fs::File;
use std::io::{BufReader, Read};
use flate2::bufread::GzDecoder;
use serde::Deserialize;
use smallvec::SmallVec;

/// This file contains some simple helpers for loading test data. Its used by benchmarking and
/// testing code.

#[derive(Debug, Clone, Deserialize)]
pub struct TestPatch(pub usize, pub usize, pub String);

#[derive(Debug, Clone, Deserialize)]
pub struct TestTxn {
    // time: String, // ISO String. Unused.
    pub patches: Vec<TestPatch>
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestData {
    #[serde(rename = "startContent")]
    pub start_content: String,
    #[serde(rename = "endContent")]
    pub end_content: String,

    pub txns: Vec<TestTxn>,
}

pub fn load_testing_data(filename: &str) -> TestData {
    // let start = SystemTime::now();
    // let mut file = File::open("benchmark_data/automerge-paper.json.gz").unwrap();
    let file = File::open(filename).unwrap();

    let reader = BufReader::new(file);
    // We could pass the GzDecoder straight to serde, but it makes it way slower to parse for
    // some reason.
    let mut reader = GzDecoder::new(reader);
    let mut raw_json = vec!();
    reader.read_to_end(&mut raw_json).unwrap();

    // println!("uncompress time {}", start.elapsed().unwrap().as_millis());

    // let start = SystemTime::now();
    let data: TestData = serde_json::from_reader(raw_json.as_slice()).unwrap();
    // println!("JSON parse time {}", start.elapsed().unwrap().as_millis());

    data
}