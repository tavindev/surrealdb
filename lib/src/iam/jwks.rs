use crate::err::Error;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyOperations, PublicKeyUse};
use jsonwebtoken::{DecodingKey, Validation};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
static CACHE_EXPIRATION: Lazy<chrono::Duration> = Lazy::new(|| Duration::seconds(1));
#[cfg(not(test))]
static CACHE_EXPIRATION: Lazy<chrono::Duration> =
	Lazy::new(|| match std::env::var("SURREAL_JWKS_CACHE_EXPIRATION") {
		Ok(seconds_str) => {
			let seconds = seconds_str
				.parse::<u64>()
				.expect("Expected a valid number of seconds for SURREAL_JWKS_CACHE_EXPIRATION");
			Duration::seconds(seconds as i64)
		}
		Err(_) => {
			Duration::seconds(43200) // Set default cache expiration of 12 hours
		}
	});

#[cfg(test)]
static CACHE_COOLDOWN: Lazy<chrono::Duration> = Lazy::new(|| Duration::seconds(300));
#[cfg(not(test))]
static CACHE_COOLDOWN: Lazy<chrono::Duration> =
	Lazy::new(|| match std::env::var("SURREAL_JWKS_CACHE_COOLDOWN") {
		Ok(seconds_str) => {
			let seconds = seconds_str
				.parse::<u64>()
				.expect("Expected a valid number of seconds for SURREAL_JWKS_CACHE_COOLDOWN");
			Duration::seconds(seconds as i64)
		}
		Err(_) => {
			Duration::seconds(300) // Set default cache refresh cooldown of 5 minutes
		}
	});

static REMOTE_TIMEOUT: Lazy<chrono::Duration> =
	Lazy::new(|| match std::env::var("SURREAL_JWKS_REMOTE_TIMEOUT") {
		Ok(milliseconds_str) => {
			let milliseconds = milliseconds_str
				.parse::<u64>()
				.expect("Expected a valid number of milliseconds for SURREAL_JWKS_REMOTE_TIMEOUT");
			Duration::milliseconds(milliseconds as i64)
		}
		Err(_) => {
			Duration::milliseconds(1000) // Set default remote timeout to 1 second
		}
	});

pub(super) async fn config(kid: String, url: String) -> Result<(DecodingKey, Validation), Error> {
	// Attempt to fetch relevant JWK object either from local cache or remote location
	let jwk = match fetch_jwks_from_cache(url.clone()).await {
		Ok(jwks_cache) => {
			trace!("Successfully fetched JWKS object from local cache");
			// Check that the cached JWKS object has not expired yet
			if Utc::now().signed_duration_since(jwks_cache.time) < *CACHE_EXPIRATION {
				// Attempt to find JWK in JWKS object from local cache
				match jwks_cache.jwks.find(&kid) {
					Some(jwk) => jwk.to_owned(),
					_ => {
						trace!("Could not find valid JWK object in cached JWKS object");
						// Check that the cached JWKS object has not been recently updated
						if Utc::now().signed_duration_since(jwks_cache.time) < *CACHE_COOLDOWN {
							warn!("Refused to refresh cache too soon after last refresh");
							return Err(Error::InvalidAuth); // Return opaque error
						}
						find_jwk_and_cache_jwks(url, kid).await?
					}
				}
			} else {
				trace!("Fetched JWKS object from local cache has expired");
				find_jwk_and_cache_jwks(url, kid).await?
			}
		}
		Err(_) => {
			trace!("Could not fetch JWKS object from local cache");
			find_jwk_and_cache_jwks(url, kid).await?
		}
	};
	// Check if key is intended to be used for signing
	// Source: https://datatracker.ietf.org/doc/html/rfc7517#section-4.2
	if let Some(PublicKeyUse::Signature) = &jwk.common.public_key_use {
	} else {
		warn!("Invalid value for parameter 'use' in JWK object: '{:?}'", jwk.common.public_key_use);
		return Err(Error::JwkInvalidParameter("use".to_string()));
	}
	// Check if key operations (if specified) include verification
	// Source: https://datatracker.ietf.org/doc/html/rfc7517#section-4.3
	if let Some(ops) = &jwk.common.key_operations {
		if !ops.iter().any(|op| *op == KeyOperations::Verify) {
			warn!(
				"Invalid value for parameter 'key_ops' in JWK object: '{:?}'",
				jwk.common.key_operations
			);
			return Err(Error::JwkInvalidParameter("key_ops".to_string()));
		}
	}
	// Check if algorithm specified is supported
	let alg = match jwk.common.algorithm {
		Some(alg) => alg,
		_ => {
			warn!("Invalid value for parameter 'alg' in JWK object: '{:?}'", jwk.common.algorithm);
			return Err(Error::JwkInvalidParameter("alg".to_string()));
		}
	};
	Ok((DecodingKey::from_jwk(&jwk)?, Validation::new(alg)))
}

