use super::{Blob, StorageTransaction};
use chrono::{DateTime, NaiveDateTime, Utc};
use failure::Error;
use futures_util::{
    future::TryFutureExt,
    stream::{FuturesUnordered, StreamExt},
};
use log::warn;
use once_cell::sync::Lazy;
use rusoto_core::region::Region;
use rusoto_credential::DefaultCredentialsProvider;
use rusoto_s3::{GetObjectRequest, PutObjectRequest, S3Client, S3};
use std::{convert::TryInto, io::Write};
use tokio::runtime::Runtime;

#[cfg(test)]
mod test;
#[cfg(test)]
pub(crate) use test::TestS3;

pub(crate) static S3_BUCKET_NAME: &str = "rust-docs-rs";
pub(crate) static S3_RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Failed to create S3 runtime"));

pub(crate) struct S3Backend {
    pub client: S3Client,
    bucket: String,
}

impl S3Backend {
    pub(crate) fn new(client: S3Client, bucket: &str) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    pub(super) fn get(&self, path: &str, max_size: usize) -> Result<Blob, Error> {
        S3_RUNTIME.handle().block_on(async {
            let res = self
                .client
                .get_object(GetObjectRequest {
                    bucket: self.bucket.to_string(),
                    key: path.into(),
                    ..Default::default()
                })
                .await?;

            let mut content = crate::utils::sized_buffer::SizedBuffer::new(max_size);
            content.reserve(
                res.content_length
                    .and_then(|l| l.try_into().ok())
                    .unwrap_or(0),
            );

            let mut body = res
                .body
                .ok_or_else(|| failure::err_msg("Received a response from S3 with no body"))?;

            while let Some(data) = body.next().await.transpose()? {
                content.write_all(data.as_ref())?;
            }

            let date_updated = parse_timespec(&res.last_modified.unwrap())?;
            let compression = res.content_encoding.and_then(|s| s.parse().ok());

            Ok(Blob {
                path: path.into(),
                mime: res.content_type.unwrap(),
                date_updated,
                content: content.into_inner(),
                compression,
            })
        })
    }

    pub(super) fn start_storage_transaction(&self) -> Result<S3StorageTransaction, Error> {
        Ok(S3StorageTransaction { s3: self })
    }
}

pub(super) struct S3StorageTransaction<'a> {
    s3: &'a S3Backend,
}

impl<'a> StorageTransaction for S3StorageTransaction<'a> {
    fn store_batch(&mut self, mut batch: Vec<Blob>) -> Result<(), Error> {
        S3_RUNTIME.handle().block_on(async {
            // Attempt to upload the batch 3 times
            for _ in 0..3 {
                let mut futures = FuturesUnordered::new();
                for blob in batch.drain(..) {
                    futures.push(
                        self.s3
                            .client
                            .put_object(PutObjectRequest {
                                bucket: self.s3.bucket.to_string(),
                                key: blob.path.clone(),
                                body: Some(blob.content.clone().into()),
                                content_type: Some(blob.mime.clone()),
                                content_encoding: blob
                                    .compression
                                    .as_ref()
                                    .map(|alg| alg.to_string()),
                                ..Default::default()
                            })
                            .map_ok(|_| {
                                crate::web::metrics::UPLOADED_FILES_TOTAL.inc_by(1);
                            })
                            .map_err(|err| {
                                log::error!("Failed to upload blob to S3: {:?}", err);
                                // Reintroduce failed blobs for a retry
                                blob
                            }),
                    );
                }

                while let Some(result) = futures.next().await {
                    // Push each failed blob back into the batch
                    if let Err(blob) = result {
                        batch.push(blob);
                    }
                }

                // If we uploaded everything in the batch, we're done
                if batch.is_empty() {
                    return Ok(());
                }
            }

            panic!("failed to upload 3 times, exiting");
        })
    }

    fn complete(self: Box<Self>) -> Result<(), Error> {
        Ok(())
    }
}

fn parse_timespec(mut raw: &str) -> Result<DateTime<Utc>, Error> {
    raw = raw.trim_end_matches(" GMT");

    Ok(DateTime::from_utc(
        NaiveDateTime::parse_from_str(raw, "%a, %d %b %Y %H:%M:%S")?,
        Utc,
    ))
}

