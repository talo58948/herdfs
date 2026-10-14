use reqwest::Client;
use serde::Deserialize;

use super::{TokenFile, expiry};
use crate::{Error, Result};

const USER_AGENT: &str = concat!("herdfs/", env!("CARGO_PKG_VERSION"));

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    Client::builder().user_agent(USER_AGENT)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

pub(crate) async fn exchange(
    http: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<TokenFile> {
    let response = http
        .post(endpoint)
        .form(form)
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    let refresh_token = response
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::Protocol("issuer returned no refresh token".into()))?;
    let client_id = form
        .iter()
        .find_map(|(name, value)| (*name == "client_id").then(|| (*value).to_owned()))
        .ok_or_else(|| Error::Protocol("token exchange omitted client_id".into()))?;
    let resource = form
        .iter()
        .find_map(|(name, value)| (*name == "resource").then(|| (*value).to_owned()));
    Ok(TokenFile {
        generation: 0,
        access_token: response.access_token,
        refresh_token,
        expires_at: expiry(response.expires_in),
        client_id,
        token_endpoint: endpoint.to_owned(),
        resource,
    })
}