// Attempts to find a relevant JWK object inside a JWKS object fetched from a remote location
// Caches the fetched JWKS object in the local cache even if the JWK object is not found
async fn find_jwk_and_cache_jwks(url: String, kid: String) -> Result<Jwk, Error> {
	// Attempt to fetch JWKS object from remote location
	match fetch_jwks_from_url(url.clone()).await {
		Ok(jwks) => {
			trace!("Successfully fetched JWKS object from remote location");
			// If successful, cache the JWKS object by its URL
			match cache_jwks(jwks.clone(), url.clone()).await {
				Ok(_) => trace!("Successfully stored JWKS object in local cache"),
				Err(err) => {
					warn!("Failed to store JWKS object in local cache: '{}'", err)
				}
			};
			// Attempt to find JWK in JWKS by the key identifier
			match jwks.find(&kid) {
				Some(jwk) => Ok(jwk.to_owned()),
				_ => Err(Error::JwkNotFound(kid, url)),
			}
		}
		Err(err) => {
			warn!("Failed to fetch JWKS object from remote location: '{}'", err);
			Err(Error::JwksNotFound(url))
		}
	}
}

async fn fetch_jwks_from_url(url: String) -> Result<JwkSet, Error> {
	let client = Client::new();
	let res = client.get(&url).timeout((*REMOTE_TIMEOUT).to_std().unwrap()).send().await?;
	if !res.status().is_success() {
		return Err(Error::JwksNotFound(res.url().to_string()));
	}
	let jwks = res.bytes().await?;
	match serde_json::from_slice(&jwks) {
		Ok(jwks) => Ok(jwks),
		Err(err) => {
			warn!("Failed to parse JWKS object: '{}'", err);
			Err(Error::JwksMalformed)
		}
	}
}

#[derive(Serialize, Deserialize)]
struct JwksCache {
	jwks: JwkSet,
	time: DateTime<Utc>,
}

fn cache_path_from_url(url: String) -> String {
	let mut hasher = Sha256::new();
	hasher.update(url.as_bytes());
	let result = hasher.finalize();
	format!("jwks/{:x}.json", result)
}

async fn cache_jwks(jwks: JwkSet, url: String) -> Result<(), Error> {
	let jwks_cache = JwksCache {
		jwks,
		time: Utc::now(),
	};
	let path = cache_path_from_url(url);
	match serde_json::to_vec(&jwks_cache) {
		Ok(data) => crate::obs::put(&path, data).await,
		Err(err) => {
			println!("{}", err);
			Err(Error::JwksMalformed)
		}
	}
}

async fn fetch_jwks_from_cache(url: String) -> Result<JwksCache, Error> {
	let path = cache_path_from_url(url);
	let bytes = crate::obs::get(&path).await?;
	let jwks_cache: JwksCache = match serde_json::from_slice(&bytes) {
		Ok(jwks_cache) => jwks_cache,
		Err(err) => {
			warn!("Failed to parse JWKS object: '{}'", err);
			return Err(Error::JwksMalformed);
		}
	};
	Ok(jwks_cache)
}

#[cfg(test)]
mod tests {
	use super::*;
	use rand::{distributions::Alphanumeric, Rng};
	use wiremock::matchers::{method, path};
	use wiremock::{Mock, MockServer, ResponseTemplate};

	// Use unique path to prevent accidental cache reuse
	fn random_path() -> String {
		let rng = rand::thread_rng();
		rng.sample_iter(&Alphanumeric).take(8).map(char::from).collect()
	}