pub(crate) fn s3_client() -> Option<S3Client> {
    // If AWS keys aren't configured, then presume we should use the DB exclusively
    // for file storage.
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() && std::env::var_os("FORCE_S3").is_none() {
        return None;
    }

    let creds = match DefaultCredentialsProvider::new() {
        Ok(creds) => creds,
        Err(err) => {
            warn!("failed to retrieve AWS credentials: {}", err);
            return None;
        }
    };

    Some(S3Client::new_with(
        rusoto_core::request::HttpClient::new().unwrap(),
        creds,
        std::env::var("S3_ENDPOINT")
            .ok()
            .map(|e| Region::Custom {
                name: std::env::var("S3_REGION").unwrap_or_else(|_| "us-west-1".to_owned()),
                endpoint: e,
            })
            .unwrap_or(Region::UsWest1),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test::*;
    use chrono::TimeZone;

    #[test]
    fn test_parse_timespec() {
        // Test valid conversions
        assert_eq!(
            parse_timespec("Thu, 1 Jan 1970 00:00:00 GMT").unwrap(),
            Utc.ymd(1970, 1, 1).and_hms(0, 0, 0),
        );
        assert_eq!(
            parse_timespec("Mon, 16 Apr 2018 04:33:50 GMT").unwrap(),
            Utc.ymd(2018, 4, 16).and_hms(4, 33, 50),
        );

        // Test invalid conversion
        assert!(parse_timespec("foo").is_err());
    }

    #[test]
    fn test_get() {
        wrapper(|env| {
            let blob = Blob {
                path: "dir/foo.txt".into(),
                mime: "text/plain".into(),
                date_updated: Utc::now(),
                content: "Hello world!".into(),
                compression: None,
            };

            // Add a test file to the database
            let s3 = env.s3();
            s3.upload(vec![blob.clone()]).unwrap();

            // Test that the proper file was returned
            s3.assert_blob(&blob, "dir/foo.txt");

            // Test that other files are not returned
            s3.assert_404("dir/bar.txt");
            s3.assert_404("foo.txt");

            Ok(())
        });
    }

    #[test]
    fn test_get_too_big() {
        const MAX_SIZE: usize = 1024;

        wrapper(|env| {
            let small_blob = Blob {
                path: "small-blob.bin".into(),
                mime: "text/plain".into(),
                date_updated: Utc::now(),
                content: vec![0; MAX_SIZE],
                compression: None,
            };
            let big_blob = Blob {
                path: "big-blob.bin".into(),
                mime: "text/plain".into(),
                date_updated: Utc::now(),
                content: vec![0; MAX_SIZE * 2],
                compression: None,
            };

            let s3 = env.s3();
            s3.upload(vec![small_blob.clone()]).unwrap();
            s3.upload(vec![big_blob]).unwrap();

            s3.with_client(|client| {
                let blob = client.get("small-blob.bin", MAX_SIZE).unwrap();
                assert_eq!(blob.content.len(), small_blob.content.len());

                assert!(client
                    .get("big-blob.bin", MAX_SIZE)
                    .unwrap_err()
                    .downcast_ref::<std::io::Error>()
                    .and_then(|io| io.get_ref())
                    .and_then(|err| err.downcast_ref::<crate::error::SizeLimitReached>())
                    .is_some());
            });

            Ok(())
        })
    }

    #[test]
    fn test_store() {
        wrapper(|env| {
            let s3 = env.s3();
            let names = [
                "a",
                "b",
                "a_very_long_file_name_that_has_an.extension",
                "parent/child",
                "h/i/g/h/l/y/_/n/e/s/t/e/d/_/d/i/r/e/c/t/o/r/i/e/s",
            ];

            let blobs: Vec<_> = names
                .iter()
                .map(|&path| Blob {
                    path: path.into(),
                    mime: "text/plain".into(),
                    date_updated: Utc::now(),
                    content: "Hello world!".into(),
                    compression: None,
                })
                .collect();

            s3.upload(blobs.clone()).unwrap();
            for blob in &blobs {
                s3.assert_blob(blob, &blob.path);
            }

            Ok(())
        })
    }

    // NOTE: trying to upload a file ending with `/` will behave differently in test and prod.
    // NOTE: On s3, it will succeed and create a file called `/`.
    // NOTE: On min.io, it will fail with 'Object name contains unsupported characters.'
}
