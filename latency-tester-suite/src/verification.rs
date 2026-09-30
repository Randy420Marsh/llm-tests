//! Verification utilities for result validation and hash checking

use anyhow::Result;
use sha2::{Sha256, Digest};
use std::path::Path;

/// Verify application binary integrity
pub fn verify_app_integrity(expected_hash: &str) -> Result<bool> {
    let exe_path = std::env::current_exe()?;
    let mut file = std::fs::File::open(&exe_path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    let actual_hash = hex::encode(hash);
    
    Ok(actual_hash == expected_hash)
}

/// Compute hash of a file
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Verify a result file against its embedded signature
pub fn verify_result_file(path: &Path, public_key: &[u8]) -> Result<crate::result_logger::VerificationResult> {
    use crate::result_logger::{VerifiedResult, ResultLogger};
    
    let mut file = std::fs::File::open(path)?;
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents)?;
    
    let result: VerifiedResult = serde_json::from_str(&contents)?;
    
    // Create a temporary logger with the provided public key for verification
    let logger = ResultLogger::new_with_key(
        result.header.app_version.clone(),
        ".".to_string(),
        public_key.try_into().map_err(|_| anyhow::anyhow!("Invalid public key length"))?,
    )?;
    
    Ok(logger.verify_result(&result))
}

/// Generate a hash for the current application binary
pub fn generate_app_hash() -> Result<String> {
    let exe_path = std::env::current_exe()?;
    compute_file_hash(&exe_path)
}

/// Create a signed result package for distribution
pub fn create_signed_package(
    results_dir: &Path,
    output_path: &Path,
    private_key: &[u8; 32],
) -> Result<()> {
    use crate::result_logger::{ResultLogger, VerifiedResult};
    use std::fs::File;
    use std::io::Write;
    
    let logger = ResultLogger::new_with_key(
        env!("CARGO_PKG_VERSION").to_string(),
        results_dir.to_string_lossy().to_string(),
        private_key,
    )?;
    
    let results = logger.load_all_results()?;
    
    // Create a package with all results and a manifest
    let mut package = Vec::new();
    
    // Write manifest
    let manifest = serde_json::json!({
        "version": 1,
        "app_version": env!("CARGO_PKG_VERSION"),
        "app_hash": logger.app_hash.clone(),
        "result_count": results.len(),
        "created": chrono::Utc::now().to_rfc3339(),
    });
    
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    package.extend_from_slice(&(manifest_bytes.len() as u32).to_le_bytes());
    package.extend_from_slice(&manifest_bytes);
    
    // Write each result
    for result in results {
        let result_bytes = bincode::serialize(&result)?;
        package.extend_from_slice(&(result_bytes.len() as u32).to_le_bytes());
        package.extend_from_slice(&result_bytes);
    }
    
    // Sign the entire package
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    
    let mut mac = HmacSha256::new_from_slice(private_key)?;
    mac.update(&package);
    let signature = mac.finalize().into_bytes();
    
    // Write signature at the end
    package.extend_from_slice(&signature);
    
    // Write to file
    let mut file = File::create(output_path)?;
    file.write_all(&package)?;
    
    Ok(())
}

/// Verify a signed package
pub fn verify_signed_package(
    package_path: &Path,
    public_key: &[u8; 32],
) -> Result<PackageVerificationResult> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    
    let mut file = std::fs::File::open(package_path)?;
    let mut package = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut package)?;
    
    if package.len() < 32 {
        return Ok(PackageVerificationResult {
            valid: false,
            message: "Package too small".to_string(),
            manifest: None,
            results: Vec::new(),
        });
    }
    
    // Split package and signature
    let (package_data, signature) = package.split_at(package.len() - 32);
    
    // Verify signature
    let mut mac = HmacSha256::new_from_slice(public_key)?;
    mac.update(package_data);
    let expected_signature = mac.finalize().into_bytes();
    
    if !constant_time_eq(signature, &expected_signature) {
        return Ok(PackageVerificationResult {
            valid: false,
            message: "Package signature verification failed".to_string(),
            manifest: None,
            results: Vec::new(),
        });
    }
    
    // Parse package
    let mut offset = 0;
    
    // Read manifest
    let manifest_len = u32::from_le_bytes([
        package_data[offset], package_data[offset+1], 
        package_data[offset+2], package_data[offset+3]
    ]) as usize;
    offset += 4;
    
    let manifest_bytes = &package_data[offset..offset+manifest_len];
    offset += manifest_len;
    
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes)?;
    
    // Read results
    let mut results = Vec::new();
    while offset < package_data.len() {
        if offset + 4 > package_data.len() {
            break;
        }
        
        let result_len = u32::from_le_bytes([
            package_data[offset], package_data[offset+1], 
            package_data[offset+2], package_data[offset+3]
        ]) as usize;
        offset += 4;
        
        if offset + result_len > package_data.len() {
            break;
        }
        
        let result_bytes = &package_data[offset..offset+result_len];
        offset += result_len;
        
        let result: crate::result_logger::VerifiedResult = bincode::deserialize(result_bytes)?;
        results.push(result);
    }
    
    Ok(PackageVerificationResult {
        valid: true,
        message: "Package verified successfully".to_string(),
        manifest: Some(manifest),
        results,
    })
}

#[derive(Debug, Clone)]
pub struct PackageVerificationResult {
    pub valid: bool,
    pub message: String,
    pub manifest: Option<serde_json::Value>,
    pub results: Vec<crate::result_logger::VerifiedResult>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// ResultLogger extension for verification with custom key
impl crate::result_logger::ResultLogger {
    pub fn new_with_key(
        app_version: String,
        output_dir: String,
        private_key: &[u8; 32],
    ) -> Result<Self> {
        let app_hash = Self::compute_app_hash()?;
        
        Ok(Self {
            app_version,
            app_hash,
            private_key: *private_key,
            output_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_app_hash() {
        let hash = generate_app_hash();
        assert!(hash.is_ok());
        assert_eq!(hash.unwrap().len(), 64); // SHA256 hex = 64 chars
    }
}