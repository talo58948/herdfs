//! The lab's own infrastructure, without the authority or client binaries.
//! contract: none; these check the lab that the contract suites run in.

use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{BadToken, Lab, LabConfig, Links, Profile, Strategy};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};

fn near_and_far() -> Result<Lab> {
    Lab::start_infra(
        LabConfig::default()
            .client("near", "alice", &["eng"], Profile::Lan, Strategy::Stream)
            .client("far", "bob", &[], Profile::Wan, Strategy::Stream),
    )
}

#[test]
fn issuer_keys_validate_its_tokens_and_reject_the_bad_ones() -> Result<()> {
    let lab = near_and_far()?;
    let http = reqwest::blocking::Client::new();
    let discovery_url = format!("{}/.well-known/openid-configuration", lab.issuer_url());
    let discovery: Value = http.get(discovery_url).send()?.json()?;
    assert_eq!(discovery["issuer"], lab.issuer_url());
    let jwks_url = discovery["jwks_uri"].as_str().expect("discovery names the JWKS");
    let jwks: JwkSet = http.get(jwks_url).send()?.json()?;
    let [jwk] = jwks.keys.as_slice() else {
        panic!("expected one key, got {jwks:?}");
    };
    let key = DecodingKey::from_jwk(jwk)?;
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[lab.issuer_url()]);
    validation.set_audience(&["herdfs"]);
    let validate =
        |token: &str| jsonwebtoken::decode::<Value>(token, &key, &validation).map(|data| data.claims);

    let claims = validate(&lab.token("alice", &["eng"]))?;
    assert_eq!(
        (&claims["sub"], &claims["groups"]),
        (&json!("alice"), &json!(["eng"]))
    );
    let rejected = [
        BadToken::Expired,
        BadToken::WrongAudience,
        BadToken::UnknownKey,
        BadToken::AlgNone,
    ]
    .map(|kind| {
        validate(&lab.bad_token("alice", kind))
            .map_err(|error| error.into_kind())
            .err()
    });
    assert!(
        matches!(
            rejected,
            [
                Some(ErrorKind::ExpiredSignature),
                Some(ErrorKind::InvalidAudience),
                Some(ErrorKind::InvalidSignature),
                Some(_),
            ]
        ),
        "{rejected:?}"
    );
    Ok(())
}

#[test]
fn presigned_urls_work_through_each_clients_store_link() -> Result<()> {
    let lab = near_and_far()?;
    let put = lab
        .http("near")?
        .put(lab.presign_put("smoke"))
        .body("hello")
        .send()?;
    assert!(put.status().is_success(), "{}", put.text()?);
    let get = lab.http("far")?.get(lab.presign_get("smoke")).send()?;
    assert!(get.status().is_success(), "{}", get.status());
    assert_eq!(get.text()?, "hello");
    Ok(())
}

#[test]
fn each_profile_adds_round_trip_latency() -> Result<()> {
    let lab = Lab::start_infra(
        LabConfig::default()
            .client("near", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("far", "bob", &[], Profile::Wan, Strategy::Stream)
            .client("rough", "carol", &[], Profile::Bad, Strategy::Stream),
    )?;
    let url = lab.presign_get("absent");
    let median = |name: &str| -> Result<Duration> {
        let http = lab.http(name)?;
        let mut times = (0..5)
            .map(|_| {
                let start = Instant::now();
                http.get(url.clone()).send()?;
                Ok(start.elapsed())
            })
            .collect::<Result<Vec<_>>>()?;
        times.sort();
        Ok(times[times.len() / 2])
    };
    let (lan, wan, bad) = (median("near")?, median("far")?, median("rough")?);
    let margin = Duration::from_millis(60);
    assert!(
        wan >= lan + margin && bad >= wan + margin,
        "lan {lan:?}, wan {wan:?}, bad {bad:?}"
    );
    Ok(())
}

#[test]
fn cut_refuses_a_clients_links_and_restore_reopens_them() -> Result<()> {
    let lab = near_and_far()?;
    let http = lab.http("near")?;
    let url = lab.presign_get("absent");
    assert!(http.get(url.clone()).send().is_ok());
    lab.cut("near", Links::Both)?;
    assert!(http.get(url.clone()).send().is_err());
    assert!(
        lab.http("far")?.get(url.clone()).send().is_ok(),
        "other clients keep their links"
    );
    lab.restore("near", Links::Both)?;
    assert!(http.get(url).send().is_ok());
    Ok(())
}