	static DEFAULT_JWKS: Lazy<JwkSet> = Lazy::new(|| {
		JwkSet{
		keys: vec![Jwk{
			common: jsonwebtoken::jwk::CommonParameters {
				public_key_use: Some(jsonwebtoken::jwk::PublicKeyUse::Signature),
				key_operations: None,
				algorithm: Some(jsonwebtoken::Algorithm::RS256),
				key_id: Some("test_1".to_string()),
				x509_url: None,
				x509_chain: Some(vec![
					"MIIDBTCCAe2gAwIBAgIJdeyDfUXHLwX+MA0GCSqGSIb3DQEBCwUAMCAxHjAcBgNVBAMTFWdlcmF1c2VyLmV1LmF1dGgwLmNvbTAeFw0yMzEwMzExNzI5MDBaFw0zNzA3MDkxNzI5MDBaMCAxHjAcBgNVBAMTFWdlcmF1c2VyLmV1LmF1dGgwLmNvbTCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBANp7Er60Z7sOjiyQqpbZIkQBldO/t+YrHT8Mk661kNz8MRGXPpQ26rkO2fRZMeWPpXVcwW0xxLG5oQ2Iwbm1JUuHYbLb6WA/d/et0sOkOoEI6MP0+MVqrGnro+D6XGoz4yP8m2w8C2u2yFxAc+wAt1AIMWNJIYhEX6tqrliGnitDCye2wXKchhe4WctUlHoUNfO/sgazPQ7ItqisUF/fNSRbHLRJyS2mm76FlDELDLnEyVwUaeV/2xie9F44AfOQzVk1asO18BH3v6YjOQ3L41XEfOm2DMPkLOOmtyM7yA7OeF/fvn6zN+SByza6cFW37IOKoJsmvxkzxeDUlWm9MWkCAwEAAaNCMEAwDwYDVR0TAQH/BAUwAwEB/zAdBgNVHQ4EFgQUOeOmT9I3/MJ/zI/lS74gPQmAQfEwDgYDVR0PAQH/BAQDAgKEMA0GCSqGSIb3DQEBCwUAA4IBAQBDue8iM90XJcLORvr6e+h15f5tlvVjZ/cAzv09487QSHJtUd6qwTlunEABS5818TgMEMFDRafQ7CDX3KaAAXFpo2bnyWY9c+Ozp0PWtp8ChunOs94ayaG+viO0AiTrIY28cc26ehNBZ/4gC4/1k0IlXEk8rd1e03hQuhNiy7SQaxS2f1xkJfR4vCeF8HTN5omjKvIMcWRqkkwFZm4gdgkMfi2lNsV8V6+HXyTc3XUIdcwOUcC+Ms/m+vKxnyxw0UGh0RliB1qBc0ADg83hOsXEqZjneHh1ZhqqVF4IkKSJTfK5ofcc14GqvpLjjTR3s2eX6zxdujzwf4gnHdxjVvdJ".to_string(),
				]),
				x509_sha1_fingerprint: None,
				x509_sha256_fingerprint: None,
			},
			algorithm: jsonwebtoken::jwk::AlgorithmParameters::RSA(
				jsonwebtoken::jwk::RSAKeyParameters{
				key_type: jsonwebtoken::jwk::RSAKeyType::RSA,
				n: "2nsSvrRnuw6OLJCqltkiRAGV07-35isdPwyTrrWQ3PwxEZc-lDbquQ7Z9Fkx5Y-ldVzBbTHEsbmhDYjBubUlS4dhstvpYD93963Sw6Q6gQjow_T4xWqsaeuj4PpcajPjI_ybbDwLa7bIXEBz7AC3UAgxY0khiERfq2quWIaeK0MLJ7bBcpyGF7hZy1SUehQ187-yBrM9Dsi2qKxQX981JFsctEnJLaabvoWUMQsMucTJXBRp5X_bGJ70XjgB85DNWTVqw7XwEfe_piM5DcvjVcR86bYMw-Qs46a3IzvIDs54X9--frM35IHLNrpwVbfsg4qgmya_GTPF4NSVab0xaQ".to_string(),
				e: "AQAB".to_string(),
				}
			),
		},
		Jwk{
			common: jsonwebtoken::jwk::CommonParameters {
				public_key_use: Some(jsonwebtoken::jwk::PublicKeyUse::Signature),
				key_operations: None,
				algorithm: Some(jsonwebtoken::Algorithm::RS256),
				key_id: Some("test_2".to_string()),
				x509_url: None,
				x509_chain: Some(vec![
					"MIIDBTCCAe2gAwIBAgIJUzJ062XCgOVQMA0GCSqGSIb3DQEBCwUAMCAxHjAcBgNVBAMTFWdlcmF1c2VyLmV1LmF1dGgwLmNvbTAeFw0yMzEwMzExNzI5MDBaFw0zNzA3MDkxNzI5MDBaMCAxHjAcBgNVBAMTFWdlcmF1c2VyLmV1LmF1dGgwLmNvbTCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAL7TgjrojPySPO5kPBOiAgiS0guSpYkfb0AtLNVyVeANe5vgjJoBQDe7vAGm68SHft841GQBPp5KWpxDTO+liECVAnbdR9YHwuZWOGuPRVVwNqtVmS8A75YG/mTWGV4tr2h+dLjjV3jvV0hvXRJwVFShlUS9+BqgevFBoF6zxi5AHIx/k1tCg1y2fhSlzYUHxEiFRgx0RhtJfizyv9QHoLSY3RFI4QOAkPtYwN5C1X69nEHPK0Q+W+POkeV7wuMQZWTRRT+xZuYn+JIYQCQviZ52FoJsrTzOEO5jlmrUa9PMEJpn0Aw68OdyLHjQPsip8B2JSegoVP1LTc0tDoqVGqUCAwEAAaNCMEAwDwYDVR0TAQH/BAUwAwEB/zAdBgNVHQ4EFgQUYp67WM42b2pqF7ES0LsFvAI/Qy8wDgYDVR0PAQH/BAQDAgKEMA0GCSqGSIb3DQEBCwUAA4IBAQANOhmYz0jxNJG6pZ0klNtH00E6SoEsM/MNYH+atraTVZNeqPLAZH9514gMcqdu7+rBfQ/pRjpQG1YbkdZGQBaq5cZNlE6hNCT4BSgKddBYsN0WbfTGxstDVdoXLySgGCYpyKO6Hek4ULxwAf1LmMyOYpn4JrECy4mYShsCcfe504qfzUTd7pz1VaZ4minclOhz0dZbgYa+slUepe0C2+w+T3US138x0lPB9C266SLDakb6n/JTum+Czn2xlFBf4K4w6eWuSknvTlRrqTGE8RX3vzOiKTM3hpDdjU7Tu7eNsZpLkDR1e+w33m5NMi9iYgJcyTGsIeeHr0xjrRPD9Dwh".to_string(),
				]),
				x509_sha1_fingerprint: None,
				x509_sha256_fingerprint: None,
			},
			algorithm: jsonwebtoken::jwk::AlgorithmParameters::RSA(
				jsonwebtoken::jwk::RSAKeyParameters{
				key_type: jsonwebtoken::jwk::RSAKeyType::RSA,
				n: "vtOCOuiM_JI87mQ8E6ICCJLSC5KliR9vQC0s1XJV4A17m-CMmgFAN7u8AabrxId-3zjUZAE-nkpanENM76WIQJUCdt1H1gfC5lY4a49FVXA2q1WZLwDvlgb-ZNYZXi2vaH50uONXeO9XSG9dEnBUVKGVRL34GqB68UGgXrPGLkAcjH-TW0KDXLZ-FKXNhQfESIVGDHRGG0l-LPK_1AegtJjdEUjhA4CQ-1jA3kLVfr2cQc8rRD5b486R5XvC4xBlZNFFP7Fm5if4khhAJC-JnnYWgmytPM4Q7mOWatRr08wQmmfQDDrw53IseNA-yKnwHYlJ6ChU_UtNzS0OipUapQ".to_string(),
				e: "AQAB".to_string(),
				}
			),
		}
		],
	}
	});

