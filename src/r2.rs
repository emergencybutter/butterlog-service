use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use crate::config::Config;

#[derive(Clone, Debug)]
pub struct R2Client {
    client: aws_sdk_s3::Client,
    bucket: String,
    public_url: String,
}

impl R2Client {
    pub fn new(config: &Config) -> Self {
        let credentials = Credentials::new(
            &config.r2_access_key_id,
            &config.r2_secret_access_key,
            None,
            None,
            "hardcoded",
        );

        let s3_config = aws_sdk_s3::config::Builder::new()
            .region(Region::new("auto"))
            .endpoint_url(&config.r2_endpoint)
            .credentials_provider(credentials)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();

        let client = aws_sdk_s3::Client::from_conf(s3_config);

        Self {
            client,
            bucket: config.r2_bucket.clone(),
            public_url: config.r2_public_url.clone(),
        }
    }

    pub async fn upload_object(&self, key: &str, data: Vec<u8>, content_type: &str) -> Result<String, String> {
        let body = ByteStream::from(data);
        
        tracing::info!("[Outgoing Request] R2 PUT {} (bucket: {}, content_type: {})", key, self.bucket, content_type);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("R2 PUT {} failed: {:?}", key, e);
                format!("Failed to upload object to R2: {:?}", e)
            })?;
        tracing::info!("[Outgoing Response] R2 PUT {} completed successfully", key);

        let separator = if self.public_url.ends_with('/') { "" } else { "/" };
        Ok(format!("{}{}{}", self.public_url, separator, key))
    }

    pub async fn delete_object(&self, key: &str) -> Result<(), String> {
        tracing::info!("[Outgoing Request] R2 DELETE {} (bucket: {})", key, self.bucket);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("R2 DELETE {} failed: {:?}", key, e);
                format!("Failed to delete object from R2: {:?}", e)
            })?;
        tracing::info!("[Outgoing Response] R2 DELETE {} completed successfully", key);

        Ok(())
    }

    pub async fn download_object(&self, key: &str) -> Result<Vec<u8>, String> {
        tracing::info!("[Outgoing Request] R2 GET {} (bucket: {})", key, self.bucket);
        let resp = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("R2 GET {} failed: {:?}", key, e);
                format!("Failed to download object from R2: {:?}", e)
            })?;
        tracing::info!("[Outgoing Response] R2 GET {} completed successfully", key);

        let data = resp.body.collect().await
            .map_err(|e| format!("Failed to read R2 byte stream: {:?}", e))?;

        Ok(data.into_bytes().to_vec())
    }
}