	#[tokio::test]
	async fn test_golden_path() {
		let jwks = DEFAULT_JWKS.clone();

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		// Get first token configuration from remote location
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_ok(), "Failed to validate token the first time: {:?}", res.err());

		// Drop server to force usage of the local cache
		drop(mock_server);

		// Get second token configuration from local cache
		let res = config("test_2".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_ok(), "Failed to validate token the second time: {:?}", res.err());
	}

	#[tokio::test]
	async fn test_cache_expiration() {
		let jwks = DEFAULT_JWKS.clone();

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.expect(2)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		// Get token configuration from remote location
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_ok(), "Failed to validate token the first time: {:?}", res.err());

		// Wait for cache to expire
		std::thread::sleep((*CACHE_EXPIRATION + Duration::seconds(1)).to_std().unwrap());

		// Get same token configuration again after cache has expired
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_ok(), "Failed to validate token the second time: {:?}", res.err());

		// The server will panic if it does not receive exactly two expected requests
	}

	#[tokio::test]
	async fn test_cache_expiration_remote_down() {
		let jwks = DEFAULT_JWKS.clone();

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.up_to_n_times(1) // Only respond the first time
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		// Get token configuration from remote location
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_ok(), "Failed to validate token the first time: {:?}", res.err());

		// Wait for cache to expire
		std::thread::sleep((*CACHE_EXPIRATION + Duration::seconds(1)).to_std().unwrap());

		// Get same token configuration again after cache has expired
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token with an expired cache and remote down"
		);
	}

	#[tokio::test]
	async fn test_unsupported_algorithm() {
		let mut jwks = DEFAULT_JWKS.clone();
		jwks.keys[0].common.algorithm = None;

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token with key using unsupported algorithm"
		);
	}

	#[tokio::test]
	async fn test_encryption_only_use() {
		let mut jwks = DEFAULT_JWKS.clone();
		jwks.keys[0].common.public_key_use = Some(jsonwebtoken::jwk::PublicKeyUse::Encryption);

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token with key that only supports encryption"
		);
	}

	#[tokio::test]
	async fn test_encryption_only_ops() {
		let mut jwks = DEFAULT_JWKS.clone();
		jwks.keys[0].common.key_operations = Some(vec![jsonwebtoken::jwk::KeyOperations::Encrypt]);

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token with key that only supports encryption"
		);
	}

	#[tokio::test]
	async fn test_dos_protection() {
		let jwks = DEFAULT_JWKS.clone();

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200).set_body_json(jwks);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.expect(1)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		// Use token with invalid key identifier claim to force cache refresh
		let res = config("invalid".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_err(), "Unexpected success validating token with invalid key identifier");

		// Use token with invalid key identifier claim to force cache refresh again before cooldown
		let res = config("invalid".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(res.is_err(), "Unexpected success validating token with invalid key identifier");

		// The server will panic if it receives more than the single expected request
	}

	#[tokio::test]
	async fn test_remote_timeout() {
		let jwks = DEFAULT_JWKS.clone();

		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(200)
			.set_body_json(jwks)
			.set_delay((*REMOTE_TIMEOUT + Duration::seconds(10)).to_std().unwrap());
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.expect(1)
			.mount(&mock_server)
			.await;
		let url = mock_server.uri();

		let start_time = Utc::now();
		// Get token configuration from remote location respnding very slowly
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token configuration with unavailable remote location"
		);
		assert!(
			Utc::now() - start_time < *REMOTE_TIMEOUT + Duration::seconds(1),
			"Remote request was not aborted immediately after timeout"
		);
	}

	#[tokio::test]
	async fn test_remote_down() {
		let jwks_path = format!("{}/jwks.json", random_path());
		let mock_server = MockServer::start().await;
		let response = ResponseTemplate::new(500);
		Mock::given(method("GET"))
			.and(path(&jwks_path))
			.respond_with(response)
			.mount(&mock_server)
			.await;

		let url = mock_server.uri();

		// Get token configuration from remote location respnding with Internal Server Error
		let res = config("test_1".to_string(), format!("{}/{}", &url, &jwks_path)).await;
		assert!(
			res.is_err(),
			"Unexpected success validating token configuration with unavailable remote location"
		);
	}
}
